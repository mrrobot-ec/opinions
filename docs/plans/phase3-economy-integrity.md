# Opinions — Phase 3 (Economy & Full Integrity) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. **This revision integrates the full P3R1 review fix set (codex BLOCKED verdict + grok fix-first; see docs/reviews/p3r1-resolution.md).**

**Goal:** The voter economy becomes real and the oracle gets its full defense: reputation (EWMA of vote scores, quality-floored) with tier-gated position caps and fee discounts, weekly leaderboards from **immutable realization facts**, the resolution integrity sweep (big pots hold in `Resolving` under a two-signal flag policy before money moves), sybil signals captured honestly at the edge, LP PnL with a race-safe D23 kill-switch, and fee accounting with an honest breakdown.

**Architecture:** unchanged and binding — `domain ← application ← adapters ← main`, machine-checked; pure math in `domain`; application logic over ports; edge capture as plain data; `web/` REST+WS only.

## Global Constraints (Phase 3 deltas)

- **NO GIT / NO VCS COMMANDS, EVER** (standing).
- **Coverage: 100% line on all three crates holds** (`coverage-allowlist.toml` stays empty unless truly forced, justified per entry). TDD failing-first; contract suites extend for every new port method (fake + Pg).
- **Lock order (extended, global):** `serialize_key` (advisory class 1) → **user locks in canonical UUID ascending order** (class 2; one for trades, the preflighted voter set for resolves) → `market_for_update` → row locks (accounts in id order) → writes. The LP kill-switch adds advisory **class 3** (`lp_kill`, one global key) taken only inside `SeedMarket` after `serialize_key`. Every writer takes locks in this order — deadlock-freedom is by construction and contract-tested.
- **Typed configs with validated constructors that FAIL STARTUP on present-but-invalid env** (codex M5, completed P3R2 — no silent defaults for malformed values). The full validation set: thresholds strictly ascending in `0..=1_000_000`; caps nonnegative nondecreasing; discounts ≤ 10_000; `min_fee_bps ≤ 10_000` **and every seeded market's base fee ≥ min_fee** (checked in SeedMarket); every ppm share/coverage/multiplier field in `0..=1_000_000` (multiplier may exceed — bounded ≤ 100_000_000); `min_votes_for_ratios ≥ 1`; `rep_score_min_pot_micro ≥ 0` (0 = floor disabled, stated); `discount_flip_window_secs ≥ 0` (0 = anti-churn rule off, stated); `leaderboard_min_scored ≥ 1`; `DEVICE_HASH_SECRET` non-empty whenever device capture is enabled; `TRUSTED_PROXY_CIDRS` entries must parse as CIDRs. Each rule has a startup-failure test:
  - `RepConfig { half_life: HalfLife, tier_thresholds_micro: [i64; 4], position_cap_micro_by_tier: [i64; 5], fee_discount_bp_by_tier: [u16; 5], min_fee_bps: u16, rep_score_min_pot_micro: i64, discount_flip_window_secs: u64, leaderboard_min_scored: u32 }` — thresholds strictly ascending within `0..=1_000_000`; caps nonnegative nondecreasing; each discount ≤ 10_000 and floor-saturation behavior explicitly tested; `HalfLife` is an enum of supported values (below).
  - `IntegritySweepConfig { payout_hold_threshold_micro: i64 (>0), sweep_delay_secs: u64 (>0), burst_window_secs: u64 (>0), prior_horizon_windows: u32 (>0), burst_multiplier_ppm: u32, young_account_age_secs: u64, young_account_share_max_ppm: u32, subnet_share_max_ppm: u32, device_share_max_ppm: u32, min_votes_for_ratios: u32, min_metadata_coverage_ppm: u32 }`.
  - `LpKillConfig { max_loss_micro: i64 (>0; compared as sum ≤ −max_loss), window_days: u32 (>0) }`.
  - `TRUSTED_PROXY_CIDRS` (list; empty default): forwarded-for is honored **only when the direct peer is inside one of these CIDRs** (codex m1 — a bare TRUST_PROXY flag is forgeable); `DEVICE_HASH_SECRET` for HMAC.

**Worker protocol:** one worker owns the Rust chain **3.0 → 3.1 → 3.2 → 3.3** sequentially; `web/` task 3.4 parallel; 3.5 last.

---

### Task 3.0: Migration 0005 + pure domain math (reputation, effective fee, anomaly checks)

**Files:**
- Create: `migrations/0005_economy_integrity.sql`, `crates/domain/src/reputation.rs`, `crates/domain/src/integrity.rs`, `crates/domain/src/fee_policy.rs`
- Modify: `crates/domain/src/lib.rs`

**Migration 0005 (revised per P3R1):**

```sql
alter table votes add column cast_ip inet;
alter table votes add column device_hash text;          -- stores HMAC(secret, client id) — never the raw id
alter table vote_scores add column created_at timestamptz not null default now();

alter table markets add column lp_pnl_micro bigint;
alter table markets add column settled_at timestamptz;         -- codex P3R2 N2: the LP window's time axis, written in the settlement tx
alter table markets add column integrity_due_at timestamptz;   -- codex M2: review is scheduled, not same-tick
create index markets_settled_idx on markets (settled_at) where lp_pnl_micro is not null;

-- reputation rows must exist for every user (codex B2): backfill + creation-path guarantee
insert into reputation (user_id, rep_micro, tier, updated_at)
  select id, 0, 0, now() from users
  on conflict (user_id) do nothing;

-- immutable realization facts — the ONLY source of trader PnL (codex B5)
create table realizations (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  market_id uuid not null references markets(id),
  outcome_id uuid not null references outcomes(id),
  source text not null check (source in ('sell','settlement','void')),
  realized_delta_micro bigint not null,      -- signed; zero-payout losses are negative rows
  txn_id uuid not null references ledger_transactions(id),
  created_at timestamptz not null default now(),
  unique (txn_id, user_id, outcome_id),
  -- codex P3R2 N1: the cross-market invariant every other table carries
  foreign key (outcome_id, market_id) references outcomes (id, market_id)
);
create index realizations_window_idx on realizations (created_at, user_id);

create table integrity_reports (
  id uuid primary key default gen_random_uuid(),
  market_id uuid not null references markets(id) unique,
  checks jsonb not null,        -- per-check: {name, value, threshold, coverage, strength, note, config_version}
  verdict text not null check (verdict in ('pass','flag')),
  created_at timestamptz not null default now()
);

create index vote_scores_time_idx on vote_scores (created_at);
create index votes_market_created_idx on votes (market_id, created_at);
create index ledger_txn_kind_time_idx on ledger_transactions (kind, created_at, id);
create index ledger_entries_account_time_idx on ledger_entries (account_id, created_at);
create index markets_review_due_idx on markets (integrity_due_at)
  where status = 'resolving' and curator_flagged_at is null;
```

**`domain::reputation` (pure; the P3R1-corrected math):**

```rust
/// Supported half-lives with PRECOMPUTED, exactly-pinned K (codex B1: no runtime nth-root;
/// no rounding path can ever yield K == UNIT, which would freeze rep).
pub enum HalfLife { H10, H20, H40 }
impl HalfLife { pub fn k_ppm(self) -> u32 { match self { H10 => 933_033, H20 => 965_936, H40 => 982_821 } } }

/// rep' = round_half_up((rep*K + score*(UNIT−K)) / UNIT), all i128 checked, operands 0..=1_000_000.
pub fn update_rep(rep_micro: i64, score_micro: i64, h: HalfLife) -> Result<i64, RepError>;
pub fn tier_for(rep_micro: i64, thresholds: &[i64; 4]) -> u8;
```

Errors: `OperandOutOfRange` (anything outside `0..=1_000_000`). Tests are **golden integer recurrence vectors** (precomputed exactly for the pinned K values — e.g. H20: 0 with twenty 1_000_000-score updates, 1_000_000 with twenty 0-score updates; the vectors state the exact end values, and the doc comment states the proven truncation bound of round-half-up vs floor). No "approximately half" claims anywhere. Monotonicity, tier boundaries inclusive-lower (after validated ascending thresholds), saturation at bounds.

**`domain::fee_policy` (pure — ONE effective-fee authority, codex B3):**

```rust
pub struct FeeContext { pub base_bps: u16, pub tier: u8, pub discount_bp_by_tier: [u16; 5],
    pub min_fee_bps: u16, pub is_flip_within_window: bool }
pub fn effective_fee(ctx: &FeeContext) -> BasisPoints
// flip-within-window (grok B1 anti-churn): discount does NOT apply — base fee. Otherwise
// max(base − discount[tier], min_fee). Saturation cases pinned by test.
```

**`domain::integrity` (pure; definitions closed per codex M3):**

```rust
pub struct VoteStats { pub total: u32, pub last_window: u32,
    pub prior_windows_total: u32, pub prior_horizon_windows: u32,   // avg derived inside, zero-baseline rule below
    pub young_accounts: u32, pub subnet_observed: u32, pub top_subnet_count: u32,
    pub device_observed: u32, pub top_device_count: u32 }
pub enum Strength { Weak, Medium }
pub struct CheckResult { pub name: &'static str, pub value_ppm: u64, pub threshold_ppm: u64,
    pub coverage_ppm: u32, pub strength: Strength, pub flagged: bool, pub note: &'static str }
pub fn sweep_checks(s: &VoteStats, t: &SweepThresholds) -> Vec<CheckResult>;
pub fn verdict(results: &[CheckResult]) -> Verdict   // grok B3: 'flag' requires >= 2 flagged checks; 
                                                     // a single flagged check records pass-with-note
```

Check definitions (each boundary-tested): `vote_burst` — final window anchored at the **locked `closes_at`** (`[closes_at − burst_window, closes_at)`), prior average over the `prior_horizon_windows` windows immediately before it; zero-baseline rule: if prior total is 0, flag only when `last_window > min_votes_for_ratios`. `young_account_share` (age < `young_account_age_secs` at cast time). `subnet_concentration` — IPv4 grouped by /24, IPv6 by /64 (stated in the note field); skipped (coverage-marked) when `subnet_observed * 1e6 / total < min_metadata_coverage_ppm`. `device_concentration` — same coverage rule; strength `Weak` (client-asserted id), subnet `Weak`, burst/young `Medium`. All integer ppm math.

- [x] Failing tests → implement → domain 100% holds → deps-check.

---

### Task 3.1: Reputation in the money path — batched, quality-floored, fee-coherent

**Files:** modify `crates/application/src/resolve_market.rs`, `place_trade.rs`, `preview_trade.rs`, `ports.rs`, `fakes.rs`, `contract.rs`, `model.rs`; `crates/adapters/src/pg/*`; `crates/adapters/src/http/dto.rs`/`routes.rs`; main config plumbing.

**Interfaces (P3R1-corrected):**
- **Reputation rows exist by construction:** `UserWriter::insert_user` creates the reputation row in the same statement/tx (contract test: fresh user has rep row); 0005 backfilled the past.
- **Batched rep ports (codex B2 — no N+1, no unordered locks):**
  - `ResolveTx::voter_ids(&mut self, m) -> Vec<UserId>` (preflight read — votes are immutable once the market is Closed/Resolving);
  - `ResolveTx::reps_for_update(&mut self, users_sorted: &[UserId]) -> Vec<(UserId, i64, u8)>` — locks `FOR UPDATE` in the given canonical ascending UUID order;
  - `ResolveTx::save_reps(&mut self, batch: &[(UserId, i64, u8)])` — one set-based write;
  - **bulk everywhere the voter count multiplies (codex P3R2 B2):** `save_vote_scores(&mut self, batch)` and `OutboxWriter::append_batch(&mut self, events)` replace per-row calls on the resolve path — the settlement transaction issues a **constant number of set-based statements** regardless of voter count. The 1,000-voter Pg fixture asserts the whole settlement tx completes in **< 2s as a hard passing gate** (not a report-if-slow note).
- **ResolveMarket sequence change:** `serialize_key` → `voter_ids` (read) → sort → `reps_for_update` (user locks, canonical order) → `market_for_update` → …settlement… → scores (bulk as before) → **rep quality floor (grok B2):** rep updates apply only when `votes_total ≥ min_votes_to_resolve && escrow ≥ rep_score_min_pot_micro` (scores are still written for transparency; the floor is documented on the scoring page) → `domain::reputation::update_rep` per voter in memory → `save_reps` → `RepUpdated` outbox events **only on tier change**. All in the settlement tx; `resolve:<market>` replay cannot re-apply (test).
- **Voter-count budget stated:** Phase 3 accepts up to ~2,000 voters per market in one settlement tx (bulk reads/writes, three set-based statements — measured in the Pg contract test with a 1,000-voter fixture; if the measured tx time exceeds 2s the worker MUST report it rather than silently accept).
- **Fee coherence (codex B3):** `PreviewTradeCmd` gains `user_id` (routes stop discarding it — REST and converse both already send it); PreviewTrade reads `(rep, tier)` via `MarketQueries::user_rep` (lock-free — previews are estimates); PlaceTrade reads rep **in-tx after its user lock** (the same lock a concurrent resolve must take in canonical order — no stale-tier race). Both compute `domain::fee_policy::effective_fee` with `is_flip_within_window` = (sell && shares being sold were increased within `discount_flip_window_secs`). **The `last_buy_at(u, o) -> Option<OffsetDateTime>` read exists on BOTH authority surfaces (codex P3R2 N3): lock-free on `MarketQueries` for PreviewTrade — which now takes the same injected `Clock` — and in-tx on `TradeTx` for PlaceTrade; the boundary is pinned as flip iff `now − last_buy_at < window` (strict: a sell at exactly `window` after the buy is NOT a flip), tested at the boundary on both surfaces so preview and execution cannot diverge.** (Doc comment: "most recent buy timestamp for this position, None if never.") **Anti-churn scope, stated honestly (grok P3R2): the rule taxes the exit leg only — buys always earn their tier discount; a flip pays base fee on the sell. This raises round-trip cost for churners without punishing ordinary entries; it is a tax on wash-shaped behavior, not a prohibition, and the scoring page says exactly that.** The **domain quote functions gain an explicit fee parameter** (`quote_buy(pool, side, collateral, fee)`; `Pool.fee` remains the canonical stored base fee and `pool_after` preserves it — the fake can no longer accidentally persist a per-user fee as the market fee). Preview fee == trade-row fee == ledger fee entry, pinned for REST and converse, base and discounted, buy and sell.
- **Position cap (codex M1 — per market, both sides):** new port read `TradeTx::market_position_cost(&mut self, u, m) -> MicroUsd` (sum of cost across the market's outcomes, under the market lock); rule: `market_position_cost + buy_gross ≤ position_cap_micro_by_tier[tier]` → else `PositionCapExceeded` (422 names cap + tier). Sells exempt (proportional cost relief cannot increase aggregate cost — stated). Metric documented as **aggregate open cost basis per market, fees included** (a committed-capital limit, not mark-to-market).
- DTOs: profile/positions gain `{rep_micro, tier}`. **Explicit non-goal (grok M6): no tier badge on the public tape.**

**Tests:** golden-vector rep updates for winner+loser voters; quality-floor boundary (pot at floor updates, below doesn't; thin-vote market doesn't); tier-change event once; replay safety; 1,000-voter Pg fixture (bulk, timed); cap boundary incl. opposing-side accumulation (YES cap then NO buy rejected when sum exceeds); flip-window sell pays base fee, post-window sell gets discount; preview==execute==ledger fee in all four combos; tier-0 regression (Phase 2 suites untouched).

- [x] Failing tests → implement (fake + Pg + contracts) → all gates green.

---

### Task 3.2: Resolution integrity sweep — atomic authority, scheduled review, honest signals

**Files:** modify `resolve_market.rs`, `advance_due.rs`, `cast_vote.rs`, `ports.rs`, `fakes.rs`, `contract.rs`; `crates/adapters/src/pg/*`; `crates/adapters/src/http/routes.rs` (metadata capture, `under_review`, curator list), `ws.rs`; converse untouched (null metadata); main config.

**Behavior contract (P3R1-hardened):**
- **Capture:** `POST /votes` records `cast_ip` — the direct peer address, replaced by the selected `x-forwarded-for` hop **only when the direct peer is within `TRUSTED_PROXY_CIDRS`** (rule: take the last hop not in the trusted set; spoof test pinned) — and `device_hash = HMAC_SHA256(DEVICE_HASH_SECRET, x-device-id)` truncated hex (codex m1: never store the raw client id; the column name is now honest). Converse votes carry neither.
- **Hold (in ResolveMarket, atomic):** market `Closed` and locked escrow ≥ threshold → one transaction: `sweep:<market>` lifecycle command + `Closed→Resolving` + `integrity_due_at = now() + sweep_delay_secs` (codex M2: the review is *scheduled*, honoring the published "2–5 minutes"; the cascade cannot spin-select it because the due query filters on `integrity_due_at <= now()`). Emits the lifecycle frame. Below threshold → settle immediately (unchanged).
- **Settlement authority from `Resolving` (codex B4 — exactly two paths, enforced under the market lock):** (1) a stored `integrity_reports` row with `verdict='pass'` AND `curator_flagged_at IS NULL`; (2) an explicit `curator_override` when flagged. **No report → `AppError::UnderReview` (423), including direct admin resolve with no body.** A flagged market with `curator_override: None` → `AppError::CuratorRequired` (409). Negative tests for both bypasses codex enumerated: direct resolve after hold before the report exists; resolve-with-None after a flag.
- **The sweep (scheduler) — commit protocol pinned (codex P3R2 B4/M2):** `AdvanceDue` gains a `Resolving && integrity_due_at <= now && not flagged` arm running **two transactions in sequence**: **Tx A** — `vote_stats(m, cfg)` (one SQL aggregation implementing the Task 3.0 definitions) → `domain::integrity::sweep_checks` + `verdict` → insert report `ON CONFLICT (market_id) DO NOTHING` → read the stored row (racing sweeps converge on the winner's verdict) → **COMMIT** (the report must be durably visible before any settlement attempt — `ResolveMarket` opens its own transaction and cannot see an uncommitted report). Then **Tx B** — stored verdict `pass`: invoke `ResolveMarket` normally (its authority read finds the committed report); stored verdict `flag`: `FlagCuratorNeeded` (tightened predicate `status='resolving' AND curator_flagged_at IS NULL`). **`ResolveMarket` returns a typed outcome** — `ResolveOutcome { Settled, HeldForReview { due_at }, CuratorRequired, Voided }` — so `AdvanceDue` increments `held` from the hold path and `swept_pass`/`swept_flag` from the sweep path without inference. `SweepReport` carries `held: u32, swept_pass: u32, swept_flag: u32`.
- **Curator inbox (grok B4):** `GET /admin/markets/flagged` — markets with `curator_flagged_at IS NOT NULL` or `status='resolving'`, joined with their report (checks jsonb included); the existing decision endpoint settles + clears. Curl-usable; web admin UI is out of scope. **Integrity reports are admin-only surfaces — check details never appear on public endpoints, frames, or web copy (grok P3R2: publishing per-check values would hand attackers a calibration oracle); the public sees only `under_review: bool` and the generic copy.**
- REST detail + snapshot gain `under_review: bool`; web copy in 3.4 must say **"automated heuristics, not a guilt finding"** (grok M4).

**Tests:** threshold boundary; held market not re-selected before `integrity_due_at`; sweep pass settles once across racing schedulers (report conflict + resolve key both exercised); flag requires ≥2 check flags (single-flag fixture records pass-with-note and settles); both bypass paths rejected; each check flags in isolation at boundary and passes below; coverage-skip when metadata sparse; all-converse market can pass; XFF spoof from untrusted peer ignored; HMAC applied (raw id absent from the DB — asserted); replayed resolve after sweep uses the stored verdict.

- [x] Failing tests → implement → gates green.

---

### Task 3.3: Leaderboards from realization facts, LP kill-switch, fee summary

**Files:** modify `ports.rs` (queries + realization writes), `place_trade.rs` (sell realization fact), `resolve_market.rs` (settlement/void facts + `lp_pnl_micro`), `seed_market.rs` (kill-switch), pg store, http routes/dto; regenerate `openapi.json` + `api_models.py`; main config.

**Interfaces (P3R1-corrected):**
- **Realization facts are written where PnL realizes** (codex B5): PlaceTrade sell path inserts `(user, market, outcome, 'sell', realized_delta, txn_id)` using the exact position-accounting delta already computed; ResolveMarket settlement inserts one fact per settled non-pool holding — `payout − relieved_cost` — **including zero-payout losers** (negative rows); void facts use source `'void'`. Same transaction as the money movement; `unique(txn_id, user_id, outcome_id)` makes replays no-ops.
- `MarketQueries::top_traders(since, until, limit) -> Vec<TraderRow { handle, realized_pnl_micro, realizations: u32 }>` = `SUM(realized_delta_micro)` over the UTC half-open window from `realizations` — nothing derived from gross payouts, nothing double-counted (sold shares' cost is relieved at sell; later settlement covers only what remained). utoipa description states **"settled/realized PnL in window — not mark-to-market."**
- `MarketQueries::top_voters(since, until, limit) -> Vec<VoterRow { handle, avg_score_bp, markets_scored, tier }>` — **average** score with `markets_scored ≥ leaderboard_min_scored` (grok M1: volume must not beat accuracy), tier ≥ 1 filter, formula in utoipa + the scoring page.
- `ResolveMarket` writes `markets.lp_pnl_micro = pool_payout − seeded_micro` **and `markets.settled_at = now()`** in the settlement tx (codex P3R2 N2); the LP window query is `sum(lp_pnl_micro) over markets where settled_at ∈ [now − window_days, now)` on the partial index — **voids participate** (neutral redemption still realizes the pool's LP result; stated so the breaker sees manipulation-shaped losses however they settle).
- **Kill-switch, race-safe (codex M6):** inside `SeedMarket`'s transaction, after `serialize_key`: advisory lock class 3 (single global key) → `lp_pnl_sum(window)` = sum of `lp_pnl_micro` over markets **resolved** in the window (definition pinned) → if `sum ≤ −max_loss_micro` → `AppError::LpPaused` (503) unless the admin body carries `force: true` (audited outbox event in the same commit). **Stated limitation (grok M3, in this task body and the admin copy): the pause is seed-time-only — already-live markets keep trading; mid-market pause/exposure control is a named Phase 6 control-plane item.**
- `GET /admin/fees/summary?days=30` — daily totals labeled **fees-account revenue** with breakdown by source (`trade` fee entries vs `payout`-txn dust — grouped by `ledger_transactions.kind`; codex M4).
- Routes: `GET /leaderboards/voters|traders?days=7&limit=10`. Regenerate both artifacts (twice + `cmp`).

**Tests:** realization fixtures — sell-then-repurchase (no double count), hold-to-payout only, zero-payout loser (negative row), void market, replayed settlement (unique no-op); leaderboard exact ordered rows incl. window edges + min-scored filter; empty windows; LP PnL for lopsided and balanced markets; two concurrent seeds with tripped switch → both blocked (advisory lock test), force path audited; fee summary matches ledger fixtures with correct source split.

- [x] Failing tests → implement → gates green (incl. artifact reproducibility).

---

### Task 3.4: Web — economy surfaces (parallel; owns `web/` only)

- Leaderboards go live (REST on load — one fetch, no intervals): Top Voters (handle, **avg** weekly score, markets-scored count, tier badge) and Top Traders (handle, weekly **settled** PnL — the label says "settled PnL") with existing empty states.
- Profile chip: tier badge + effective fee line ("Tier 2 · fee 0.90%"). No tier badges on the tape (explicit non-goal).
- Vote calls send `x-device-id` (localStorage id; code comment labels it client-asserted).
- Market detail: `under_review` state copy — "Large pot — automated fairness review before payout, usually a few minutes. Heuristic checks, not a guilt finding." Terminal-state family styling.
- **`docs/copy/scoring.md` is created as the copy source of truth (grok M5)** and the how-it-works page renders/matches it: the exact scoring formula (75/25 split, 25pp accuracy kernel, tie at exactly 50.00% counts both sides), the rep EWMA + quality floor + tier table, the integrity bar (phone link, velocity, young-account friction near close), the review-hold rule (threshold + expected minutes + curator path), and the anti-churn fee rule. Numbers, not vibes.
- Gates: `pnpm build` + `pnpm test` green.
- [x] Web economy surfaces shipped (Task 3.4).

---

### Task 3.5: Phase 3 exit verification

1. Full gates: fmt, clippy `-D warnings`, `cargo test --workspace` (fresh DB, 0001–0005), `just coverage` (**100% × 3 holds**), `just deps-check`, `just gate-test`; converse pytest; web build+test.
2. E2E extensions: (a) normal loop → voter rep moved per golden vector, leaderboards show the demo user with avg-score semantics; (b) high-pot market → `Resolving` hold (WS lifecycle + `under_review`), due-time honored, clean sweep passes → settles; total close→paid time within `sweep_delay + budget`; (c) poisoned fixture (young-account burst from one device near close — trips ≥2 checks) → flag, curator inbox lists it, no payout, curator `void` settles neutrally and clears; (d) tiered user's discounted fee visible in converse preview text and equal in the ledger; flip-window sell charged base fee.
3. Docs sync (no VCS): plan checkboxes, PLAN.md row, README status.

- [x] Exit verification green (2026-08-12): `scripts/e2e_economy.sh` prints `PHASE 3 E2E GREEN` (A–D); fmt/clippy/test(255)/coverage 100%×3 (domain 1326, application 6639, adapters 2491); deps-check; gate-test; converse 33; web 27 vitest + build.

## Deferred from Phase 3 (explicit)

Phone OTP possession-proof (vendor); true ASN clustering (GeoIP is ops config; /24 + /64 prefixes are the stated proxies); strong device attestation (localStorage id is deliberately weak — arrives with the iOS app); mid-market LP pause + dynamic config console (Phase 6, named); user-facing notifications for `RepUpdated`/`CuratorNeeded` (Phase 4 fanout — admin discoverability ships NOW via the flagged list); rep decay for inactivity (needs usage data); voter cash rewards (D5 stands).

## Reviewer checkpoints (R2 verifies specifically)

1. EWMA: pinned K table values exact; round-half-up recurrence vectors; operand validation; no runtime root.
2. Rep batching: canonical-order user locks before market lock — consistent with the trade path's order; 1,000-voter fixture timing honest; replay safe.
3. Fee: one `effective_fee` authority reaching preview/execute/ledger/converse; quote signature change containing `Pool.fee` canonicity; flip-window rule testable as specified.
4. Sweep authority: the two-path settlement rule airtight under the market lock; report-conflict convergence; due-time scheduling; both bypasses dead.
5. Realization facts: uniqueness key right; no path realizes PnL without a fact; leaderboard SQL O(window) on the new index.
6. Kill-switch: class-3 advisory lock scope; window definition; force audit.
7. Signals: trusted-proxy hop rule; HMAC storage; per-check jsonb completeness (value/threshold/coverage/strength/note/config_version).
8. Config validation: fail-fast semantics; every cross-field invariant listed actually checked; no silent default on malformed env.
