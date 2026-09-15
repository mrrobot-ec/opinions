//! D24 config catalog: the COMPLETE typed key list with bounds, role, apply
//! class and max-delta — enforced 422 at write time against the whole
//! prospective snapshot — plus the D25 fence vocabulary (namespaces, canonical
//! order, request fingerprints) shared by the write paths.
//!
//! The catalog is data, not policy scattered through use cases: `SetConfig`
//! and the proposal flow both call [`validate_patch`], so a bound can never
//! be enforced on one path and forgotten on the other.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::{AppError, StoreError};
use crate::model::{AdminRole, ConfigChange, ConfigEntry, MarketId, UserId};

// ---------------------------------------------------------------------------
// Key vocabulary
// ---------------------------------------------------------------------------

/// When a committed change becomes visible to the product (D24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyClass {
    /// Stamped on the market row at go-live; never rewrites a live market.
    LiveImmutable,
    /// Read at the authoritative fence point of the next trade (D25a).
    NextTrade,
    /// New markets only (fee, seeds); a live book NEVER reprices.
    NewMarketOnly,
    /// Pauses and flags: visible to the next fence-point read, globally.
    ImmediateGlobal,
}

/// Who may write a key, and through which flow (D24/D26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteRule {
    /// Applies directly through `SetConfig` when the actor holds the role.
    Direct(AdminRole),
    /// Sensitive: ONLY the two-phase proposal flow, proposed by the listed
    /// role (or superadmin — D24 write path), confirmed by a DISTINCT token.
    TwoPhase(AdminRole),
}

/// One catalog row. `pattern` keys end in `:` and match `pattern{uuid}`.
pub struct CatalogRule {
    pub key: &'static str,
    pub pattern: bool,
    pub write: WriteRule,
    pub class: ApplyClass,
    check: fn(&str, &Value, &Snapshot) -> Result<(), &'static str>,
}

/// The committed (or prospective) snapshot the validator reads.
pub type Snapshot = BTreeMap<String, Value>;

/// Builds a snapshot map from committed entries.
#[must_use]
pub fn snapshot_of(entries: &[ConfigEntry]) -> Snapshot {
    entries
        .iter()
        .map(|e| (e.key.clone(), e.value.clone()))
        .collect()
}

fn as_i64(v: &Value) -> Result<i64, &'static str> {
    v.as_i64().ok_or("expected an integer")
}

fn as_bool(v: &Value) -> Result<bool, &'static str> {
    v.as_bool().ok_or("expected a boolean")
}

fn as_i64_array<const N: usize>(v: &Value) -> Result<[i64; N], &'static str> {
    let arr = v.as_array().ok_or("expected an array")?;
    if arr.len() != N {
        return Err("wrong array length");
    }
    let mut out = [0i64; N];
    for (i, item) in arr.iter().enumerate() {
        out[i] = item.as_i64().ok_or("expected integer entries")?;
    }
    Ok(out)
}

fn bounded(v: i64, lo: i64, hi: i64) -> Result<(), &'static str> {
    if v < lo || v > hi {
        return Err("out of bounds");
    }
    Ok(())
}

fn prior_i64(snap: &Snapshot, key: &str) -> Option<i64> {
    snap.get(key).and_then(Value::as_i64)
}

/// |new − prior| ≤ delta (enforced per apply; the catalog's `/5min` windows
/// collapse to per-apply deltas because change rows carry no timestamps
/// across the port — every apply is separately audited).
fn max_delta(prior: Option<i64>, new: i64, delta: i64) -> Result<(), &'static str> {
    match prior {
        Some(p) if (new - p).abs() > delta => Err("delta exceeds the per-apply limit"),
        _ => Ok(()),
    }
}

/// new ≤ prior × 2 (and new ≥ prior / 2 for symmetric ≤2× rules).
fn within_2x(prior: Option<i64>, new: i64) -> Result<(), &'static str> {
    match prior {
        Some(p) if p > 0 && (new > p.saturating_mul(2) || new < p / 2) => {
            Err("change exceeds 2x of the prior value")
        }
        _ => Ok(()),
    }
}

fn monotone_non_decreasing<const N: usize>(arr: &[i64; N]) -> Result<(), &'static str> {
    if arr.windows(2).any(|w| w[1] < w[0]) {
        return Err("entries must be monotone non-decreasing");
    }
    Ok(())
}

/// Published tier caps (migration-0008 seed): live floors for cap raises.
pub const PUBLISHED_POSITION_CAPS: [i64; 5] = [
    25_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
];
/// Published integrity ppm defaults (0008 seeds) — the ±20% bands anchor
/// here, NOT at the current value, so drift cannot compound (grok r3 NEW-1).
const INTEGRITY_DEFAULTS: [(&str, i64); 5] = [
    ("integrity_burst_multiplier_ppm", 2_000_000),
    ("integrity_young_share_max_ppm", 500_000),
    ("integrity_device_share_max_ppm", 600_000),
    ("integrity_subnet_share_max_ppm", 600_000),
    ("integrity_min_metadata_coverage_ppm", 500_000),
];
/// Published vote-velocity seeds; bounds are ±50% of these.
const PUBLISHED_MAX_VOTES: i64 = 30;
const PUBLISHED_VOTE_WINDOW: i64 = 3_600;
/// Published rep tier thresholds (0008 seed).
const PUBLISHED_MIN_VOTES_FLOOR: i64 = 3;

fn check_trade_fee(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 10, 200)?;
    max_delta(prior_i64(snap, "trade_fee_bps"), n, 20)
}

fn check_min_fee(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 0, 50)?;
    max_delta(prior_i64(snap, "min_fee_bps"), n, 10)
}

fn check_flip_window(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 600, 86_400)
}

/// Discount vector (codex r4 NEW-1; grok r3 N9): each entry ∈
/// [0, prospective `trade_fee_bps`], monotone non-decreasing, Δ ≤ 10bp per
/// entry per apply. The `min_fee_bps` floor applies to the COMPUTED effective
/// fee (`max(base − discount, min_fee)`), never to the entries — the
/// published `[0, 0, 10, 20, 30]` seed must validate.
fn check_discounts(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let arr = as_i64_array::<5>(v)?;
    let base = prior_i64(snap, "trade_fee_bps").unwrap_or(100);
    for entry in arr {
        bounded(entry, 0, base)?;
    }
    monotone_non_decreasing(&arr)?;
    if let Some(prior) = snap
        .get("fee_discount_bp_by_tier")
        .and_then(|p| as_i64_array::<5>(p).ok())
    {
        for i in 0..5 {
            max_delta(Some(prior[i]), arr[i], 10)?;
        }
    }
    Ok(())
}

fn check_position_caps(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    let arr = as_i64_array::<5>(v)?;
    for i in 0..5 {
        if arr[i] < PUBLISHED_POSITION_CAPS[i] {
            return Err("cap below the published tier table");
        }
    }
    monotone_non_decreasing(&arr)
}

/// Rep tier thresholds: monotone, Δ ≤ 10% per proposal per entry.
fn check_tier_thresholds(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let arr = as_i64_array::<5>(v)?;
    monotone_non_decreasing(&arr)?;
    if let Some(prior) = snap
        .get("rep_tier_thresholds_micro")
        .and_then(|p| as_i64_array::<5>(p).ok())
    {
        for i in 0..5 {
            let band = prior[i] / 10;
            if (arr[i] - prior[i]).abs() > band {
                return Err("threshold moves more than 10% in one proposal");
            }
        }
    }
    Ok(())
}

fn check_min_pot(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 0, 1_000_000_000)?;
    within_2x(prior_i64(snap, "rep_score_min_pot_micro"), n)
}

fn check_seed(key: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    if n <= 0 {
        return Err("seed must be positive");
    }
    within_2x(prior_i64(snap, key), n)
}

fn check_seed_budget(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 100_000_000_000)?;
    within_2x(prior_i64(snap, "daily_seed_budget_micro"), n)
}

fn check_hidden_window(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 60, 900)
}

fn check_min_votes_floor(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    if n < PUBLISHED_MIN_VOTES_FLOOR {
        return Err("below the published floor");
    }
    Ok(())
}

fn check_oi_floor(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 0, 1_000_000_000)
}

fn check_hold_threshold(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 10_000_000_000)?;
    within_2x(prior_i64(snap, "payout_hold_threshold_micro"), n)
}

fn check_sweep_delay(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 60, 3_600)?;
    within_2x(prior_i64(snap, "sweep_delay_secs"), n)
}

fn check_max_votes(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(
        as_i64(v)?,
        PUBLISHED_MAX_VOTES / 2,
        PUBLISHED_MAX_VOTES * 3 / 2,
    )
}

fn check_vote_window(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(
        as_i64(v)?,
        PUBLISHED_VOTE_WINDOW / 2,
        PUBLISHED_VOTE_WINDOW * 3 / 2,
    )
}

/// Integrity ppm knobs: ±20% of the SHIPPED defaults (never the current
/// value), so a red swarm run cannot be "fixed" by threshold surgery.
fn check_integrity_ppm(key: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    let Some((_, published)) = INTEGRITY_DEFAULTS.iter().find(|(k, _)| *k == key) else {
        return Err("unknown integrity knob");
    };
    bounded(n, published * 4 / 5, published * 6 / 5)
}

fn check_cadence(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 900, 86_400)
}

fn check_daily_slots(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1, 4)?;
    max_delta(prior_i64(snap, "daily_slots"), n, 1)
}

fn check_bool(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    as_bool(v).map(|_| ())
}

fn check_faucet_cap(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 0, 1_000_000_000)?;
    within_2x(prior_i64(snap, "faucet_per_call_cap_micro"), n)
}

fn check_remedial_market_cap(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 0, 500_000_000)
}

fn check_remedial_daily_cap(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 0, 2_000_000_000)
}

fn check_receivable_cap(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 0, 1_000_000_000_000)
}

fn check_writeoff_item_cap(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 0, 500_000_000)
}

fn check_writeoff_daily_cap(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 0, 2_000_000_000)
}

fn check_proposal_ttl(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 60, 3_600)
}

fn check_dual_delay(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    bounded(as_i64(v)?, 0, 86_400)
}

fn mul_delta(prior: Option<i64>, new: i64, factor: i64, seed: i64) -> Result<(), &'static str> {
    match prior {
        Some(0) | None if new == 0 => Ok(()),
        Some(0) => {
            if new > seed {
                Err("re-enable exceeds the seed baseline")
            } else {
                Ok(())
            }
        }
        Some(_) if new == 0 => Ok(()),
        Some(p)
            if p > 0 && (new > p.saturating_mul(factor) || (factor > 1 && new < p / factor)) =>
        {
            Err("change exceeds the multiplicative per-apply limit")
        }
        _ => Ok(()),
    }
}

fn check_kyc_tier(k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 0, 2)?;
    mul_delta(
        prior_i64(snap, k),
        n,
        1,
        if k.contains("deposit") { 1 } else { 2 },
    )
}

fn check_withdraw_min(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000)?;
    mul_delta(prior_i64(snap, "withdraw_min_micro"), n, 10, 5_000_000)
}

fn check_withdraw_max(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000_000)?;
    mul_delta(prior_i64(snap, "withdraw_max_micro"), n, 10, 1_000_000_000)
}

fn check_withdraw_daily(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000_000)?;
    mul_delta(
        prior_i64(snap, "withdraw_daily_limit_micro"),
        n,
        10,
        2_000_000_000,
    )
}

fn check_auto_approve(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 0, 1_000_000_000)?;
    mul_delta(
        prior_i64(snap, "withdraw_auto_approve_micro"),
        n,
        10,
        50_000_000,
    )
}

fn check_dual_control(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000_000)?;
    mul_delta(
        prior_i64(snap, "withdraw_dual_control_micro"),
        n,
        10,
        500_000_000,
    )
}

fn check_warm_floor(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000)?;
    mul_delta(prior_i64(snap, "dest_warm_floor_micro"), n, 10, 100_000_000)
}

fn check_warm_age(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1, 720)?;
    mul_delta(prior_i64(snap, "dest_warm_age_hours"), n, 4, 72)
}

fn check_dest_daily(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000_000)?;
    mul_delta(
        prior_i64(snap, "dest_daily_limit_micro"),
        n,
        10,
        1_000_000_000,
    )
}

fn check_hot_wallet(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 10_000_000_000_000)?;
    mul_delta(
        prior_i64(snap, "hot_wallet_daily_limit_micro"),
        n,
        4,
        10_000_000_000,
    )
}

fn check_confirmations(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1, 300)?;
    mul_delta(prior_i64(snap, "deposit_confirmations"), n, 4, 32)
}

fn check_credit_grant(k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 0, 100_000_000)?;
    mul_delta(prior_i64(snap, k), n, 4, 5_000_000)
}

fn check_referral_min(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000)?;
    mul_delta(
        prior_i64(snap, "referral_min_notional_micro"),
        n,
        10,
        10_000_000,
    )
}

fn check_bonus_mint(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 0, 10_000_000_000)?;
    mul_delta(
        prior_i64(snap, "bonus_mint_daily_cap_micro"),
        n,
        4,
        500_000_000,
    )
}

fn check_bonus_structure(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    match v.as_str() {
        Some("real_money" | "sweeps") => Ok(()),
        _ => Err("bonus_structure must be real_money or sweeps"),
    }
}

fn check_aml_velocity(k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000_000)?;
    mul_delta(prior_i64(snap, k), n, 4, 5_000_000_000)
}

fn check_structuring_n(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 2, 100)?;
    mul_delta(prior_i64(snap, "aml_structuring_n"), n, 4, 4)
}

fn check_structuring_window(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1, 168)?;
    mul_delta(prior_i64(snap, "aml_structuring_window_hours"), n, 4, 24)
}

fn check_structuring_threshold(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 100_000_000, 10_000_000_000)?;
    mul_delta(
        prior_i64(snap, "aml_structuring_threshold_micro"),
        n,
        4,
        500_000_000,
    )
}

fn check_structuring_floor(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 10_000_000, 1_000_000_000)?;
    mul_delta(
        prior_i64(snap, "aml_structuring_floor_micro"),
        n,
        4,
        100_000_000,
    )
}

fn check_shadow_cap(k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 0, 1_000_000_000)?;
    mul_delta(prior_i64(snap, k), n, 10, 25_000_000)
}

fn check_stuck_sla(k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 60, 86_400)?;
    mul_delta(prior_i64(snap, k), n, 4, 900)
}

fn check_region_allowset(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    let arr = v.as_array().ok_or("expected an array")?;
    if arr.is_empty() {
        return Err("region_allowset must be non-empty when present");
    }
    if arr.iter().all(|item| item.as_str().is_some()) {
        Ok(())
    } else {
        Err("region_allowset entries must be strings")
    }
}

fn check_region_version(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    if n < 1 {
        return Err("region_allowset_version must be >= 1");
    }
    if let Some(p) = prior_i64(snap, "region_allowset_version") {
        if n != p && n != p + 1 {
            return Err("region_allowset_version must increase by 1");
        }
    }
    Ok(())
}

fn check_approve_daily(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 10_000_000_000_000)?;
    mul_delta(
        prior_i64(snap, "withdraw_approve_daily_cap_micro"),
        n,
        4,
        5_000_000_000,
    )
}

fn check_reserve_topup(_k: &str, v: &Value, snap: &Snapshot) -> Result<(), &'static str> {
    let n = as_i64(v)?;
    bounded(n, 1_000_000, 1_000_000_000_000)?;
    mul_delta(
        prior_i64(snap, "bonus_reserve_topup_daily_cap_micro"),
        n,
        4,
        1_000_000_000,
    )
}

fn check_fee_override(_k: &str, v: &Value, _s: &Snapshot) -> Result<(), &'static str> {
    if v.as_str() == Some("inherit") {
        return Ok(());
    }
    if let Some(obj) = v.as_object() {
        if obj.len() == 1 {
            if let Some(bps) = obj.get("override").and_then(Value::as_i64) {
                return bounded(bps, 10, 200);
            }
        }
    }
    Err("fee override must be inherit or override(bps)")
}

fn check_p7_cross_keys(snap: &Snapshot) -> Result<(), &'static str> {
    let get = |k: &str| prior_i64(snap, k);
    if let (
        Some(min),
        Some(auto),
        Some(floor),
        Some(dual),
        Some(max),
        Some(dest),
        Some(daily),
        Some(hot),
    ) = (
        get("withdraw_min_micro"),
        get("withdraw_auto_approve_micro"),
        get("dest_warm_floor_micro"),
        get("withdraw_dual_control_micro"),
        get("withdraw_max_micro"),
        get("dest_daily_limit_micro"),
        get("withdraw_daily_limit_micro"),
        get("hot_wallet_daily_limit_micro"),
    ) {
        let auto_disabled = auto == 0;
        if auto_disabled {
            if !(min <= dual && dual <= max && max <= dest && dest <= daily && daily <= hot) {
                return Err("dual <= max <= dest_daily <= daily <= hot_wallet");
            }
        } else if !(min <= auto && auto < floor) {
            return Err("min <= auto < dest_warm_floor");
        } else if !(auto <= dual && dual <= max && max <= dest && dest <= daily && daily <= hot) {
            return Err("auto <= dual <= max <= dest_daily <= daily <= hot_wallet");
        }
    }
    if let (Some(floor), Some(threshold)) = (
        get("aml_structuring_floor_micro"),
        get("aml_structuring_threshold_micro"),
    ) {
        if !(0 < floor && floor < threshold) {
            return Err("0 < aml_structuring_floor < threshold");
        }
    }
    Ok(())
}

/// Published Phase-7 money-catalog seed (0011). `region_allowset` is absent.
#[must_use]
pub fn phase7_seed_snapshot() -> Snapshot {
    let mut snap = Snapshot::new();
    for (k, v) in [
        ("deposit_kyc_tier", json!(1)),
        ("withdraw_kyc_tier", json!(2)),
        ("withdraw_min_micro", json!(5_000_000)),
        ("withdraw_max_micro", json!(1_000_000_000i64)),
        ("withdraw_daily_limit_micro", json!(2_000_000_000i64)),
        ("withdraw_auto_approve_micro", json!(50_000_000)),
        ("withdraw_dual_control_micro", json!(500_000_000)),
        ("dest_warm_floor_micro", json!(100_000_000)),
        ("dest_warm_age_hours", json!(72)),
        ("dest_daily_limit_micro", json!(1_000_000_000i64)),
        ("hot_wallet_daily_limit_micro", json!(10_000_000_000i64)),
        ("deposit_confirmations", json!(32)),
        ("pause_deposits", json!(false)),
        ("pause_withdrawals", json!(false)),
        ("credit_signup_micro", json!(5_000_000)),
        ("credit_referral_referrer_micro", json!(5_000_000)),
        ("credit_referral_referee_micro", json!(5_000_000)),
        ("referral_min_notional_micro", json!(10_000_000)),
        ("bonus_mint_daily_cap_micro", json!(500_000_000)),
        ("bonus_structure", json!("real_money")),
        ("aml_deposit_velocity_micro_24h", json!(5_000_000_000i64)),
        ("aml_withdraw_velocity_micro_24h", json!(5_000_000_000i64)),
        ("aml_structuring_n", json!(4)),
        ("aml_structuring_window_hours", json!(24)),
        ("aml_structuring_threshold_micro", json!(500_000_000)),
        ("aml_structuring_floor_micro", json!(100_000_000)),
        ("shadow_trade_cap_micro", json!(25_000_000)),
        ("shadow_deposit_cap_micro", json!(25_000_000)),
        ("stuck_send_sla_secs", json!(900)),
        ("stuck_screening_sla_secs", json!(3600)),
        ("region_allowset_version", json!(1)),
        ("withdraw_approve_daily_cap_micro", json!(5_000_000_000i64)),
        (
            "bonus_reserve_topup_daily_cap_micro",
            json!(1_000_000_000i64),
        ),
    ] {
        snap.insert(k.into(), v);
    }
    snap
}

use AdminRole::{Curator, Finance, Ops, Superadmin};
use ApplyClass::{ImmediateGlobal, LiveImmutable, NewMarketOnly, NextTrade};
use WriteRule::{Direct, TwoPhase};

/// THE complete catalog (D24). Every key carries all four attributes.
pub const CATALOG: &[CatalogRule] = &[
    CatalogRule {
        key: "trade_fee_bps",
        pattern: false,
        write: TwoPhase(Finance),
        class: NewMarketOnly,
        check: check_trade_fee,
    },
    CatalogRule {
        key: "min_fee_bps",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_min_fee,
    },
    CatalogRule {
        key: "discount_flip_window_secs",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_flip_window,
    },
    CatalogRule {
        key: "fee_discount_bp_by_tier",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_discounts,
    },
    CatalogRule {
        key: "position_cap_micro_by_tier",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_position_caps,
    },
    CatalogRule {
        key: "rep_tier_thresholds_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_tier_thresholds,
    },
    CatalogRule {
        key: "rep_score_min_pot_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_min_pot,
    },
    CatalogRule {
        key: "seed_micro_daily",
        pattern: false,
        write: TwoPhase(Finance),
        class: NewMarketOnly,
        check: check_seed,
    },
    CatalogRule {
        key: "seed_micro_flash",
        pattern: false,
        write: TwoPhase(Finance),
        class: NewMarketOnly,
        check: check_seed,
    },
    CatalogRule {
        key: "daily_seed_budget_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NewMarketOnly,
        check: check_seed_budget,
    },
    CatalogRule {
        key: "hidden_window_secs",
        pattern: false,
        write: TwoPhase(Finance),
        class: LiveImmutable,
        check: check_hidden_window,
    },
    CatalogRule {
        key: "min_votes_to_resolve_floor",
        pattern: false,
        write: TwoPhase(Finance),
        class: LiveImmutable,
        check: check_min_votes_floor,
    },
    CatalogRule {
        key: "oi_floor_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: LiveImmutable,
        check: check_oi_floor,
    },
    CatalogRule {
        key: "payout_hold_threshold_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: LiveImmutable,
        check: check_hold_threshold,
    },
    CatalogRule {
        key: "sweep_delay_secs",
        pattern: false,
        write: Direct(Ops),
        class: ImmediateGlobal,
        check: check_sweep_delay,
    },
    CatalogRule {
        key: "max_votes_per_window",
        pattern: false,
        write: TwoPhase(Ops),
        class: ImmediateGlobal,
        check: check_max_votes,
    },
    CatalogRule {
        key: "vote_window_secs",
        pattern: false,
        write: TwoPhase(Ops),
        class: ImmediateGlobal,
        check: check_vote_window,
    },
    CatalogRule {
        key: "integrity_burst_multiplier_ppm",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: ImmediateGlobal,
        check: check_integrity_ppm,
    },
    CatalogRule {
        key: "integrity_young_share_max_ppm",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: ImmediateGlobal,
        check: check_integrity_ppm,
    },
    CatalogRule {
        key: "integrity_device_share_max_ppm",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: ImmediateGlobal,
        check: check_integrity_ppm,
    },
    CatalogRule {
        key: "integrity_subnet_share_max_ppm",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: ImmediateGlobal,
        check: check_integrity_ppm,
    },
    CatalogRule {
        key: "integrity_min_metadata_coverage_ppm",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: ImmediateGlobal,
        check: check_integrity_ppm,
    },
    CatalogRule {
        key: "flash_cadence_secs",
        pattern: false,
        write: Direct(Curator),
        class: ImmediateGlobal,
        check: check_cadence,
    },
    CatalogRule {
        key: "daily_slots",
        pattern: false,
        write: Direct(Curator),
        class: ImmediateGlobal,
        check: check_daily_slots,
    },
    CatalogRule {
        key: "feature_flash_markets",
        pattern: false,
        write: Direct(Ops),
        class: ImmediateGlobal,
        check: check_bool,
    },
    CatalogRule {
        key: "feature_comments",
        pattern: false,
        write: Direct(Ops),
        class: ImmediateGlobal,
        check: check_bool,
    },
    CatalogRule {
        key: "feature_referrals",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_bool,
    },
    CatalogRule {
        key: "trading_paused",
        pattern: false,
        write: Direct(Ops),
        class: ImmediateGlobal,
        check: check_bool,
    },
    CatalogRule {
        key: "market_paused:",
        pattern: true,
        write: Direct(Ops),
        class: ImmediateGlobal,
        check: check_bool,
    },
    CatalogRule {
        key: "voting_paused:",
        pattern: true,
        write: TwoPhase(Superadmin),
        class: ImmediateGlobal,
        check: check_bool,
    },
    CatalogRule {
        key: "faucet_per_call_cap_micro",
        pattern: false,
        write: Direct(Finance),
        class: ImmediateGlobal,
        check: check_faucet_cap,
    },
    CatalogRule {
        key: "remedial_credit_market_cap_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_remedial_market_cap,
    },
    CatalogRule {
        key: "remedial_credit_daily_cap_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_remedial_daily_cap,
    },
    CatalogRule {
        key: "receivable_outstanding_cap_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_receivable_cap,
    },
    CatalogRule {
        key: "writeoff_per_item_cap_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_writeoff_item_cap,
    },
    CatalogRule {
        key: "writeoff_daily_cap_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_writeoff_daily_cap,
    },
    CatalogRule {
        key: "proposal_ttl_secs",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: ImmediateGlobal,
        check: check_proposal_ttl,
    },
    CatalogRule {
        key: "dual_control_delay_secs",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: ImmediateGlobal,
        check: check_dual_delay,
    },
    CatalogRule {
        key: "deposit_kyc_tier",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_kyc_tier,
    },
    CatalogRule {
        key: "withdraw_kyc_tier",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_kyc_tier,
    },
    CatalogRule {
        key: "withdraw_min_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_withdraw_min,
    },
    CatalogRule {
        key: "withdraw_max_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_withdraw_max,
    },
    CatalogRule {
        key: "withdraw_daily_limit_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_withdraw_daily,
    },
    CatalogRule {
        key: "withdraw_auto_approve_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_auto_approve,
    },
    CatalogRule {
        key: "withdraw_dual_control_micro",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: NextTrade,
        check: check_dual_control,
    },
    CatalogRule {
        key: "dest_warm_floor_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_warm_floor,
    },
    CatalogRule {
        key: "dest_warm_age_hours",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_warm_age,
    },
    CatalogRule {
        key: "dest_daily_limit_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_dest_daily,
    },
    CatalogRule {
        key: "hot_wallet_daily_limit_micro",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: NextTrade,
        check: check_hot_wallet,
    },
    CatalogRule {
        key: "deposit_confirmations",
        pattern: false,
        write: TwoPhase(Ops),
        class: ImmediateGlobal,
        check: check_confirmations,
    },
    CatalogRule {
        key: "pause_deposits",
        pattern: false,
        write: Direct(Ops),
        class: ImmediateGlobal,
        check: check_bool,
    },
    CatalogRule {
        key: "pause_withdrawals",
        pattern: false,
        write: Direct(Ops),
        class: ImmediateGlobal,
        check: check_bool,
    },
    CatalogRule {
        key: "credit_signup_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_credit_grant,
    },
    CatalogRule {
        key: "credit_referral_referrer_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_credit_grant,
    },
    CatalogRule {
        key: "credit_referral_referee_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_credit_grant,
    },
    CatalogRule {
        key: "referral_min_notional_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_referral_min,
    },
    CatalogRule {
        key: "bonus_mint_daily_cap_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_bonus_mint,
    },
    CatalogRule {
        key: "bonus_structure",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: NextTrade,
        check: check_bonus_structure,
    },
    CatalogRule {
        key: "aml_deposit_velocity_micro_24h",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_aml_velocity,
    },
    CatalogRule {
        key: "aml_withdraw_velocity_micro_24h",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_aml_velocity,
    },
    CatalogRule {
        key: "aml_structuring_n",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_structuring_n,
    },
    CatalogRule {
        key: "aml_structuring_window_hours",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_structuring_window,
    },
    CatalogRule {
        key: "aml_structuring_threshold_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_structuring_threshold,
    },
    CatalogRule {
        key: "aml_structuring_floor_micro",
        pattern: false,
        write: TwoPhase(Finance),
        class: ImmediateGlobal,
        check: check_structuring_floor,
    },
    CatalogRule {
        key: "shadow_trade_cap_micro",
        pattern: false,
        write: Direct(Ops),
        class: NextTrade,
        check: check_shadow_cap,
    },
    CatalogRule {
        key: "shadow_deposit_cap_micro",
        pattern: false,
        write: Direct(Ops),
        class: NextTrade,
        check: check_shadow_cap,
    },
    CatalogRule {
        key: "stuck_send_sla_secs",
        pattern: false,
        write: TwoPhase(Ops),
        class: ImmediateGlobal,
        check: check_stuck_sla,
    },
    CatalogRule {
        key: "stuck_screening_sla_secs",
        pattern: false,
        write: TwoPhase(Ops),
        class: ImmediateGlobal,
        check: check_stuck_sla,
    },
    CatalogRule {
        key: "region_allowset",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: NextTrade,
        check: check_region_allowset,
    },
    CatalogRule {
        key: "region_allowset_version",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: NextTrade,
        check: check_region_version,
    },
    CatalogRule {
        key: "withdraw_approve_daily_cap_micro",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: NextTrade,
        check: check_approve_daily,
    },
    CatalogRule {
        key: "bonus_reserve_topup_daily_cap_micro",
        pattern: false,
        write: TwoPhase(Superadmin),
        class: NextTrade,
        check: check_reserve_topup,
    },
    CatalogRule {
        key: "fee_bps_override:",
        pattern: true,
        write: TwoPhase(Finance),
        class: NextTrade,
        check: check_fee_override,
    },
];

/// Looks up the catalog rule for a concrete key (pattern keys match by
/// prefix and require a UUID suffix).
#[must_use]
pub fn rule_for(key: &str) -> Option<&'static CatalogRule> {
    CATALOG.iter().find(|rule| {
        if rule.pattern {
            key.strip_prefix(rule.key)
                .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
        } else {
            rule.key == key
        }
    })
}

/// True when the key requires the two-phase proposal flow.
#[must_use]
pub fn is_sensitive(key: &str) -> bool {
    matches!(rule_for(key).map(|r| &r.write), Some(TwoPhase(_)))
}

/// May `role` write `key` through the given flow? Superadmin is additionally
/// accepted as a proposer/confirmer for two-phase keys (D24 write path);
/// direct keys stay strictly with their listed role — roles are sets, not a
/// hierarchy (D26).
#[must_use]
pub fn role_may_write(role: AdminRole, key: &str) -> bool {
    match rule_for(key).map(|r| r.write) {
        Some(Direct(listed)) => role == listed,
        Some(TwoPhase(listed)) => role == listed || role == AdminRole::Superadmin,
        None => false,
    }
}

/// Validates a patch against the prospective whole snapshot (D24 write
/// path): unknown keys, type errors, bounds, monotonicity and per-apply
/// max-delta are all typed 422s. Returns the per-key change rows (no-op
/// values are dropped — a change row records a CHANGED key).
///
/// # Errors
/// [`AppError::ConfigInvalid`] naming the offending key.
pub fn validate_patch(current: &Snapshot, patch: &Value) -> Result<Vec<ConfigChange>, AppError> {
    let object = patch.as_object().ok_or(AppError::ConfigInvalid {
        key: "patch".to_string(),
        reason: "patch must be a JSON object",
    })?;
    if object.is_empty() {
        return Err(AppError::ConfigInvalid {
            key: "patch".to_string(),
            reason: "patch must not be empty",
        });
    }
    // The prospective snapshot: current ⊕ patch. Cross-key checks (e.g. the
    // discount vector against `trade_fee_bps`) read THIS, so a patch that
    // moves both keys is validated as it will land.
    let mut prospective = current.clone();
    for (key, value) in object {
        prospective.insert(key.clone(), value.clone());
    }
    let mut changes = Vec::new();
    for (key, value) in object {
        let rule = rule_for(key).ok_or_else(|| AppError::ConfigInvalid {
            key: key.clone(),
            reason: "unknown config key",
        })?;
        // Delta rules compare against the CURRENT committed value, while
        // bounds/cross-key rules read the prospective snapshot; pass current
        // values through the prospective map minus this key's own new value.
        let mut delta_view = prospective.clone();
        if let Some(prior) = current.get(key) {
            delta_view.insert(key.clone(), prior.clone());
        } else {
            delta_view.remove(key);
        }
        (rule.check)(key, value, &delta_view).map_err(|reason| AppError::ConfigInvalid {
            key: key.clone(),
            reason,
        })?;
        let old = current.get(key).cloned();
        if old.as_ref() != Some(value) {
            changes.push(ConfigChange {
                key: key.clone(),
                old,
                new: value.clone(),
            });
        }
    }
    check_p7_cross_keys(&prospective).map_err(|reason| AppError::ConfigInvalid {
        key: "cross_key".to_string(),
        reason,
    })?;
    Ok(changes)
}

// ---------------------------------------------------------------------------
// Fences (D25) — namespaces, canonical order
// ---------------------------------------------------------------------------

/// Advisory-lock class for the three fence namespaces (classes 1–3 are the
/// idempotency, user and LP-kill locks).
pub const FENCE_CLASS: i32 = 4;

#[must_use]
pub fn trading_global_fence() -> String {
    "trading-global".to_string()
}

#[must_use]
pub fn trading_market_fence(market: MarketId) -> String {
    format!("trading-market:{}", market.0)
}

#[must_use]
pub fn voting_market_fence(market: MarketId) -> String {
    format!("voting-market:{}", market.0)
}

/// The exclusive fences a config patch must hold, in CANONICAL order:
/// `trading-global` first, then market-scoped fences sorted by name. Pause
/// writers take only (proposal row → these fences → generation row → config
/// rows) and never any downstream trade/vote lock (one-way lock graph,
/// codex r2 NEW-1).
#[must_use]
pub fn exclusive_fences_for_patch(keys: &[String]) -> Vec<String> {
    let mut scoped: Vec<String> = Vec::new();
    let mut global = false;
    for key in keys {
        if key == "trading_paused" {
            global = true;
        } else if let Some(id) = key.strip_prefix("market_paused:") {
            scoped.push(format!("trading-market:{id}"));
        } else if let Some(id) = key.strip_prefix("voting_paused:") {
            scoped.push(format!("voting-market:{id}"));
        }
    }
    scoped.sort();
    scoped.dedup();
    let mut out = Vec::new();
    if global {
        out.push(trading_global_fence());
    }
    out.extend(scoped);
    out
}

/// Reads a pause flag out of a config value: absent or non-`true` is unpaused.
#[must_use]
pub fn pause_in_force(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Canonical JSON + hashes + request fingerprints (D25 replay precedence)
// ---------------------------------------------------------------------------

/// Canonical JSON: objects sorted by key at every depth, no whitespace.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    fn canon(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: BTreeMap<_, _> =
                    map.iter().map(|(k, v)| (k.clone(), canon(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(canon).collect()),
            other => other.clone(),
        }
    }
    canon(value).to_string()
}

/// SHA-256 hex of the canonical JSON encoding — the proposal `patch_hash`.
#[must_use]
pub fn patch_hash(patch: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(patch).as_bytes());
    hex_of(&hasher.finalize())
}

fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Canonical `PlaceTrade` request fingerprint (codex r3 NEW-3, pinned):
/// market/user/side/action/amount/`expected_config_version`.
#[must_use]
pub fn trade_fingerprint(
    market: MarketId,
    user: UserId,
    side: domain::amm::Side,
    action: crate::model::TradeAction,
    amount_micro: i64,
    expected_config_version: Option<i64>,
) -> String {
    let version = expected_config_version.map_or_else(|| "-".to_string(), |v| v.to_string());
    format!(
        "trade:{}:{}:{side:?}:{action:?}:{amount_micro}:{version}",
        market.0, user.0
    )
    .to_ascii_lowercase()
}

/// Canonical `CastVote` request fingerprint: market/user/side/guess.
#[must_use]
pub fn vote_fingerprint(
    market: MarketId,
    user: UserId,
    side: domain::amm::Side,
    crowd_guess_pct: u8,
) -> String {
    format!("vote:{}:{}:{side:?}:{crowd_guess_pct}", market.0, user.0).to_ascii_lowercase()
}

/// Validates a persisted request fingerprint while preserving compatibility
/// with pre-fingerprint idempotency rows.
///
/// # Errors
/// [`AppError::IdempotencyConflict`] when a persisted fingerprint differs.
pub fn validate_replay_fingerprint(stored: Option<&str>, expected: &str) -> Result<(), AppError> {
    if stored.is_some_and(|stored| stored != expected) {
        return Err(AppError::IdempotencyConflict);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// D25 fence-point role (composed into TradeTx/VoteTx by ports/market.rs)
// ---------------------------------------------------------------------------

/// Fence-point reads and writes shared by `PlaceTrade` and `CastVote` (D25):
/// shared class-4 fences acquired AFTER the row locks, authoritative pause
/// and staleness reads under those fences, and the persisted request
/// fingerprint that gives idempotent hits precedence over every other check.
#[async_trait]
pub trait FencePoint: Send {
    /// Acquires shared class-4 fences in the given (canonical) order. Blocks
    /// only while a pause writer holds the exclusive side.
    async fn acquire_shared_fences(&mut self, namespaces: &[String]) -> Result<(), StoreError>;
    /// Authoritative in-transaction config read (fence point only — the lock
    /// -free read path is [`crate::ports::ConfigReads`]).
    async fn fence_config_value(&mut self, key: &str) -> Result<Option<Value>, StoreError>;
    /// Returns `(current_generation, changes)` where `changes` is every
    /// change row STRICTLY after `generation` (oldest first), or `None` when
    /// `generation` predates the retention watermark — the caller must then
    /// conservatively treat the preview as stale (D24 bounded history).
    async fn fence_changes_since(
        &mut self,
        generation: i64,
    ) -> Result<(i64, Option<Vec<ConfigChange>>), StoreError>;
    /// The persisted canonical fingerprint for an idempotency key, if any.
    async fn request_fingerprint(&mut self, key: &str) -> Result<Option<String>, StoreError>;
    /// Persists the fingerprint for a fresh key in the SAME transaction as
    /// the write it describes.
    async fn save_request_fingerprint(
        &mut self,
        key: &str,
        fingerprint: &str,
    ) -> Result<(), StoreError>;
}

// ---------------------------------------------------------------------------
// D24 write-side support (composed into `OpsConfigTx` by ports/ops.rs)
// ---------------------------------------------------------------------------

/// Write-side fence and context reads for the config transaction (D24/D25):
/// exclusive class-4 fences (the pause writer's side), lock-free market
/// timing reads for the pause rules, and the proposal lookups the two-phase
/// flow needs beyond the frozen `OpsConfigTx` surface.
#[async_trait]
pub trait OpsWriteSupport: Send {
    /// Acquires EXCLUSIVE class-4 fences in canonical order. Waits only on
    /// currently-writing holders — pause writers never take any downstream
    /// trade/vote lock (one-way lock graph, codex r2 NEW-1).
    async fn acquire_exclusive_fences(&mut self, namespaces: &[String]) -> Result<(), StoreError>;
    /// Lock-free market timing read (state, `tally_hidden_at`, `closes_at`)
    /// for the pause-write rules. Deliberately NOT `market_for_update`: a
    /// row lock here would close the lock graph into a deadlock cycle.
    async fn market_times(
        &mut self,
        market: MarketId,
    ) -> Result<Option<crate::model::MarketRow>, StoreError>;
    /// Proposal row by idempotency key (idempotent create returns the
    /// existing row when the patch hash matches).
    async fn proposal_by_idempotency_key(
        &mut self,
        key: &str,
    ) -> Result<Option<crate::model::ConfigProposal>, StoreError>;
    /// Every change row of ONE generation (the emergency-revert pre-fill).
    async fn changes_in_generation(
        &mut self,
        generation: i64,
    ) -> Result<Vec<ConfigChange>, StoreError>;
}

// ---------------------------------------------------------------------------
// D25a staleness: the market/user-relevant change set
// ---------------------------------------------------------------------------

/// What the fence point knows about THIS request when deciding relevance.
#[derive(Debug, Clone, Copy)]
pub struct StalenessContext {
    pub user_tier: u8,
    /// Base fee stamped on this market's pool (immutable for its life).
    pub pool_fee_bps: u16,
    /// Typed override active at the authoritative request fence. When the
    /// override key itself did not change, other fee inputs must still be
    /// evaluated against this base rather than the immutable pool stamp.
    pub active_fee_override: crate::money::FeeBpsOverride,
    /// Committed rep values the process is running with (fallbacks when a
    /// key never changed).
    pub rep: crate::model::RepConfig,
    pub action: crate::model::TradeAction,
    /// Seconds since this user's last buy of the outcome, when selling.
    pub secs_since_last_buy: Option<i64>,
    /// Market whose `fee_bps_override:{id}` is relevant (D36).
    pub market: Option<crate::model::MarketId>,
}

fn value_at_preview<'a>(changes: &'a [ConfigChange], key: &str) -> Option<&'a Value> {
    changes
        .iter()
        .find(|c| c.key == key)
        .map(|c| c.old.as_ref().unwrap_or(&Value::Null))
}

fn value_now<'a>(changes: &'a [ConfigChange], key: &str) -> Option<&'a Value> {
    changes.iter().rev().find(|c| c.key == key).map(|c| &c.new)
}

/// D25a: `409 StaleConfig` iff a preview-relevant field for THIS market and
/// user changed since the previewed generation — this user's effective
/// position cap, the flip window when the pending action would flip, or any
/// effective-fee input for this user (`min_fee_bps`,
/// `fee_discount_bp_by_tier` — grok r3 N9). A global `trade_fee_bps` change
/// is new-market-only and NEVER stales an existing book.
#[must_use]
pub fn preview_relevant_drift(changes: &[ConfigChange], ctx: &StalenessContext) -> bool {
    if changes.is_empty() {
        return false;
    }
    let tier = usize::from(ctx.user_tier.min(4));

    let tier_entry = |v: Option<&Value>, fallback: i64| -> i64 {
        v.and_then(|value| as_i64_array::<5>(value).ok())
            .map_or(fallback, |arr| arr[tier])
    };

    // Effective position cap for this user's tier.
    let cap_fallback = ctx.rep.position_cap_micro_by_tier[tier];
    let cap_before = tier_entry(
        value_at_preview(changes, "position_cap_micro_by_tier"),
        cap_fallback,
    );
    let cap_after = tier_entry(value_now(changes, "position_cap_micro_by_tier"), cap_before);
    if changes
        .iter()
        .any(|c| c.key == "position_cap_micro_by_tier")
        && cap_before != cap_after
    {
        return true;
    }

    // Effective fee inputs: compute the shipped `effective_fee` formula with
    // the at-preview values and the current values; drift iff they differ.
    let scalar =
        |v: Option<&Value>, fallback: i64| -> i64 { v.and_then(Value::as_i64).unwrap_or(fallback) };
    let min_fee_before = scalar(
        value_at_preview(changes, "min_fee_bps"),
        i64::from(ctx.rep.min_fee_bps),
    );
    let min_fee_after = scalar(value_now(changes, "min_fee_bps"), min_fee_before);
    let discount_before = tier_entry(
        value_at_preview(changes, "fee_discount_bp_by_tier"),
        i64::from(ctx.rep.fee_discount_bp_by_tier[tier]),
    );
    let discount_after = tier_entry(
        value_now(changes, "fee_discount_bp_by_tier"),
        discount_before,
    );
    let window_before = scalar(
        value_at_preview(changes, "discount_flip_window_secs"),
        i64::try_from(ctx.rep.discount_flip_window_secs).unwrap_or(i64::MAX),
    );
    let window_after = scalar(
        value_now(changes, "discount_flip_window_secs"),
        window_before,
    );

    let flips = |window: i64| -> bool {
        ctx.action == crate::model::TradeAction::Sell
            && window != 0
            && ctx.secs_since_last_buy.is_some_and(|secs| secs < window)
    };
    let override_key = ctx
        .market
        .map(crate::money::fee_override_key)
        .unwrap_or_default();
    let override_changed =
        !override_key.is_empty() && changes.iter().any(|change| change.key == override_key);
    let (override_before, override_after) = if override_changed {
        (
            crate::money::FeeBpsOverride::parse(value_at_preview(changes, &override_key)),
            crate::money::FeeBpsOverride::parse(value_now(changes, &override_key)),
        )
    } else {
        (Ok(ctx.active_fee_override), Ok(ctx.active_fee_override))
    };
    let (Ok(override_before), Ok(override_after)) = (override_before, override_after) else {
        // A malformed persisted fee override is conservatively relevant;
        // the authoritative request path will then fail closed while parsing
        // the same value rather than price with a silent fallback.
        return true;
    };
    let base_before = i64::from(override_before.base_bps(ctx.pool_fee_bps));
    let base_after = i64::from(override_after.base_bps(ctx.pool_fee_bps));

    let fee = |base: i64, min_fee: i64, discount: i64, flip: bool| -> i64 {
        if flip {
            base
        } else {
            (base - discount).max(min_fee)
        }
    };
    let fee_before = fee(
        base_before,
        min_fee_before,
        discount_before,
        flips(window_before),
    );
    let fee_after = fee(
        base_after,
        min_fee_after,
        discount_after,
        flips(window_after),
    );
    fee_before != fee_after
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use serde_json::json;

    use super::*;
    use crate::model::RepConfig;

    fn seed_snapshot() -> Snapshot {
        let mut snap = Snapshot::new();
        snap.insert("trade_fee_bps".into(), json!(100));
        snap.insert("min_fee_bps".into(), json!(10));
        snap.insert("discount_flip_window_secs".into(), json!(3600));
        snap.insert("fee_discount_bp_by_tier".into(), json!([0, 0, 10, 20, 30]));
        snap.insert(
            "position_cap_micro_by_tier".into(),
            json!([
                25_000_000i64,
                50_000_000i64,
                100_000_000i64,
                250_000_000i64,
                500_000_000i64
            ]),
        );
        snap.insert("trading_paused".into(), json!(false));
        snap.insert("daily_slots".into(), json!(2));
        snap
    }

    #[test]
    fn the_published_seed_discount_vector_validates_against_the_min_fee_floor() {
        // codex r4 NEW-1 / grok r3 N9: the floor applies to the COMPUTED
        // effective fee, so the published [0,0,10,20,30] seed re-validates.
        let snap = seed_snapshot();
        let changes = validate_patch(
            &snap,
            &json!({"fee_discount_bp_by_tier": [0, 0, 10, 20, 30]}),
        )
        .unwrap();
        assert!(changes.is_empty(), "no-op patch produces no change rows");
    }

    #[test]
    fn bounds_and_unknown_keys_are_typed_422s() {
        let snap = seed_snapshot();
        assert!(validate_patch(&snap, &json!({"trade_fee_bps": 5})).is_err());
        assert!(validate_patch(&snap, &json!({"trade_fee_bps": 300})).is_err());
        assert!(validate_patch(&snap, &json!({"no_such_key": 1})).is_err());
        assert!(validate_patch(&snap, &json!({"trade_fee_bps": "not-a-number"})).is_err());
        assert!(validate_patch(&snap, &json!([1, 2])).is_err());
        assert!(validate_patch(&snap, &json!({})).is_err());
    }

    #[test]
    fn max_delta_rules_bind_per_apply() {
        let snap = seed_snapshot();
        // 100 → 120 is the exact fee limit; 121 exceeds it.
        assert!(validate_patch(&snap, &json!({"trade_fee_bps": 120})).is_ok());
        assert!(validate_patch(&snap, &json!({"trade_fee_bps": 121})).is_err());
        // Discount entries move at most 10bp each.
        assert!(validate_patch(&snap, &json!({"fee_discount_bp_by_tier": [0,0,20,30,40]})).is_ok());
        assert!(
            validate_patch(&snap, &json!({"fee_discount_bp_by_tier": [0,0,21,30,40]})).is_err()
        );
        // daily_slots moves one step.
        assert!(validate_patch(&snap, &json!({"daily_slots": 3})).is_ok());
        assert!(validate_patch(&snap, &json!({"daily_slots": 4})).is_err());
    }

    #[test]
    fn monotonicity_and_published_floors_hold() {
        let snap = seed_snapshot();
        assert!(
            validate_patch(
                &snap,
                &json!({"fee_discount_bp_by_tier": [0, 10, 5, 20, 30]})
            )
            .is_err(),
            "non-monotone discounts rejected"
        );
        assert!(
            validate_patch(
                &snap,
                &json!({"position_cap_micro_by_tier": [1, 50_000_000i64, 100_000_000i64, 250_000_000i64, 500_000_000i64]})
            )
            .is_err(),
            "caps below the published tier table rejected"
        );
    }

    #[test]
    fn discount_entries_bound_to_the_prospective_fee() {
        let snap = seed_snapshot();
        // Discounts may not exceed the PROSPECTIVE trade_fee_bps when the
        // same patch lowers it: entry 30 > 25 must reject.
        let err = validate_patch(
            &snap,
            &json!({"trade_fee_bps": 90, "fee_discount_bp_by_tier": [0, 0, 10, 20, 95]}),
        );
        assert!(err.is_err());
    }

    #[test]
    fn integrity_bands_anchor_at_shipped_defaults() {
        let snap = seed_snapshot();
        assert!(validate_patch(&snap, &json!({"integrity_young_share_max_ppm": 600_000})).is_ok());
        assert!(validate_patch(&snap, &json!({"integrity_young_share_max_ppm": 601_000})).is_err());
        assert!(validate_patch(&snap, &json!({"integrity_young_share_max_ppm": 399_000})).is_err());
    }

    #[test]
    fn sensitivity_and_roles_follow_the_catalog() {
        assert!(is_sensitive("trade_fee_bps"));
        assert!(is_sensitive(
            "voting_paused:2c1a2f3e-0000-0000-0000-000000000000"
        ));
        assert!(!is_sensitive("trading_paused"));
        assert!(!is_sensitive("sweep_delay_secs"));

        assert!(role_may_write(AdminRole::Ops, "trading_paused"));
        assert!(
            !role_may_write(AdminRole::Superadmin, "trading_paused"),
            "direct keys stay with their listed role — no hierarchy"
        );
        assert!(role_may_write(AdminRole::Finance, "trade_fee_bps"));
        assert!(role_may_write(AdminRole::Superadmin, "trade_fee_bps"));
        assert!(!role_may_write(AdminRole::Curator, "trade_fee_bps"));
        assert!(role_may_write(AdminRole::Curator, "daily_slots"));
        assert!(!role_may_write(AdminRole::Ops, "unknown_key"));
    }

    #[test]
    fn pattern_keys_require_a_uuid_suffix() {
        assert!(rule_for("market_paused:6b8bd4c4-9a0f-4d3e-8f5a-111111111111").is_some());
        assert!(rule_for("market_paused:not-a-uuid").is_none());
        assert!(rule_for("market_paused:").is_none());
    }

    #[test]
    fn exclusive_fences_are_canonically_ordered() {
        let a = MarketId(uuid::Uuid::from_u128(2));
        let b = MarketId(uuid::Uuid::from_u128(1));
        let keys = vec![
            format!("voting_paused:{}", a.0),
            "trading_paused".to_string(),
            format!("market_paused:{}", b.0),
            "trade_fee_bps".to_string(),
        ];
        let fences = exclusive_fences_for_patch(&keys);
        assert_eq!(fences[0], "trading-global");
        let mut sorted = fences[1..].to_vec();
        sorted.sort();
        assert_eq!(fences[1..], sorted[..], "scoped fences sorted by name");
        assert_eq!(fences.len(), 3, "non-pause keys take no fence");
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_depth_and_hashes_stably() {
        let a = json!({"b": 1, "a": {"d": 2, "c": [3, {"f": 4, "e": 5}]}});
        let b = json!({"a": {"c": [3, {"e": 5, "f": 4}], "d": 2}, "b": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(patch_hash(&a), patch_hash(&b));
        assert_eq!(patch_hash(&a).len(), 64);
    }

    #[test]
    fn fingerprints_pin_the_canonical_request_shape() {
        let market = MarketId(uuid::Uuid::from_u128(7));
        let user = UserId(uuid::Uuid::from_u128(9));
        let with_version = trade_fingerprint(
            market,
            user,
            domain::amm::Side::Yes,
            crate::model::TradeAction::Buy,
            5_000_000,
            Some(3),
        );
        let without = trade_fingerprint(
            market,
            user,
            domain::amm::Side::Yes,
            crate::model::TradeAction::Buy,
            5_000_000,
            None,
        );
        assert_ne!(
            with_version, without,
            "the version is part of the fingerprint"
        );
        assert_ne!(
            with_version,
            trade_fingerprint(
                market,
                user,
                domain::amm::Side::Yes,
                crate::model::TradeAction::Buy,
                5_000_001,
                Some(3),
            ),
            "the amount is part of the fingerprint"
        );
        assert_ne!(
            vote_fingerprint(market, user, domain::amm::Side::Yes, 60),
            vote_fingerprint(market, user, domain::amm::Side::No, 60),
        );
        assert!(validate_replay_fingerprint(None, "new").is_ok());
        assert!(validate_replay_fingerprint(Some("same"), "same").is_ok());
        assert_eq!(
            validate_replay_fingerprint(Some("old"), "new"),
            Err(AppError::IdempotencyConflict)
        );
    }

    fn drift_ctx(action: crate::model::TradeAction) -> StalenessContext {
        // The PUBLISHED economy values (`RepConfig::default()` is the
        // all-zero test scaffold, under which every effective-fee compare
        // would collapse to the base fee).
        StalenessContext {
            user_tier: 2,
            pool_fee_bps: 100,
            active_fee_override: crate::money::FeeBpsOverride::Inherit,
            rep: RepConfig {
                fee_discount_bp_by_tier: [0, 0, 10, 20, 30],
                min_fee_bps: 10,
                discount_flip_window_secs: 3600,
                ..RepConfig::default()
            },
            action,
            secs_since_last_buy: None,
            market: None,
        }
    }

    #[test]
    fn cap_change_at_this_users_tier_is_relevant_drift() {
        let changes = vec![ConfigChange {
            key: "position_cap_micro_by_tier".into(),
            old: Some(json!([
                25_000_000i64,
                50_000_000i64,
                100_000_000i64,
                250_000_000i64,
                500_000_000i64
            ])),
            new: json!([
                25_000_000i64,
                50_000_000i64,
                120_000_000i64,
                250_000_000i64,
                500_000_000i64
            ]),
        }];
        assert!(preview_relevant_drift(
            &changes,
            &drift_ctx(crate::model::TradeAction::Buy)
        ));
    }

    #[test]
    fn cap_change_at_another_tier_is_not_relevant() {
        let changes = vec![ConfigChange {
            key: "position_cap_micro_by_tier".into(),
            old: Some(json!([
                25_000_000i64,
                50_000_000i64,
                100_000_000i64,
                250_000_000i64,
                500_000_000i64
            ])),
            new: json!([
                25_000_000i64,
                50_000_000i64,
                100_000_000i64,
                250_000_000i64,
                900_000_000i64
            ]),
        }];
        assert!(!preview_relevant_drift(
            &changes,
            &drift_ctx(crate::model::TradeAction::Buy)
        ));
    }

    #[test]
    fn min_fee_change_is_relevant_only_when_it_moves_this_users_effective_fee() {
        // Tier 2 discount 10 → effective fee max(100-10, min_fee). A floor
        // move 10 → 20 keeps 90 ≥ 20: NOT relevant. A floor move to 95
        // binds: relevant.
        let benign = vec![ConfigChange {
            key: "min_fee_bps".into(),
            old: Some(json!(10)),
            new: json!(20),
        }];
        assert!(!preview_relevant_drift(
            &benign,
            &drift_ctx(crate::model::TradeAction::Buy)
        ));
        let binding = vec![ConfigChange {
            key: "min_fee_bps".into(),
            old: Some(json!(10)),
            new: json!(95),
        }];
        assert!(preview_relevant_drift(
            &binding,
            &drift_ctx(crate::model::TradeAction::Buy)
        ));
    }

    #[test]
    fn unchanged_active_override_is_used_when_another_fee_input_drifts() {
        let changes = vec![ConfigChange {
            key: "min_fee_bps".into(),
            old: Some(json!(10)),
            new: json!(20),
        }];
        let mut ctx = drift_ctx(crate::model::TradeAction::Buy);
        ctx.active_fee_override = crate::money::FeeBpsOverride::Override(10);
        assert!(preview_relevant_drift(&changes, &ctx));
    }

    #[test]
    fn discount_change_at_this_tier_is_relevant() {
        let changes = vec![ConfigChange {
            key: "fee_discount_bp_by_tier".into(),
            old: Some(json!([0, 0, 10, 20, 30])),
            new: json!([0, 0, 20, 20, 30]),
        }];
        assert!(preview_relevant_drift(
            &changes,
            &drift_ctx(crate::model::TradeAction::Buy)
        ));
    }

    #[test]
    fn flip_window_churn_does_not_stale_a_non_flip_buy() {
        // The exit-criteria e2e shape (§3 item 3): flip-window drift is
        // relevant only when the pending action would flip.
        let changes = vec![ConfigChange {
            key: "discount_flip_window_secs".into(),
            old: Some(json!(3600)),
            new: json!(7200),
        }];
        assert!(!preview_relevant_drift(
            &changes,
            &drift_ctx(crate::model::TradeAction::Buy)
        ));
        // A sell whose last buy sits between the two windows flips only
        // under the new window: relevant.
        let mut sell = drift_ctx(crate::model::TradeAction::Sell);
        sell.secs_since_last_buy = Some(5_000);
        assert!(preview_relevant_drift(&changes, &sell));
        // A sell far outside both windows: irrelevant.
        sell.secs_since_last_buy = Some(50_000);
        assert!(!preview_relevant_drift(&changes, &sell));
    }

    #[test]
    fn fee_override_change_stales_that_market_only() {
        let market = crate::model::MarketId(uuid::Uuid::from_u128(7));
        let key = crate::money::fee_override_key(market);
        let changes = vec![ConfigChange {
            key,
            old: Some(json!("inherit")),
            new: json!({"override": 40}),
        }];
        let mut ctx = drift_ctx(crate::model::TradeAction::Buy);
        ctx.market = Some(market);
        assert!(preview_relevant_drift(&changes, &ctx));
        ctx.market = Some(crate::model::MarketId(uuid::Uuid::from_u128(8)));
        assert!(!preview_relevant_drift(&changes, &ctx));
    }

    #[test]
    fn malformed_fee_override_is_conservatively_relevant_drift() {
        let market = crate::model::MarketId(uuid::Uuid::from_u128(7));
        let changes = vec![ConfigChange {
            key: crate::money::fee_override_key(market),
            old: Some(json!("inherit")),
            new: json!({"override": "corrupt"}),
        }];
        let mut ctx = drift_ctx(crate::model::TradeAction::Buy);
        ctx.market = Some(market);
        assert!(preview_relevant_drift(&changes, &ctx));
    }

    #[test]
    fn unrelated_keys_are_never_relevant() {
        let changes = vec![ConfigChange {
            key: "trade_fee_bps".into(),
            old: Some(json!(100)),
            new: json!(120),
        }];
        assert!(
            !preview_relevant_drift(&changes, &drift_ctx(crate::model::TradeAction::Buy)),
            "a global fee change is new-market-only and never 409s an existing book"
        );
    }

    #[test]
    fn pause_values_read_falsy_when_absent_or_malformed() {
        assert!(!pause_in_force(None));
        assert!(!pause_in_force(Some(&json!("yes"))));
        assert!(!pause_in_force(Some(&json!(false))));
        assert!(pause_in_force(Some(&json!(true))));
    }

    #[test]
    fn every_catalog_row_is_reachable_and_self_consistent() {
        for rule in CATALOG {
            if rule.pattern {
                assert!(rule.key.ends_with(':'), "{}", rule.key);
                let concrete = format!("{}{}", rule.key, uuid::Uuid::nil());
                assert!(rule_for(&concrete).is_some());
            } else {
                assert!(rule_for(rule.key).is_some());
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn catalog_validators_cover_every_boundary_and_delta_rule() {
        let empty = Snapshot::new();

        for rule in CATALOG {
            let key = if rule.pattern {
                format!("{}{}", rule.key, uuid::Uuid::nil())
            } else {
                rule.key.to_string()
            };
            assert!(
                (rule.check)(&key, &json!("wrong-type"), &empty).is_err(),
                "{} must reject the wrong JSON type",
                rule.key
            );
        }

        assert_eq!(as_i64(&json!("1")), Err("expected an integer"));
        assert_eq!(as_bool(&json!(1)), Err("expected a boolean"));
        assert_eq!(as_i64_array::<2>(&json!(1)), Err("expected an array"));
        assert_eq!(as_i64_array::<2>(&json!([1])), Err("wrong array length"));
        assert_eq!(
            as_i64_array::<2>(&json!([1, false])),
            Err("expected integer entries")
        );
        assert_eq!(as_i64_array::<2>(&json!([1, 2])).unwrap(), [1, 2]);
        assert_eq!(bounded(0, 1, 2), Err("out of bounds"));
        assert_eq!(bounded(3, 1, 2), Err("out of bounds"));
        assert!(bounded(1, 1, 2).is_ok());
        assert_eq!(
            max_delta(Some(10), 13, 2),
            Err("delta exceeds the per-apply limit")
        );
        assert!(max_delta(None, 99, 0).is_ok());
        assert_eq!(
            within_2x(Some(10), 21),
            Err("change exceeds 2x of the prior value")
        );
        assert_eq!(
            within_2x(Some(10), 4),
            Err("change exceeds 2x of the prior value")
        );
        assert!(within_2x(Some(0), 999).is_ok());
        assert_eq!(
            monotone_non_decreasing(&[1, 3, 2]),
            Err("entries must be monotone non-decreasing")
        );
        assert!(monotone_non_decreasing(&[1, 2, 3]).is_ok());
        assert!(check_min_fee("", &json!(10), &empty).is_ok());
        assert!(check_min_fee("", &json!(51), &empty).is_err());
        assert!(check_flip_window("", &json!(600), &empty).is_ok());
        assert!(check_flip_window("", &json!(599), &empty).is_err());

        let mut snap = Snapshot::new();
        snap.insert("fee_discount_bp_by_tier".into(), json!([0, 0, 10, 20, 30]));
        snap.insert(
            "rep_tier_thresholds_micro".into(),
            json!([100, 200, 300, 400, 500]),
        );
        snap.insert("rep_score_min_pot_micro".into(), json!(100));
        snap.insert("seed_micro_daily".into(), json!(100));
        snap.insert("daily_seed_budget_micro".into(), json!(2_000_000));
        snap.insert("payout_hold_threshold_micro".into(), json!(2_000_000));
        snap.insert("sweep_delay_secs".into(), json!(120));
        snap.insert("faucet_per_call_cap_micro".into(), json!(100));

        assert!(check_discounts("", &json!([0, 0, 20, 30, 40]), &snap).is_ok());
        assert!(check_discounts("", &json!([0]), &snap).is_err());
        assert!(check_discounts("", &json!([0, 0, 10, 20, false]), &snap).is_err());
        assert!(check_discounts("", &json!([0, 0, 10, 20, 30]), &empty).is_ok());
        assert!(check_discounts("", &json!([0, 0, 21, 30, 40]), &snap).is_err());
        assert!(check_discounts("", &json!([0, 20, 10, 30, 40]), &empty).is_err());
        assert!(check_discounts("", &json!([0, 0, 10, 20, 101]), &empty).is_err());
        assert!(check_position_caps(
            "",
            &json!([
                25_000_000i64,
                50_000_000i64,
                100_000_000i64,
                250_000_000i64,
                500_000_000i64
            ]),
            &empty,
        )
        .is_ok());
        assert!(check_position_caps(
            "",
            &json!([
                25_000_000i64,
                50_000_000i64,
                90_000_000i64,
                250_000_000i64,
                500_000_000i64
            ]),
            &empty,
        )
        .is_err());
        assert!(check_tier_thresholds("", &json!([110, 220, 330, 440, 550]), &snap).is_ok());
        assert!(check_tier_thresholds("", &json!([1, 2, 3, 4, 5]), &empty).is_ok());
        assert!(check_tier_thresholds("", &json!([111, 220, 330, 440, 550]), &snap).is_err());
        assert!(check_tier_thresholds("", &json!([2, 1, 3, 4, 5]), &empty).is_err());

        assert!(check_min_pot("", &json!(200), &snap).is_ok());
        assert!(check_min_pot("", &json!(201), &snap).is_err());
        assert!(check_min_pot("", &json!(-1), &snap).is_err());
        assert!(check_seed("seed_micro_daily", &json!(200), &snap).is_ok());
        assert!(check_seed("seed_micro_daily", &json!(201), &snap).is_err());
        assert!(check_seed("seed_micro_daily", &json!(0), &snap).is_err());
        assert!(check_seed_budget("", &json!(4_000_000), &snap).is_ok());
        assert!(check_seed_budget("", &json!(4_000_001), &snap).is_err());
        assert!(check_seed_budget("", &json!(999_999), &snap).is_err());
        assert!(check_hidden_window("", &json!(60), &empty).is_ok());
        assert!(check_hidden_window("", &json!(59), &empty).is_err());
        assert!(check_min_votes_floor("", &json!(3), &empty).is_ok());
        assert!(check_min_votes_floor("", &json!(2), &empty).is_err());
        assert!(check_oi_floor("", &json!(0), &empty).is_ok());
        assert!(check_oi_floor("", &json!(-1), &empty).is_err());
        assert!(check_hold_threshold("", &json!(4_000_000), &snap).is_ok());
        assert!(check_hold_threshold("", &json!(4_000_001), &snap).is_err());
        assert!(check_hold_threshold("", &json!(999_999), &snap).is_err());
        assert!(check_sweep_delay("", &json!(240), &snap).is_ok());
        assert!(check_sweep_delay("", &json!(241), &snap).is_err());
        assert!(check_max_votes("", &json!(15), &empty).is_ok());
        assert!(check_max_votes("", &json!(14), &empty).is_err());
        assert!(check_vote_window("", &json!(5_400), &empty).is_ok());
        assert!(check_vote_window("", &json!(5_401), &empty).is_err());
        assert!(
            check_integrity_ppm("integrity_young_share_max_ppm", &json!(400_000), &empty,).is_ok()
        );
        assert!(check_integrity_ppm("unknown", &json!(1), &empty).is_err());
        assert!(check_cadence("", &json!(900), &empty).is_ok());
        assert!(check_cadence("", &json!(899), &empty).is_err());
        assert!(check_daily_slots("", &json!(1), &empty).is_ok());
        assert!(check_daily_slots("", &json!(5), &empty).is_err());
        assert!(check_bool("", &json!(true), &empty).is_ok());
        assert!(check_bool("", &json!(1), &empty).is_err());
        assert!(check_faucet_cap("", &json!(200), &snap).is_ok());
        assert!(check_faucet_cap("", &json!(201), &snap).is_err());
        assert!(check_faucet_cap("", &json!(-1), &snap).is_err());
        assert!(check_remedial_market_cap("", &json!(500_000_000), &empty).is_ok());
        assert!(check_remedial_market_cap("", &json!(500_000_001), &empty).is_err());
        assert!(check_remedial_daily_cap("", &json!(2_000_000_000), &empty).is_ok());
        assert!(check_remedial_daily_cap("", &json!(2_000_000_001i64), &empty).is_err());
        assert!(check_receivable_cap("", &json!(1_000_000_000_000i64), &empty).is_ok());
        assert!(check_receivable_cap("", &json!(1_000_000_000_001i64), &empty).is_err());
        assert!(check_writeoff_item_cap("", &json!(500_000_000), &empty).is_ok());
        assert!(check_writeoff_item_cap("", &json!(500_000_001), &empty).is_err());
        assert!(check_writeoff_daily_cap("", &json!(2_000_000_000), &empty).is_ok());
        assert!(check_writeoff_daily_cap("", &json!(2_000_000_001i64), &empty).is_err());
        assert!(check_proposal_ttl("", &json!(60), &empty).is_ok());
        assert!(check_proposal_ttl("", &json!(59), &empty).is_err());
        assert!(check_dual_delay("", &json!(0), &empty).is_ok());
        assert!(check_dual_delay("", &json!(-1), &empty).is_err());
        assert!(check_bonus_structure("", &json!("real_money"), &empty).is_ok());
        assert!(check_bonus_structure("", &json!("nope"), &empty).is_err());
        assert!(check_region_allowset("", &json!(["CA"]), &empty).is_ok());
        assert!(check_region_allowset("", &json!([]), &empty).is_err());
        assert!(check_fee_override("", &json!("inherit"), &empty).is_ok());
        assert!(check_fee_override("", &json!({"override": 50}), &empty).is_ok());
        assert!(check_fee_override("", &json!({"override": 1}), &empty).is_err());
        assert!(check_fee_override("", &json!({"override": "50"}), &empty).is_err());
        assert!(check_fee_override("", &json!({}), &empty).is_err());
        assert!(
            check_fee_override("", &json!(50), &empty).is_err(),
            "D36 is a typed inherit|override enum; a bare integer is invalid"
        );
        assert!(check_fee_override("", &json!(1), &empty).is_err());
        assert!(check_fee_override("", &json!("nope"), &empty).is_err());
    }

    #[test]
    fn phase7_seed_snapshot_validates_every_key() {
        let mut snap = seed_snapshot();
        snap.extend(phase7_seed_snapshot());
        for (key, value) in phase7_seed_snapshot() {
            let patch = serde_json::Map::from_iter([(key.clone(), value)]);
            assert!(
                validate_patch(&snap, &Value::Object(patch)).is_ok(),
                "{key}"
            );
        }
        assert!(
            validate_patch(&snap, &json!({"withdraw_auto_approve_micro": 200_000_000})).is_err()
        );
        assert!(
            validate_patch(&snap, &json!({"aml_structuring_floor_micro": 600_000_000})).is_err()
        );
        assert!(role_may_write(AdminRole::Finance, "feature_referrals"));
        assert!(!role_may_write(AdminRole::Ops, "feature_referrals"));
        assert!(check_p7_cross_keys(&phase7_seed_snapshot()).is_ok());
        let empty = Snapshot::new();
        assert!(check_kyc_tier("deposit_kyc_tier", &json!(1), &empty).is_ok());
        assert!(check_kyc_tier("deposit_kyc_tier", &json!(3), &empty).is_err());
        assert!(check_region_version("", &json!(1), &empty).is_ok());
        let mut ver = Snapshot::new();
        ver.insert("region_allowset_version".into(), json!(1));
        assert!(check_region_version("", &json!(2), &ver).is_ok());
        assert!(check_region_version("", &json!(3), &ver).is_err());
        assert!(check_region_version("", &json!(0), &empty).is_err());
        assert!(mul_delta(Some(0), 5_000_000, 4, 5_000_000).is_ok());
        assert!(mul_delta(Some(0), 9_000_000, 4, 5_000_000).is_err());
        assert!(mul_delta(Some(10), 0, 4, 5).is_ok());
        assert!(mul_delta(None, 0, 4, 5).is_ok());
        assert!(check_region_allowset("", &json!([1]), &empty).is_err());
        assert!(check_shadow_cap("shadow_trade_cap_micro", &json!(-1), &empty).is_err());
        let mut disabled = phase7_seed_snapshot();
        disabled.insert("withdraw_auto_approve_micro".into(), json!(0));
        assert!(check_p7_cross_keys(&disabled).is_ok());
        disabled.insert("withdraw_dual_control_micro".into(), json!(1));
        assert!(check_p7_cross_keys(&disabled).is_err());
        let mut chain = phase7_seed_snapshot();
        chain.insert("withdraw_auto_approve_micro".into(), json!(60_000_000));
        chain.insert("withdraw_dual_control_micro".into(), json!(50_000_000));
        assert!(check_p7_cross_keys(&chain).is_err());
        let mut band = phase7_seed_snapshot();
        band.insert("aml_structuring_floor_micro".into(), json!(0));
        assert!(check_p7_cross_keys(&band).is_err());
        band.insert("aml_structuring_floor_micro".into(), json!(600_000_000));
        assert!(check_p7_cross_keys(&band).is_err());
    }
}
