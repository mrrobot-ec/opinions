//! W2 admin / inbox / gate helpers. Extends the shared [`ComplianceTx`]
//! without replacing W3's credit/deposit types in `mod.rs`.

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::model::{AdminAction, AdminContext, AdminRole, UserId, UserStatus};
use crate::ports::ScreenVerdict;

use super::kyc::kyc_meets;
use super::phone_verification::PhoneVerificationRow;
use super::self_exclusion::{SelfExclusion, UserDepositLimit};
use super::statuses::{homogeneous_cap_exceeded, parse_user_status, ShadowCaps};
use super::{allows_progress, AmlFlag, ComplianceStore, ComplianceTx, UserComplianceRow};

/// Dual-control confirmation window after `confirm_not_before`.
pub const PROPOSAL_CONFIRM_WINDOW: time::Duration = time::Duration::minutes(15);
/// Ban / unban delay.
pub const BAN_DELAY: time::Duration = time::Duration::minutes(15);
/// Frozen-funds license delay.
pub const FROZEN_FUNDS_DELAY: time::Duration = time::Duration::hours(24);
/// User deposit-limit raise delay (D34).
pub const DEPOSIT_LIMIT_RAISE_DELAY: time::Duration = time::Duration::hours(24);

/// Money mutation the gate is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateMutation {
    SignupGrant,
    DepositAdmission { amount_micro: i64 },
    PlaceTrade { amount_micro: i64 },
    CastVote,
    WithdrawRequest { dest: String, amount_micro: i64 },
    SendWithdrawal { dest: String },
    FirstTrade { amount_micro: i64 },
}

/// Fail-closed gate result. Shadow never appears as its own code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    AllowWithReview,
    Refuse(RefuseCode),
}

/// Public refuse vocabulary. No `ShadowLimited` variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefuseCode {
    AccountNotPermitted,
    SelfExclusion,
    NotEligible,
    CapExceeded { cap_micro: i64, tier: u8 },
    DepositLimit,
    FreezeCounselOnly,
    SettledDestOnly,
}

/// Egress split (D34).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressDisposition {
    FreezeCounselLicenseOnly,
    SourceLockedRefund,
    SettledDestOnly,
    Ordinary,
}

/// Snapshot the call-site owner assembles under `lock_user`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserComplianceSnapshot {
    pub user: UserId,
    pub status: UserStatus,
    pub kyc_tier: i32,
    pub kyc: ScreenVerdict,
    pub sanctions: ScreenVerdict,
    pub geo: ScreenVerdict,
    pub self_exclusion: Option<SelfExclusion>,
    pub open_aml: bool,
    pub deposit_limit_micro: Option<i64>,
    pub settled_dests: Vec<String>,
    pub required_kyc_tier: i32,
    pub shadow: ShadowCaps,
}

/// Durable webhook inbox row (`unique(provider, event_id)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRecord {
    pub provider: String,
    pub event_id: String,
    pub payload_hash: String,
    pub payload: Value,
    pub user_id: Option<UserId>,
    pub received_at: OffsetDateTime,
}

/// Same-key inbox algebra (D33).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxOutcome {
    Accepted,
    Replay,
    Conflict,
}

/// Generic dual-control money command (0011 `money_command_proposals`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoneyProposal {
    pub id: Uuid,
    pub kind: String,
    pub subject_id: Uuid,
    pub payload_hash: String,
    pub proposer_token_id: String,
    pub confirmer_token_id: Option<String>,
    pub reason: String,
    pub status: ProposalStatus,
    pub confirm_not_before: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub replay_key: String,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalStatus {
    Pending,
    Confirmed,
    Rejected,
    Expired,
}

/// Frozen-funds license fact (counsel-shaped dest; never user-picked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenFundsLicense {
    pub id: Uuid,
    pub user: UserId,
    pub dest: String,
    pub amount_micro: i64,
}

/// SHA-256 hex of raw webhook bytes.
#[must_use]
pub fn payload_hash(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

/// Lowercase hex of a byte slice.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(*byte >> 4)]),
                char::from(HEX[usize::from(*byte & 0x0f)]),
            ]
        })
        .collect()
}

/// Same-key inbox algebra: missing → accept; same hash → original; else conflict.
#[must_use]
pub fn inbox_algebra(existing: Option<&InboxRecord>, incoming: &InboxRecord) -> InboxOutcome {
    match existing {
        None => InboxOutcome::Accepted,
        Some(prior) if prior.payload_hash == incoming.payload_hash => InboxOutcome::Replay,
        Some(_) => InboxOutcome::Conflict,
    }
}

/// The three call-site facts the egress split needs beyond status and the
/// sanctions verdict. Named rather than positional: four bare booleans at a
/// call site is how a refund silently becomes a withdrawal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressFacts {
    pub self_excluded: bool,
    pub admission_refused: bool,
    /// Both geo AND sanctions currently fresh Clear.
    pub screens_clear: bool,
}

/// Egress split table (D34).
#[must_use]
pub fn egress_disposition(
    status: UserStatus,
    sanctions: &ScreenVerdict,
    facts: EgressFacts,
) -> EgressDisposition {
    if status == UserStatus::Banned || matches!(sanctions, ScreenVerdict::Hit) {
        return EgressDisposition::FreezeCounselLicenseOnly;
    }
    if facts.admission_refused && facts.screens_clear {
        return EgressDisposition::SourceLockedRefund;
    }
    if facts.self_excluded && facts.screens_clear {
        return EgressDisposition::SettledDestOnly;
    }
    EgressDisposition::Ordinary
}

/// Call-site hook: evaluate a locked snapshot.
#[must_use]
pub fn enforce(
    mutation: &GateMutation,
    snap: &UserComplianceSnapshot,
    now: OffsetDateTime,
) -> GateDecision {
    if snap.status == UserStatus::Banned {
        return GateDecision::Refuse(RefuseCode::AccountNotPermitted);
    }
    if let Some(ex) = &snap.self_exclusion {
        if ex.is_active(now) {
            return enforce_self_excluded(mutation, snap);
        }
    }
    if !allows_progress(&snap.geo, now) || !allows_progress(&snap.kyc, now) {
        return GateDecision::Refuse(RefuseCode::NotEligible);
    }
    if matches!(snap.sanctions, ScreenVerdict::Hit) {
        return GateDecision::Refuse(RefuseCode::AccountNotPermitted);
    }
    if !allows_progress(&snap.sanctions, now) {
        return GateDecision::Refuse(RefuseCode::NotEligible);
    }
    if !kyc_meets(snap.kyc_tier, snap.required_kyc_tier) {
        return GateDecision::Refuse(RefuseCode::NotEligible);
    }
    match mutation {
        GateMutation::SignupGrant | GateMutation::CastVote => GateDecision::Allow,
        GateMutation::DepositAdmission { amount_micro } => {
            enforce_amount(*amount_micro, snap, true)
        }
        // D33 asks for sanctions "additionally at first trade"; the fresh
        // sanctions Clear above already gates every mutation, so FirstTrade
        // needs no second, unreachable check of the same predicate.
        GateMutation::PlaceTrade { amount_micro } | GateMutation::FirstTrade { amount_micro } => {
            enforce_amount(*amount_micro, snap, false)
        }
        GateMutation::WithdrawRequest { dest, amount_micro } => {
            let _ = (dest, amount_micro);
            if snap.open_aml || snap.status == UserStatus::ShadowLimited {
                GateDecision::AllowWithReview
            } else {
                GateDecision::Allow
            }
        }
        GateMutation::SendWithdrawal { dest } => {
            let _ = dest;
            if snap.open_aml {
                GateDecision::Refuse(RefuseCode::NotEligible)
            } else {
                GateDecision::Allow
            }
        }
    }
}

fn enforce_self_excluded(mutation: &GateMutation, snap: &UserComplianceSnapshot) -> GateDecision {
    match mutation {
        GateMutation::WithdrawRequest { dest, .. } | GateMutation::SendWithdrawal { dest } => {
            if snap.settled_dests.iter().any(|known| known == dest) {
                GateDecision::AllowWithReview
            } else {
                GateDecision::Refuse(RefuseCode::SettledDestOnly)
            }
        }
        _ => GateDecision::Refuse(RefuseCode::SelfExclusion),
    }
}

fn enforce_amount(amount_micro: i64, snap: &UserComplianceSnapshot, deposit: bool) -> GateDecision {
    if deposit {
        if let Some(limit) = snap.deposit_limit_micro {
            if amount_micro > limit {
                return GateDecision::Refuse(RefuseCode::DepositLimit);
            }
        }
    }
    if snap.status == UserStatus::ShadowLimited {
        let cap = if deposit {
            snap.shadow.deposit_cap_micro
        } else {
            snap.shadow.trade_cap_micro
        };
        if amount_micro > cap {
            let (cap_micro, tier) = homogeneous_cap_exceeded(cap);
            return GateDecision::Refuse(RefuseCode::CapExceeded { cap_micro, tier });
        }
    }
    GateDecision::Allow
}

/// Map a refuse code onto existing [`AppError`] vocabulary.
#[must_use]
pub fn refuse_to_app_error(code: &RefuseCode) -> AppError {
    match code {
        RefuseCode::AccountNotPermitted | RefuseCode::FreezeCounselOnly => {
            AppError::MoneyForbidden("account is not permitted")
        }
        RefuseCode::SelfExclusion | RefuseCode::SettledDestOnly => {
            AppError::MoneyForbidden("self-exclusion is in effect")
        }
        RefuseCode::NotEligible | RefuseCode::DepositLimit => {
            AppError::MoneyForbidden("not eligible")
        }
        RefuseCode::CapExceeded { cap_micro, tier } => AppError::PositionCapExceeded {
            cap_micro: *cap_micro,
            tier: *tier,
        },
    }
}

/// Extract the authenticated admin principal or refuse.
///
/// # Errors
/// Machine actors cannot run dual-control or single-ops money commands.
pub fn require_admin(actor: &AdminContext) -> Result<(&str, AdminRole), AppError> {
    match actor {
        AdminContext::Admin { token_digest, role } => Ok((token_digest.as_str(), *role)),
        AdminContext::Machine => Err(AppError::AdminForbidden(
            "compliance commands require an admin actor",
        )),
    }
}

/// Role gate used by W2 command handlers.
///
/// # Errors
/// Unlisted role.
pub fn require_role(role: AdminRole, allowed: &[AdminRole]) -> Result<(), AppError> {
    if allowed.contains(&role) {
        Ok(())
    } else {
        Err(AppError::AdminForbidden(
            "role is not permitted for this compliance command",
        ))
    }
}

/// Distinct-token check for confirmers.
///
/// # Errors
/// Same principal confirming their own proposal.
pub fn require_distinct_tokens(proposer: &str, confirmer: &str) -> Result<(), AppError> {
    if proposer == confirmer {
        Err(AppError::AdminForbidden(
            "confirmer token must be distinct from proposer",
        ))
    } else {
        Ok(())
    }
}

/// Build a pending proposal with the matrix delay + 15-minute window.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn new_proposal(
    kind: &str,
    subject_id: Uuid,
    payload_hash: String,
    proposer_token_id: String,
    reason: String,
    replay_key: String,
    delay: time::Duration,
    now: OffsetDateTime,
) -> MoneyProposal {
    let confirm_not_before = now + delay;
    MoneyProposal {
        id: Uuid::new_v4(),
        kind: kind.to_string(),
        subject_id,
        payload_hash,
        proposer_token_id,
        confirmer_token_id: None,
        reason,
        status: ProposalStatus::Pending,
        confirm_not_before,
        expires_at: confirm_not_before + PROPOSAL_CONFIRM_WINDOW,
        replay_key,
        created_at: now,
    }
}

/// Stable replay key for ban/unban (`user id + epoch`).
#[must_use]
pub fn status_replay_key(kind: &str, user: UserId, epoch: i64) -> String {
    format!("{kind}:{user}:{epoch}", user = user.0)
}

/// Extra persistence the admin / phone / inbox paths need.
#[async_trait]
pub trait ComplianceAdminTx: ComplianceTx {
    async fn get_aml_flag(&mut self, id: Uuid) -> Result<AmlFlag, StoreError>;
    async fn clear_aml_flag(&mut self, id: Uuid) -> Result<AmlFlag, StoreError>;
    async fn set_user_status(
        &mut self,
        user: UserId,
        status: UserStatus,
    ) -> Result<UserStatus, StoreError>;
    async fn insert_self_exclusion(
        &mut self,
        exclusion: SelfExclusion,
    ) -> Result<SelfExclusion, StoreError>;
    async fn active_self_exclusion(
        &mut self,
        user: UserId,
    ) -> Result<Option<SelfExclusion>, StoreError>;
    async fn get_self_exclusion(&mut self, id: Uuid) -> Result<SelfExclusion, StoreError>;
    async fn lift_self_exclusion(
        &mut self,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<SelfExclusion, StoreError>;
    async fn get_deposit_limit(
        &mut self,
        user: UserId,
    ) -> Result<Option<UserDepositLimit>, StoreError>;
    async fn upsert_deposit_limit(&mut self, limit: UserDepositLimit) -> Result<(), StoreError>;
    async fn insert_phone_challenge(
        &mut self,
        row: PhoneVerificationRow,
    ) -> Result<PhoneVerificationRow, StoreError>;
    async fn active_phone_challenge(
        &mut self,
        user: UserId,
    ) -> Result<Option<PhoneVerificationRow>, StoreError>;
    async fn phone_by_hmac(
        &mut self,
        hmac: &str,
        key_version: i32,
    ) -> Result<Option<PhoneVerificationRow>, StoreError>;
    /// Re-arm this account's own unverified challenge in place (new code,
    /// new horizon, attempts back to zero).
    async fn refresh_phone_challenge(
        &mut self,
        id: Uuid,
        challenge: &str,
        expires_at: OffsetDateTime,
    ) -> Result<PhoneVerificationRow, StoreError>;
    async fn consume_phone_attempt(&mut self, id: Uuid)
        -> Result<PhoneVerificationRow, StoreError>;
    async fn mark_phone_verified(
        &mut self,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<PhoneVerificationRow, StoreError>;
    async fn verified_phone_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Option<PhoneVerificationRow>, StoreError>;
    async fn inbox_get(
        &mut self,
        provider: &str,
        event_id: &str,
    ) -> Result<Option<InboxRecord>, StoreError>;
    async fn inbox_insert(&mut self, record: InboxRecord) -> Result<InboxRecord, StoreError>;
    async fn insert_proposal(
        &mut self,
        proposal: MoneyProposal,
    ) -> Result<MoneyProposal, StoreError>;
    async fn get_proposal_by_replay(
        &mut self,
        replay_key: &str,
    ) -> Result<Option<MoneyProposal>, StoreError>;
    async fn get_proposal(&mut self, id: Uuid) -> Result<MoneyProposal, StoreError>;
    async fn confirm_proposal(
        &mut self,
        id: Uuid,
        confirmer: &str,
        now: OffsetDateTime,
    ) -> Result<MoneyProposal, StoreError>;
    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError>;
    async fn settled_dests(&mut self, user: UserId) -> Result<Vec<String>, StoreError>;
    async fn record_settled_dest(&mut self, user: UserId, dest: String) -> Result<(), StoreError>;
    async fn config_i64(&mut self, key: &str) -> Result<Option<i64>, StoreError>;
    async fn config_json(&mut self, key: &str) -> Result<Option<Value>, StoreError>;
    async fn set_config_i64(&mut self, key: &str, value: i64) -> Result<(), StoreError>;
    async fn set_config_json(&mut self, key: &str, value: Value) -> Result<(), StoreError>;
    async fn insert_frozen_license(
        &mut self,
        license: FrozenFundsLicense,
    ) -> Result<FrozenFundsLicense, StoreError>;
    async fn get_frozen_license(&mut self, id: Uuid) -> Result<FrozenFundsLicense, StoreError>;
}

/// Factory for the admin extension.
#[async_trait]
pub trait ComplianceAdminStore: ComplianceStore {
    async fn admin_tx(&self) -> Result<Box<dyn ComplianceAdminTx + '_>, StoreError>;
}

/// Accept an authenticated inbox record (user already resolved from OUR rows).
///
/// # Errors
/// Conflict on same key + different hash; store failures.
pub async fn accept_inbox(
    store: &impl ComplianceAdminStore,
    incoming: InboxRecord,
) -> Result<InboxOutcome, AppError> {
    let mut tx = store.admin_tx().await?;
    let existing = tx.inbox_get(&incoming.provider, &incoming.event_id).await?;
    match inbox_algebra(existing.as_ref(), &incoming) {
        InboxOutcome::Accepted => {
            tx.inbox_insert(incoming).await?;
            tx.commit().await?;
            Ok(InboxOutcome::Accepted)
        }
        InboxOutcome::Replay => {
            tx.commit().await?;
            Ok(InboxOutcome::Replay)
        }
        InboxOutcome::Conflict => {
            tx.commit().await?;
            Err(AppError::ProposalConflict("inbox payload hash conflict"))
        }
    }
}

/// Convenience: parse the locked row's status string.
///
/// # Errors
/// Unknown status label.
pub fn row_status(row: &UserComplianceRow) -> Result<UserStatus, StoreError> {
    parse_user_status(&row.status)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::FakeComplianceStore;
    use crate::money::PUBLISHED_TIER0_CAP_MICRO;
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(10_000)
    }

    fn clear(now: OffsetDateTime) -> ScreenVerdict {
        ScreenVerdict::Clear {
            checked_at: now,
            expires_at: now + Duration::hours(1),
            policy_version: "1".into(),
        }
    }

    fn snap(status: UserStatus) -> UserComplianceSnapshot {
        let now = t0();
        UserComplianceSnapshot {
            user: UserId(Uuid::nil()),
            status,
            kyc_tier: 2,
            kyc: clear(now),
            sanctions: clear(now),
            geo: clear(now),
            self_exclusion: None,
            open_aml: false,
            deposit_limit_micro: None,
            settled_dests: vec!["settled-dest".into()],
            required_kyc_tier: 1,
            shadow: ShadowCaps {
                trade_cap_micro: PUBLISHED_TIER0_CAP_MICRO,
                deposit_cap_micro: PUBLISHED_TIER0_CAP_MICRO,
            },
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn hex_inbox_egress_and_gate() {
        assert_eq!(hex_encode(&[0x0f, 0xa0]), "0fa0");
        assert_ne!(payload_hash(b"abc"), payload_hash(b"abd"));
        let rec = InboxRecord {
            provider: "persona".into(),
            event_id: "e1".into(),
            payload_hash: payload_hash(b"body"),
            payload: Value::Null,
            user_id: None,
            received_at: t0(),
        };
        assert_eq!(inbox_algebra(None, &rec), InboxOutcome::Accepted);
        assert_eq!(inbox_algebra(Some(&rec), &rec), InboxOutcome::Replay);
        let mut other = rec.clone();
        other.payload_hash = payload_hash(b"other");
        assert_eq!(inbox_algebra(Some(&rec), &other), InboxOutcome::Conflict);

        let now = t0();
        let facts = |self_excluded, admission_refused, screens_clear| EgressFacts {
            self_excluded,
            admission_refused,
            screens_clear,
        };
        assert_eq!(
            egress_disposition(UserStatus::Banned, &clear(now), facts(false, false, true)),
            EgressDisposition::FreezeCounselLicenseOnly
        );
        assert_eq!(
            egress_disposition(
                UserStatus::Active,
                &ScreenVerdict::Hit,
                facts(false, true, true)
            ),
            EgressDisposition::FreezeCounselLicenseOnly
        );
        assert_eq!(
            egress_disposition(UserStatus::Active, &clear(now), facts(false, true, true)),
            EgressDisposition::SourceLockedRefund
        );
        assert_eq!(
            egress_disposition(UserStatus::Active, &clear(now), facts(true, false, true)),
            EgressDisposition::SettledDestOnly
        );
        assert_eq!(
            egress_disposition(UserStatus::Active, &clear(now), facts(true, false, false)),
            EgressDisposition::Ordinary
        );

        assert_eq!(
            enforce(&GateMutation::CastVote, &snap(UserStatus::Banned), now),
            GateDecision::Refuse(RefuseCode::AccountNotPermitted)
        );
        let mut excluded = snap(UserStatus::Active);
        excluded.self_exclusion = Some(SelfExclusion {
            id: Uuid::nil(),
            user: UserId(Uuid::nil()),
            starts_at: now - Duration::hours(1),
            cooling_off_until: now + Duration::hours(24),
            lifted_at: None,
        });
        assert_eq!(
            enforce(
                &GateMutation::PlaceTrade { amount_micro: 1 },
                &excluded,
                now
            ),
            GateDecision::Refuse(RefuseCode::SelfExclusion)
        );
        assert_eq!(
            enforce(
                &GateMutation::WithdrawRequest {
                    dest: "new".into(),
                    amount_micro: 1,
                },
                &excluded,
                now
            ),
            GateDecision::Refuse(RefuseCode::SettledDestOnly)
        );
        assert!(matches!(
            enforce(
                &GateMutation::SendWithdrawal {
                    dest: "settled-dest".into(),
                },
                &excluded,
                now
            ),
            GateDecision::AllowWithReview
        ));
        let mut no_geo = snap(UserStatus::Active);
        no_geo.geo = ScreenVerdict::Indeterminate;
        assert_eq!(
            enforce(&GateMutation::SignupGrant, &no_geo, now),
            GateDecision::Refuse(RefuseCode::NotEligible)
        );
        let mut hit = snap(UserStatus::Active);
        hit.sanctions = ScreenVerdict::Hit;
        assert_eq!(
            enforce(&GateMutation::PlaceTrade { amount_micro: 1 }, &hit, now),
            GateDecision::Refuse(RefuseCode::AccountNotPermitted)
        );
        let mut stale = snap(UserStatus::Active);
        stale.sanctions = ScreenVerdict::Indeterminate;
        assert_eq!(
            enforce(&GateMutation::CastVote, &stale, now),
            GateDecision::Refuse(RefuseCode::NotEligible)
        );
        let mut low = snap(UserStatus::Active);
        low.kyc_tier = 0;
        assert_eq!(
            enforce(
                &GateMutation::DepositAdmission { amount_micro: 1 },
                &low,
                now
            ),
            GateDecision::Refuse(RefuseCode::NotEligible)
        );
        assert_eq!(
            enforce(&GateMutation::SignupGrant, &snap(UserStatus::Active), now),
            GateDecision::Allow
        );
        let mut limited = snap(UserStatus::Active);
        limited.deposit_limit_micro = Some(10);
        assert_eq!(
            enforce(
                &GateMutation::DepositAdmission { amount_micro: 11 },
                &limited,
                now
            ),
            GateDecision::Refuse(RefuseCode::DepositLimit)
        );
        let shadow = snap(UserStatus::ShadowLimited);
        assert_eq!(
            enforce(
                &GateMutation::PlaceTrade {
                    amount_micro: PUBLISHED_TIER0_CAP_MICRO + 1,
                },
                &shadow,
                now
            ),
            GateDecision::Refuse(RefuseCode::CapExceeded {
                cap_micro: PUBLISHED_TIER0_CAP_MICRO,
                tier: 0,
            })
        );
        assert_eq!(
            enforce(
                &GateMutation::DepositAdmission {
                    amount_micro: PUBLISHED_TIER0_CAP_MICRO + 1,
                },
                &shadow,
                now
            ),
            GateDecision::Refuse(RefuseCode::CapExceeded {
                cap_micro: PUBLISHED_TIER0_CAP_MICRO,
                tier: 0,
            })
        );
        assert_eq!(
            enforce(
                &GateMutation::PlaceTrade {
                    amount_micro: PUBLISHED_TIER0_CAP_MICRO,
                },
                &shadow,
                now
            ),
            GateDecision::Allow
        );
        assert_eq!(
            enforce(
                &GateMutation::WithdrawRequest {
                    dest: "x".into(),
                    amount_micro: 1,
                },
                &shadow,
                now
            ),
            GateDecision::AllowWithReview
        );
        let mut aml = snap(UserStatus::Active);
        aml.open_aml = true;
        assert_eq!(
            enforce(
                &GateMutation::SendWithdrawal { dest: "x".into() },
                &aml,
                now
            ),
            GateDecision::Refuse(RefuseCode::NotEligible)
        );
        assert_eq!(
            enforce(
                &GateMutation::SendWithdrawal { dest: "x".into() },
                &snap(UserStatus::Active),
                now
            ),
            GateDecision::Allow
        );
        assert_eq!(
            enforce(
                &GateMutation::FirstTrade { amount_micro: 1 },
                &snap(UserStatus::Active),
                now
            ),
            GateDecision::Allow
        );
        let mut first_stale = snap(UserStatus::Active);
        first_stale.sanctions = ScreenVerdict::Clear {
            checked_at: now - Duration::hours(2),
            expires_at: now,
            policy_version: "1".into(),
        };
        assert_eq!(
            enforce(
                &GateMutation::FirstTrade { amount_micro: 1 },
                &first_stale,
                now
            ),
            GateDecision::Refuse(RefuseCode::NotEligible)
        );
        let mut stale_kyc = snap(UserStatus::Active);
        stale_kyc.kyc = ScreenVerdict::Indeterminate;
        assert_eq!(
            enforce(&GateMutation::SignupGrant, &stale_kyc, now),
            GateDecision::Refuse(RefuseCode::NotEligible)
        );
        assert_eq!(
            enforce(
                &GateMutation::DepositAdmission { amount_micro: 10 },
                &limited,
                now
            ),
            GateDecision::Allow
        );
        assert_eq!(
            enforce(&GateMutation::CastVote, &snap(UserStatus::Active), now),
            GateDecision::Allow
        );
        assert_eq!(
            enforce(
                &GateMutation::WithdrawRequest {
                    dest: "settled-dest".into(),
                    amount_micro: 1,
                },
                &excluded,
                now
            ),
            GateDecision::AllowWithReview
        );
        assert_eq!(
            enforce(
                &GateMutation::WithdrawRequest {
                    dest: "x".into(),
                    amount_micro: 1,
                },
                &aml,
                now
            ),
            GateDecision::AllowWithReview
        );
    }

    #[test]
    fn a_lifted_exclusion_no_longer_gates_and_a_clean_withdrawal_is_plain_allow() {
        let now = t0();
        let mut snap = snap(UserStatus::Active);
        snap.self_exclusion = Some(SelfExclusion {
            id: Uuid::nil(),
            user: snap.user,
            starts_at: now - Duration::hours(48),
            cooling_off_until: now - Duration::hours(24),
            lifted_at: Some(now - Duration::hours(1)),
        });
        assert_eq!(
            enforce(
                &GateMutation::WithdrawRequest {
                    dest: "settled-dest".into(),
                    amount_micro: 1_000_000,
                },
                &snap,
                now
            ),
            GateDecision::Allow
        );
        assert_eq!(
            enforce(&GateMutation::CastVote, &snap, now),
            GateDecision::Allow
        );
    }

    #[test]
    fn refuse_codes_never_say_shadow() {
        for err in [
            refuse_to_app_error(&RefuseCode::AccountNotPermitted),
            refuse_to_app_error(&RefuseCode::SelfExclusion),
            refuse_to_app_error(&RefuseCode::NotEligible),
            refuse_to_app_error(&RefuseCode::CapExceeded {
                cap_micro: PUBLISHED_TIER0_CAP_MICRO,
                tier: 0,
            }),
            refuse_to_app_error(&RefuseCode::DepositLimit),
            refuse_to_app_error(&RefuseCode::FreezeCounselOnly),
            refuse_to_app_error(&RefuseCode::SettledDestOnly),
        ] {
            let text = format!("{err:?}{err}");
            assert!(!text.to_ascii_lowercase().contains("shadow"), "{text}");
        }
    }

    #[test]
    fn admin_helpers() {
        assert!(require_admin(&AdminContext::Machine).is_err());
        let actor = AdminContext::Admin {
            token_digest: "aa".into(),
            role: AdminRole::Ops,
        };
        assert_eq!(require_admin(&actor).unwrap().1, AdminRole::Ops);
        assert!(require_role(AdminRole::Ops, &[AdminRole::Ops]).is_ok());
        assert!(require_role(AdminRole::Finance, &[AdminRole::Ops]).is_err());
        assert!(require_distinct_tokens("a", "b").is_ok());
        assert!(require_distinct_tokens("a", "a").is_err());
        let p = new_proposal(
            "ban_user",
            Uuid::nil(),
            "h".into(),
            "tok".into(),
            "r".into(),
            status_replay_key("user_status", UserId(Uuid::nil()), 1),
            BAN_DELAY,
            t0(),
        );
        assert_eq!(p.status, ProposalStatus::Pending);
        assert_eq!(p.expires_at, t0() + BAN_DELAY + PROPOSAL_CONFIRM_WINDOW);
        assert_ne!(ProposalStatus::Rejected, ProposalStatus::Expired);
        assert_ne!(InboxOutcome::Accepted, InboxOutcome::Conflict);
        let license = FrozenFundsLicense {
            id: Uuid::nil(),
            user: UserId(Uuid::nil()),
            dest: "counsel".into(),
            amount_micro: 1,
        };
        assert_eq!(license.dest, "counsel");
        assert_eq!(
            row_status(&UserComplianceRow {
                user: UserId(Uuid::nil()),
                kyc_tier: 0,
                status: "active".into(),
            })
            .unwrap(),
            UserStatus::Active
        );
        assert!(row_status(&UserComplianceRow {
            user: UserId(Uuid::nil()),
            kyc_tier: 0,
            status: "nope".into(),
        })
        .is_err());
    }

    #[tokio::test]
    async fn inbox_accept_replay_conflict() {
        let store = FakeComplianceStore::new();
        let rec = InboxRecord {
            provider: "persona".into(),
            event_id: "evt-1".into(),
            payload_hash: payload_hash(b"body"),
            payload: Value::Null,
            user_id: None,
            received_at: t0(),
        };
        assert_eq!(
            accept_inbox(&store, rec.clone()).await.unwrap(),
            InboxOutcome::Accepted
        );
        assert_eq!(
            accept_inbox(&store, rec.clone()).await.unwrap(),
            InboxOutcome::Replay
        );
        let mut other = rec;
        other.payload_hash = payload_hash(b"other");
        assert!(accept_inbox(&store, other).await.is_err());
    }
}
