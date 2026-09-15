//! Postgres `ComplianceTx` / `ComplianceAdminTx` (W2).

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use application::error::StoreError;
use application::model::{AdminAction, UserId, UserStatus};
use application::money::admin::{
    ComplianceAdminStore, ComplianceAdminTx, FrozenFundsLicense, InboxRecord, MoneyProposal,
    ProposalStatus,
};
use application::money::aml::{AmlDirection, AmlFlag, AmlKind, AmlLeg};
use application::money::kyc::KycEvent;
use application::money::phone_verification::{PhoneVerificationRow, PHONE_MAX_ATTEMPTS};
use application::money::sanctions::SanctionScreening;
use application::money::self_exclusion::{SelfExclusion, UserDepositLimit};
use application::money::statuses::status_name;
use application::money::{ComplianceDecision, ComplianceStore, ComplianceTx, UserComplianceRow};
use application::ports::{Committable, ScreenVerdict};

use super::rows::{db_error, unique_violation};
use super::store::PgStore;

/// Pool wrapper; does not edit frozen `PgStore` factories.
#[derive(Clone)]
pub struct PgComplianceStore {
    pool: PgPool,
}

impl PgComplianceStore {
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn from_store(store: &PgStore) -> Self {
        Self {
            pool: store.pool_handle().clone(),
        }
    }
}

struct PgComplianceTx {
    tx: Transaction<'static, Postgres>,
}

#[async_trait]
impl ComplianceStore for PgComplianceStore {
    async fn compliance_tx(&self) -> Result<Box<dyn ComplianceTx + '_>, StoreError> {
        Ok(Box::new(PgComplianceTx {
            tx: self.pool.begin().await.map_err(db_error)?,
        }))
    }
}

#[async_trait]
impl ComplianceAdminStore for PgComplianceStore {
    async fn admin_tx(&self) -> Result<Box<dyn ComplianceAdminTx + '_>, StoreError> {
        Ok(Box::new(PgComplianceTx {
            tx: self.pool.begin().await.map_err(db_error)?,
        }))
    }
}

/// One place where a write's unique-index collision becomes a typed
/// conflict and every other database failure stays a backend error.
fn conflict_or(error: sqlx::Error, message: &'static str) -> StoreError {
    if unique_violation(&error) {
        StoreError::Conflict(message)
    } else {
        db_error(error)
    }
}

/// The single `money_command_proposals` authority (matrix / codex-p7r3 B4),
/// written once so a money-effect transaction can CAS a proposal and commit
/// its economic effect atomically instead of opening a second transaction.
pub(super) async fn proposal_insert(
    tx: &mut Transaction<'static, Postgres>,
    proposal: MoneyProposal,
) -> Result<MoneyProposal, StoreError> {
    sqlx::query(
        "insert into money_command_proposals
            (id, kind, subject_id, payload_hash, proposer_token_id, confirmer_token_id,
             reason, status, confirm_not_before, expires_at, replay_key, created_at)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(proposal.id)
    .bind(&proposal.kind)
    .bind(proposal.subject_id)
    .bind(&proposal.payload_hash)
    .bind(&proposal.proposer_token_id)
    .bind(&proposal.confirmer_token_id)
    .bind(&proposal.reason)
    .bind(proposal_status_name(proposal.status))
    .bind(proposal.confirm_not_before)
    .bind(proposal.expires_at)
    .bind(&proposal.replay_key)
    .bind(proposal.created_at)
    .execute(&mut **tx)
    .await
    .map_err(|error| conflict_or(error, "proposal replay"))?;
    Ok(proposal)
}

const PROPOSAL_COLUMNS: &str =
    "id, kind, subject_id, payload_hash, proposer_token_id, confirmer_token_id,
     reason, status, confirm_not_before, expires_at, replay_key, created_at";

pub(super) async fn proposal_by_replay(
    tx: &mut Transaction<'static, Postgres>,
    replay_key: &str,
) -> Result<Option<MoneyProposal>, StoreError> {
    let row = sqlx::query(&format!(
        "select {PROPOSAL_COLUMNS} from money_command_proposals where replay_key = $1"
    ))
    .bind(replay_key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error)?;
    row.map(|row| proposal_from_row(&row)).transpose()
}

pub(super) async fn proposal_by_id(
    tx: &mut Transaction<'static, Postgres>,
    id: Uuid,
) -> Result<MoneyProposal, StoreError> {
    let row = sqlx::query(&format!(
        "select {PROPOSAL_COLUMNS} from money_command_proposals where id = $1"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error)?
    .ok_or(StoreError::NotFound("proposal"))?;
    proposal_from_row(&row)
}

/// Confirmation is a CAS on `pending`: a second confirmer, or a confirm that
/// races a reject/expire, moves nothing and reads back the current row, so
/// the caller's `status != Pending` check refuses it.
pub(super) async fn proposal_confirm(
    tx: &mut Transaction<'static, Postgres>,
    id: Uuid,
    confirmer: &str,
) -> Result<MoneyProposal, StoreError> {
    sqlx::query(
        "update money_command_proposals
            set confirmer_token_id = $2, status = 'confirmed'
          where id = $1 and status = 'pending'",
    )
    .bind(id)
    .bind(confirmer)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    proposal_by_id(tx, id).await
}

fn inbox_subject_id(provider: &str, event_id: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(provider.as_bytes());
    hasher.update([0xff]);
    hasher.update(event_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

fn verdict_label(verdict: &ScreenVerdict) -> &'static str {
    match verdict {
        ScreenVerdict::Clear { .. } => "clear",
        ScreenVerdict::Hit => "hit",
        ScreenVerdict::Indeterminate => "indeterminate",
    }
}

fn verdict_from_row(
    label: &str,
    checked_at: OffsetDateTime,
    expires_at: Option<OffsetDateTime>,
    policy_version: Option<String>,
) -> ScreenVerdict {
    match label {
        "hit" => ScreenVerdict::Hit,
        "clear" => match (expires_at, policy_version) {
            (Some(expires_at), Some(policy_version)) => ScreenVerdict::Clear {
                checked_at,
                expires_at,
                policy_version,
            },
            _ => ScreenVerdict::Indeterminate,
        },
        _ => ScreenVerdict::Indeterminate,
    }
}

fn proposal_status(raw: &str) -> ProposalStatus {
    match raw {
        "confirmed" => ProposalStatus::Confirmed,
        "rejected" => ProposalStatus::Rejected,
        "expired" => ProposalStatus::Expired,
        _ => ProposalStatus::Pending,
    }
}

fn proposal_status_name(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Confirmed => "confirmed",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Expired => "expired",
    }
}

fn proposal_from_row(row: &PgRow) -> Result<MoneyProposal, StoreError> {
    Ok(MoneyProposal {
        id: row.try_get("id").map_err(db_error)?,
        kind: row.try_get("kind").map_err(db_error)?,
        subject_id: row.try_get("subject_id").map_err(db_error)?,
        payload_hash: row.try_get("payload_hash").map_err(db_error)?,
        proposer_token_id: row.try_get("proposer_token_id").map_err(db_error)?,
        confirmer_token_id: row.try_get("confirmer_token_id").map_err(db_error)?,
        reason: row.try_get("reason").map_err(db_error)?,
        status: proposal_status(&row.try_get::<String, _>("status").map_err(db_error)?),
        confirm_not_before: row.try_get("confirm_not_before").map_err(db_error)?,
        expires_at: row.try_get("expires_at").map_err(db_error)?,
        replay_key: row.try_get("replay_key").map_err(db_error)?,
        created_at: row.try_get("created_at").map_err(db_error)?,
    })
}

fn kyc_from_row(row: &PgRow) -> Result<KycEvent, StoreError> {
    let payload: Value = row.try_get("payload").map_err(db_error)?;
    let valid_until = payload
        .get("valid_until")
        .and_then(Value::as_i64)
        .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok());
    let policy_version = payload
        .get("policy_version")
        .and_then(Value::as_str)
        .unwrap_or("1")
        .to_string();
    Ok(KycEvent {
        id: row.try_get("id").map_err(db_error)?,
        user: UserId(row.try_get("user_id").map_err(db_error)?),
        from_tier: row.try_get("from_tier").map_err(db_error)?,
        to_tier: row.try_get("to_tier").map_err(db_error)?,
        provider_ref: row.try_get("provider_ref").map_err(db_error)?,
        at: row.try_get("at").map_err(db_error)?,
        valid_until,
        policy_version,
        payload,
    })
}

fn flag_from_row(row: &PgRow) -> Result<AmlFlag, StoreError> {
    let status: String = row.try_get("status").map_err(db_error)?;
    Ok(AmlFlag {
        id: row.try_get("id").map_err(db_error)?,
        user: UserId(row.try_get("user_id").map_err(db_error)?),
        rule: AmlKind::parse(&row.try_get::<String, _>("rule").map_err(db_error)?)?,
        window_label: row.try_get("window_label").map_err(db_error)?,
        evidence: row.try_get("evidence").map_err(db_error)?,
        open: status == "open",
        at: row.try_get("at").map_err(db_error)?,
    })
}

fn phone_from_row(row: &PgRow) -> Result<PhoneVerificationRow, StoreError> {
    Ok(PhoneVerificationRow {
        id: row.try_get("id").map_err(db_error)?,
        user: UserId(row.try_get("user_id").map_err(db_error)?),
        number_hmac: row.try_get("number_hmac").map_err(db_error)?,
        hmac_key_version: row.try_get("hmac_key_version").map_err(db_error)?,
        challenge: row.try_get("challenge").map_err(db_error)?,
        expires_at: row.try_get("expires_at").map_err(db_error)?,
        attempts: row.try_get("attempts").map_err(db_error)?,
        verified_at: row.try_get("verified_at").map_err(db_error)?,
        provider_ref: row.try_get("provider_ref").map_err(db_error)?,
    })
}

fn aml_leg_from_payload(payload: &Value) -> Option<AmlLeg> {
    Some(AmlLeg {
        id: payload.get("id")?.as_str()?.parse().ok()?,
        user: UserId(payload.get("user")?.as_str()?.parse().ok()?),
        dest: payload.get("dest")?.as_str()?.to_string(),
        amount_micro: payload.get("amount_micro")?.as_i64()?,
        at: OffsetDateTime::from_unix_timestamp(payload.get("at")?.as_i64()?).ok()?,
        direction: match payload.get("direction")?.as_str()? {
            "deposit" => AmlDirection::Deposit,
            _ => AmlDirection::Withdrawal,
        },
    })
}

#[async_trait]
impl ComplianceTx for PgComplianceTx {
    /// Class-1 fence on ONE inbox key, taken before the inbox is read.
    ///
    /// The algebra decides `Accepted` from a read, so without this two
    /// concurrent deliveries of the same `(provider, event_id)` both see
    /// "absent" under READ COMMITTED and both decide to accept. The loser then
    /// collides on `inbox_subject_id`'s primary key and surfaces a raw
    /// `Conflict("inbox event")` — which is wrong twice over: an ordinary
    /// at-least-once duplicate becomes an error the provider must retry, and a
    /// genuine same-key/different-hash divergence never reaches the typed
    /// `ProposalConflict` that the webhook pages on. Locking the user is not
    /// enough: two deliveries claiming one event id may name different users
    /// and so share no row lock at all.
    ///
    /// Class 1 is the existing idempotency namespace (`serialize_key`), and
    /// `kyc-inbox:{provider}:{event_id}` is disjoint from every key used there,
    /// so this composes with the global order (class 1 → class 2 user → …)
    /// instead of introducing a new lock class.
    async fn serialize_inbox(&mut self, provider: &str, event_id: &str) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(1, hashtext($1))")
            .bind(format!("kyc-inbox:{provider}:{event_id}"))
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    /// D33 durable webhook inbox, moved onto `ComplianceTx` so the algebra and
    /// the KYC effect commit in ONE transaction. Committing the inbox row in a
    /// separate transaction from the effect means a crash between them makes
    /// every later retry a permanent `Replay`: the tier is never applied and
    /// the failure is reported to the provider as 200 success
    /// (docs/reviews/codex-p7r1.md:73-75 accepted exactly this coupling).
    ///
    /// Concurrency is decided by [`Self::serialize_inbox`], NOT by the primary
    /// key. The algebra is computed from this read, so the class-1 fence has to
    /// be held before it: otherwise two simultaneous deliveries both read
    /// "absent", both decide `Accepted`, and the loser only discovers it lost
    /// when `inbox_insert` collides — far too late, because by then it has
    /// skipped the algebra. That produced two wrong outcomes: an ordinary
    /// at-least-once duplicate surfaced as an error the provider must retry
    /// instead of `Replay`, and a genuine same-key/different-hash divergence
    /// surfaced as a raw `StoreError::Conflict("inbox event")` instead of the
    /// typed `ProposalConflict` the webhook pages on — so nobody was alerted.
    ///
    /// The deterministic `inbox_subject_id(provider, event_id)` PRIMARY KEY on
    /// `compliance_decisions` stays as defence in depth: it makes a second row
    /// for one event impossible even if a future caller forgets the fence.
    /// `conflict_or` still maps that collision to
    /// `StoreError::Conflict("inbox event")`, which under the fence is now
    /// unreachable rather than routine.
    async fn inbox_get(
        &mut self,
        provider: &str,
        event_id: &str,
    ) -> Result<Option<InboxRecord>, StoreError> {
        let id = inbox_subject_id(provider, event_id);
        let row = sqlx::query(
            "select payload, at from compliance_decisions
              where subject_type = 'inbox' and subject_id = $1 limit 1",
        )
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            let payload: Value = row.try_get("payload").map_err(db_error)?;
            Ok(InboxRecord {
                provider: provider.to_string(),
                event_id: event_id.to_string(),
                payload_hash: payload
                    .get("payload_hash")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                payload: payload.get("body").cloned().unwrap_or(Value::Null),
                user_id: payload
                    .get("user_id")
                    .and_then(Value::as_str)
                    .and_then(|raw| raw.parse().ok())
                    .map(UserId),
                received_at: row.try_get("at").map_err(db_error)?,
            })
        })
        .transpose()
    }

    async fn inbox_insert(&mut self, record: InboxRecord) -> Result<InboxRecord, StoreError> {
        let id = inbox_subject_id(&record.provider, &record.event_id);
        sqlx::query(
            "insert into compliance_decisions (id, subject_type, subject_id, kind, actor, at, payload)
             values ($1,'inbox',$2,$3,'webhook',$4,$5)",
        )
        .bind(id)
        .bind(id)
        .bind(&record.provider)
        .bind(record.received_at)
        .bind(json!({
            "event_id": record.event_id,
            "payload_hash": record.payload_hash,
            "body": record.payload,
            "user_id": record.user_id.map(|u| u.0.to_string()),
        }))
        .execute(&mut *self.tx)
        .await
        .map_err(|error| conflict_or(error, "inbox event"))?;
        Ok(record)
    }

    async fn lock_user(&mut self, user: UserId) -> Result<UserComplianceRow, StoreError> {
        let row = sqlx::query("select id, kyc_tier, status from users where id = $1 for update")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("user"))?;
        Ok(UserComplianceRow {
            user: UserId(row.try_get("id").map_err(db_error)?),
            kyc_tier: row.try_get("kyc_tier").map_err(db_error)?,
            status: row.try_get("status").map_err(db_error)?,
        })
    }

    async fn insert_kyc_event(&mut self, event: KycEvent) -> Result<(), StoreError> {
        let mut payload = event.payload.clone();
        if let Some(until) = event.valid_until {
            payload
                .as_object_mut()
                .map(|obj| obj.insert("valid_until".into(), json!(until.unix_timestamp())));
        }
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("policy_version".into(), json!(event.policy_version));
        }
        sqlx::query(
            "insert into kyc_events (id, user_id, from_tier, to_tier, provider_ref, at, payload)
             values ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(event.id)
        .bind(event.user.0)
        .bind(event.from_tier)
        .bind(event.to_tier)
        .bind(event.provider_ref)
        .bind(event.at)
        .bind(payload)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn set_kyc_tier(&mut self, user: UserId, tier: i32) -> Result<(), StoreError> {
        sqlx::query("update users set kyc_tier = $2 where id = $1")
            .bind(user.0)
            .bind(tier)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn latest_kyc(&mut self, user: UserId) -> Result<Option<KycEvent>, StoreError> {
        let row = sqlx::query(
            "select id, user_id, from_tier, to_tier, provider_ref, at, payload
               from kyc_events where user_id = $1 order by at desc, id desc limit 1",
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| kyc_from_row(&row)).transpose()
    }

    async fn insert_screening(&mut self, screening: SanctionScreening) -> Result<(), StoreError> {
        let (expires, policy) = match &screening.verdict {
            ScreenVerdict::Clear {
                expires_at,
                policy_version,
                ..
            } => (Some(*expires_at), Some(policy_version.clone())),
            _ => (None, None),
        };
        sqlx::query(
            "insert into sanction_screenings
                (id, user_id, context, verdict, raw_ref, checked_at, expires_at, policy_version)
             values ($1,$2,$3,$4,$5, now(), $6,$7)",
        )
        .bind(screening.id)
        .bind(screening.user.0)
        .bind(screening.context)
        .bind(verdict_label(&screening.verdict))
        .bind(screening.raw_ref)
        .bind(expires)
        .bind(policy)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn latest_screening(
        &mut self,
        user: UserId,
        context: &str,
    ) -> Result<Option<SanctionScreening>, StoreError> {
        let row = sqlx::query(
            "select id, user_id, context, verdict, raw_ref, checked_at, expires_at, policy_version
               from sanction_screenings
              where user_id = $1 and context = $2
              order by checked_at desc limit 1",
        )
        .bind(user.0)
        .bind(context)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            let checked_at = row.try_get("checked_at").map_err(db_error)?;
            Ok(SanctionScreening {
                id: row.try_get("id").map_err(db_error)?,
                user: UserId(row.try_get("user_id").map_err(db_error)?),
                context: row.try_get("context").map_err(db_error)?,
                verdict: verdict_from_row(
                    &row.try_get::<String, _>("verdict").map_err(db_error)?,
                    checked_at,
                    row.try_get("expires_at").map_err(db_error)?,
                    row.try_get("policy_version").map_err(db_error)?,
                ),
                raw_ref: row.try_get("raw_ref").map_err(db_error)?,
            })
        })
        .transpose()
    }

    async fn insert_aml_flag(&mut self, flag: AmlFlag) -> Result<(), StoreError> {
        sqlx::query(
            "insert into aml_flags (id, user_id, rule, window_label, evidence, status, at)
             values ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(flag.id)
        .bind(flag.user.0)
        .bind(flag.rule.as_str())
        .bind(flag.window_label)
        .bind(flag.evidence)
        .bind(if flag.open { "open" } else { "cleared" })
        .bind(flag.at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn open_aml_flags(&mut self, user: UserId) -> Result<Vec<AmlFlag>, StoreError> {
        let rows = sqlx::query(
            "select id, user_id, rule, window_label, evidence, status, at
               from aml_flags where user_id = $1 and status = 'open'",
        )
        .bind(user.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter().map(flag_from_row).collect()
    }

    async fn list_aml_legs(
        &mut self,
        user: UserId,
        since: OffsetDateTime,
    ) -> Result<Vec<AmlLeg>, StoreError> {
        self.collect_legs(Some(user), None, since).await
    }

    async fn list_dest_aml_legs(
        &mut self,
        dest: &str,
        since: OffsetDateTime,
    ) -> Result<Vec<AmlLeg>, StoreError> {
        self.collect_legs(None, Some(dest), since).await
    }

    async fn record_aml_leg(&mut self, leg: AmlLeg) -> Result<(), StoreError> {
        sqlx::query(
            "insert into compliance_decisions (id, subject_type, subject_id, kind, actor, at, payload)
             values ($1,'aml_leg',$2,'aml_leg','machine',$3,$4)",
        )
        .bind(leg.id)
        .bind(leg.user.0)
        .bind(leg.at)
        .bind(json!({
            "id": leg.id.to_string(),
            "user": leg.user.0.to_string(),
            "dest": leg.dest,
            "amount_micro": leg.amount_micro,
            "at": leg.at.unix_timestamp(),
            "direction": match leg.direction {
                AmlDirection::Deposit => "deposit",
                AmlDirection::Withdrawal => "withdrawal",
            },
        }))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn insert_decision(&mut self, decision: ComplianceDecision) -> Result<(), StoreError> {
        sqlx::query(
            "insert into compliance_decisions (id, subject_type, subject_id, kind, actor, at, payload)
             values ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(decision.id)
        .bind(decision.subject_type)
        .bind(decision.subject_id)
        .bind(decision.kind)
        .bind(decision.actor)
        .bind(decision.at)
        .bind(decision.payload)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}

impl PgComplianceTx {
    async fn collect_legs(
        &mut self,
        user: Option<UserId>,
        dest: Option<&str>,
        since: OffsetDateTime,
    ) -> Result<Vec<AmlLeg>, StoreError> {
        let mut legs = Vec::new();
        let rows = sqlx::query(
            "select payload from compliance_decisions
              where kind = 'aml_leg' and at >= $1",
        )
        .bind(since)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        for row in rows {
            let payload: Value = row.try_get("payload").map_err(db_error)?;
            if let Some(leg) = aml_leg_from_payload(&payload) {
                if user.is_some_and(|u| leg.user != u) {
                    continue;
                }
                if dest.is_some_and(|d| leg.dest != d) {
                    continue;
                }
                legs.push(leg);
            }
        }
        let deposits = sqlx::query(
            "select id, user_id, coalesce(source_address, 'deposit') as dest, amount_micro, created_at
               from deposits
              where created_at >= $1
                and ($2::uuid is null or user_id = $2)
                and ($3::text is null or coalesce(source_address, 'deposit') = $3)
                and status not in ('refunded')",
        )
        .bind(since)
        .bind(user.map(|u| u.0))
        .bind(dest)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        for row in deposits {
            let Some(uid) = row
                .try_get::<Option<Uuid>, _>("user_id")
                .map_err(db_error)?
            else {
                continue;
            };
            legs.push(AmlLeg {
                id: row.try_get("id").map_err(db_error)?,
                user: UserId(uid),
                dest: row.try_get("dest").map_err(db_error)?,
                amount_micro: row.try_get("amount_micro").map_err(db_error)?,
                at: row.try_get("created_at").map_err(db_error)?,
                direction: AmlDirection::Deposit,
            });
        }
        let withdrawals = sqlx::query(
            "select id, user_id, dest_address, amount_micro, requested_at
               from withdrawals
              where requested_at >= $1
                and ($2::uuid is null or user_id = $2)
                and ($3::text is null or dest_address = $3)
                and status not in ('denied','failed')",
        )
        .bind(since)
        .bind(user.map(|u| u.0))
        .bind(dest)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        for row in withdrawals {
            legs.push(AmlLeg {
                id: row.try_get("id").map_err(db_error)?,
                user: UserId(row.try_get("user_id").map_err(db_error)?),
                dest: row.try_get("dest_address").map_err(db_error)?,
                amount_micro: row.try_get("amount_micro").map_err(db_error)?,
                at: row.try_get("requested_at").map_err(db_error)?,
                direction: AmlDirection::Withdrawal,
            });
        }
        let converts = sqlx::query(
            "select id, user_id, amount_micro, converted_at
               from credit_grant_lots
              where converted_at is not null and converted_at >= $1
                and ($2::uuid is null or user_id = $2)",
        )
        .bind(since)
        .bind(user.map(|u| u.0))
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        for row in converts {
            let id: Uuid = row.try_get("id").map_err(db_error)?;
            // A converted lot's dest is the synthetic `convert:<lot>`; a
            // per-dest query must not collect every unrelated conversion, or
            // conversions alone would push any dest over the structuring N.
            if dest.is_some_and(|d| d != format!("convert:{id}")) {
                continue;
            }
            legs.push(AmlLeg {
                id,
                user: UserId(row.try_get("user_id").map_err(db_error)?),
                dest: format!("convert:{id}"),
                amount_micro: row.try_get("amount_micro").map_err(db_error)?,
                at: row.try_get("converted_at").map_err(db_error)?,
                direction: AmlDirection::Withdrawal,
            });
        }
        Ok(legs)
    }
}

#[async_trait]
impl ComplianceAdminTx for PgComplianceTx {
    async fn get_aml_flag(&mut self, id: Uuid) -> Result<AmlFlag, StoreError> {
        let row = sqlx::query(
            "select id, user_id, rule, window_label, evidence, status, at from aml_flags where id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("aml flag"))?;
        flag_from_row(&row)
    }

    async fn clear_aml_flag(&mut self, id: Uuid) -> Result<AmlFlag, StoreError> {
        sqlx::query("update aml_flags set status = 'cleared' where id = $1")
            .bind(id)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        self.get_aml_flag(id).await
    }

    async fn set_user_status(
        &mut self,
        user: UserId,
        status: UserStatus,
    ) -> Result<UserStatus, StoreError> {
        sqlx::query("update users set status = $2 where id = $1")
            .bind(user.0)
            .bind(status_name(status))
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(status)
    }

    async fn insert_self_exclusion(
        &mut self,
        exclusion: SelfExclusion,
    ) -> Result<SelfExclusion, StoreError> {
        sqlx::query(
            "insert into self_exclusions (id, user_id, starts_at, cooling_off_until, lifted_at)
             values ($1,$2,$3,$4,$5)",
        )
        .bind(exclusion.id)
        .bind(exclusion.user.0)
        .bind(exclusion.starts_at)
        .bind(exclusion.cooling_off_until)
        .bind(exclusion.lifted_at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(exclusion)
    }

    async fn active_self_exclusion(
        &mut self,
        user: UserId,
    ) -> Result<Option<SelfExclusion>, StoreError> {
        let row = sqlx::query(
            "select id, user_id, starts_at, cooling_off_until, lifted_at
               from self_exclusions
              where user_id = $1 and lifted_at is null
              order by starts_at desc limit 1",
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            Ok(SelfExclusion {
                id: row.try_get("id").map_err(db_error)?,
                user: UserId(row.try_get("user_id").map_err(db_error)?),
                starts_at: row.try_get("starts_at").map_err(db_error)?,
                cooling_off_until: row.try_get("cooling_off_until").map_err(db_error)?,
                lifted_at: row.try_get("lifted_at").map_err(db_error)?,
            })
        })
        .transpose()
    }

    async fn get_self_exclusion(&mut self, id: Uuid) -> Result<SelfExclusion, StoreError> {
        let row = sqlx::query(
            "select id, user_id, starts_at, cooling_off_until, lifted_at from self_exclusions where id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("self exclusion"))?;
        Ok(SelfExclusion {
            id: row.try_get("id").map_err(db_error)?,
            user: UserId(row.try_get("user_id").map_err(db_error)?),
            starts_at: row.try_get("starts_at").map_err(db_error)?,
            cooling_off_until: row.try_get("cooling_off_until").map_err(db_error)?,
            lifted_at: row.try_get("lifted_at").map_err(db_error)?,
        })
    }

    async fn lift_self_exclusion(
        &mut self,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<SelfExclusion, StoreError> {
        sqlx::query("update self_exclusions set lifted_at = $2 where id = $1")
            .bind(id)
            .bind(at)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        self.get_self_exclusion(id).await
    }

    async fn get_deposit_limit(
        &mut self,
        user: UserId,
    ) -> Result<Option<UserDepositLimit>, StoreError> {
        let row = sqlx::query(
            "select user_id, limit_micro, pending_limit_micro, pending_effective_at, updated_at
               from user_deposit_limits where user_id = $1",
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            Ok(UserDepositLimit {
                user: UserId(row.try_get("user_id").map_err(db_error)?),
                limit_micro: row.try_get("limit_micro").map_err(db_error)?,
                pending_limit_micro: row.try_get("pending_limit_micro").map_err(db_error)?,
                pending_effective_at: row.try_get("pending_effective_at").map_err(db_error)?,
                updated_at: row.try_get("updated_at").map_err(db_error)?,
            })
        })
        .transpose()
    }

    async fn upsert_deposit_limit(&mut self, limit: UserDepositLimit) -> Result<(), StoreError> {
        sqlx::query(
            "insert into user_deposit_limits
                (user_id, limit_micro, pending_limit_micro, pending_effective_at, updated_at)
             values ($1,$2,$3,$4,$5)
             on conflict (user_id) do update set
                limit_micro = excluded.limit_micro,
                pending_limit_micro = excluded.pending_limit_micro,
                pending_effective_at = excluded.pending_effective_at,
                updated_at = excluded.updated_at",
        )
        .bind(limit.user.0)
        .bind(limit.limit_micro)
        .bind(limit.pending_limit_micro)
        .bind(limit.pending_effective_at)
        .bind(limit.updated_at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn insert_phone_challenge(
        &mut self,
        row: PhoneVerificationRow,
    ) -> Result<PhoneVerificationRow, StoreError> {
        sqlx::query(
            "insert into phone_verifications
                (id, user_id, number_hmac, hmac_key_version, challenge, expires_at, attempts, verified_at, provider_ref)
             values ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        )
        .bind(row.id)
        .bind(row.user.0)
        .bind(&row.number_hmac)
        .bind(row.hmac_key_version)
        .bind(&row.challenge)
        .bind(row.expires_at)
        .bind(row.attempts)
        .bind(row.verified_at)
        .bind(&row.provider_ref)
        .execute(&mut *self.tx)
        .await
        .map_err(|error| conflict_or(error, "phone number already bound"))?;
        Ok(row)
    }

    async fn active_phone_challenge(
        &mut self,
        user: UserId,
    ) -> Result<Option<PhoneVerificationRow>, StoreError> {
        let row = sqlx::query(
            "select id, user_id, number_hmac, hmac_key_version, challenge, expires_at, attempts, verified_at, provider_ref
               from phone_verifications where user_id = $1
               order by expires_at desc nulls last limit 1",
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| phone_from_row(&row)).transpose()
    }

    async fn phone_by_hmac(
        &mut self,
        hmac: &str,
        key_version: i32,
    ) -> Result<Option<PhoneVerificationRow>, StoreError> {
        let row = sqlx::query(
            "select id, user_id, number_hmac, hmac_key_version, challenge, expires_at, attempts, verified_at, provider_ref
               from phone_verifications where number_hmac = $1 and hmac_key_version = $2",
        )
        .bind(hmac)
        .bind(key_version)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| phone_from_row(&row)).transpose()
    }

    async fn refresh_phone_challenge(
        &mut self,
        id: Uuid,
        challenge: &str,
        expires_at: OffsetDateTime,
    ) -> Result<PhoneVerificationRow, StoreError> {
        let row = sqlx::query(
            "update phone_verifications
                set challenge = $2, expires_at = $3, attempts = 0
              where id = $1 and verified_at is null
              returning id, user_id, number_hmac, hmac_key_version, challenge, expires_at, attempts, verified_at, provider_ref",
        )
        .bind(id)
        .bind(challenge)
        .bind(expires_at)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("phone challenge"))?;
        phone_from_row(&row)
    }

    async fn consume_phone_attempt(
        &mut self,
        id: Uuid,
    ) -> Result<PhoneVerificationRow, StoreError> {
        let row = sqlx::query(
            "update phone_verifications
                set attempts = attempts + 1
              where id = $1 and attempts < $2
              returning id, user_id, number_hmac, hmac_key_version, challenge, expires_at, attempts, verified_at, provider_ref",
        )
        .bind(id)
        .bind(PHONE_MAX_ATTEMPTS)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::Conflict("phone challenge exhausted"))?;
        phone_from_row(&row)
    }

    async fn mark_phone_verified(
        &mut self,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<PhoneVerificationRow, StoreError> {
        let row = sqlx::query(
            "update phone_verifications
                set verified_at = $2, challenge = null
              where id = $1
              returning id, user_id, number_hmac, hmac_key_version, challenge, expires_at, attempts, verified_at, provider_ref",
        )
        .bind(id)
        .bind(at)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        phone_from_row(&row)
    }

    async fn verified_phone_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Option<PhoneVerificationRow>, StoreError> {
        let row = sqlx::query(
            "select id, user_id, number_hmac, hmac_key_version, challenge, expires_at, attempts, verified_at, provider_ref
               from phone_verifications
              where user_id = $1 and verified_at is not null
              order by verified_at desc limit 1",
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| phone_from_row(&row)).transpose()
    }

    async fn insert_proposal(
        &mut self,
        proposal: MoneyProposal,
    ) -> Result<MoneyProposal, StoreError> {
        proposal_insert(&mut self.tx, proposal).await
    }

    async fn get_proposal_by_replay(
        &mut self,
        replay_key: &str,
    ) -> Result<Option<MoneyProposal>, StoreError> {
        proposal_by_replay(&mut self.tx, replay_key).await
    }

    async fn get_proposal(&mut self, id: Uuid) -> Result<MoneyProposal, StoreError> {
        proposal_by_id(&mut self.tx, id).await
    }

    async fn confirm_proposal(
        &mut self,
        id: Uuid,
        confirmer: &str,
        now: OffsetDateTime,
    ) -> Result<MoneyProposal, StoreError> {
        let _ = now;
        proposal_confirm(&mut self.tx, id, confirmer).await
    }

    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError> {
        sqlx::query(
            "insert into admin_actions (actor_role, actor_token_digest, action, subject, before, after, reason)
             values ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(action.actor_role.name())
        .bind(action.actor_token_digest)
        .bind(action.action)
        .bind(action.subject)
        .bind(action.before)
        .bind(action.after)
        .bind(action.reason)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn settled_dests(&mut self, user: UserId) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "select distinct dest_address from withdrawals
              where user_id = $1 and status = 'settled'",
        )
        .bind(user.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let mut dests = Vec::new();
        for row in rows {
            dests.push(row.try_get("dest_address").map_err(db_error)?);
        }
        Ok(dests)
    }

    async fn record_settled_dest(&mut self, user: UserId, dest: String) -> Result<(), StoreError> {
        // Settled dests are derived from withdrawals; this records a decision fact
        // so tests without a full withdrawal row still have a dest.
        sqlx::query(
            "insert into compliance_decisions (subject_type, subject_id, kind, actor, payload)
             values ('settled_dest',$1,'settled_dest','machine',$2)",
        )
        .bind(user.0)
        .bind(json!({ "dest": dest }))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn config_i64(&mut self, key: &str) -> Result<Option<i64>, StoreError> {
        let row = sqlx::query("select value from config_entries where key = $1")
            .bind(key)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(row.and_then(|row| {
            row.try_get::<Value, _>("value")
                .ok()
                .and_then(|value| value.as_i64())
        }))
    }

    async fn config_json(&mut self, key: &str) -> Result<Option<Value>, StoreError> {
        let row = sqlx::query("select value from config_entries where key = $1")
            .bind(key)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(row.and_then(|row| row.try_get("value").ok()))
    }

    async fn set_config_i64(&mut self, key: &str, value: i64) -> Result<(), StoreError> {
        self.set_config_json(key, json!(value)).await
    }

    async fn set_config_json(&mut self, key: &str, value: Value) -> Result<(), StoreError> {
        sqlx::query(
            "insert into config_entries (key, value) values ($1,$2)
             on conflict (key) do update set value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn insert_frozen_license(
        &mut self,
        license: FrozenFundsLicense,
    ) -> Result<FrozenFundsLicense, StoreError> {
        sqlx::query(
            "insert into compliance_decisions (id, subject_type, subject_id, kind, actor, payload)
             values ($1,'frozen_license',$2,'frozen_funds_license','finance',$3)",
        )
        .bind(license.id)
        .bind(license.user.0)
        .bind(json!({
            "dest": license.dest,
            "amount_micro": license.amount_micro,
        }))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(license)
    }

    async fn get_frozen_license(&mut self, id: Uuid) -> Result<FrozenFundsLicense, StoreError> {
        let row = sqlx::query(
            "select id, subject_id, payload from compliance_decisions
              where id = $1 and kind = 'frozen_funds_license'",
        )
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("frozen license"))?;
        let payload: Value = row.try_get("payload").map_err(db_error)?;
        Ok(FrozenFundsLicense {
            id: row.try_get("id").map_err(db_error)?,
            user: UserId(row.try_get("subject_id").map_err(db_error)?),
            dest: payload
                .get("dest")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            amount_micro: payload
                .get("amount_micro")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        })
    }
}

#[async_trait]
impl Committable for PgComplianceTx {
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        self.tx.commit().await.map_err(db_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use application::ports::ScreenVerdict;
    use time::Duration;

    #[test]
    fn helpers_cover_labels_and_inbox_id() {
        let now = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(verdict_label(&ScreenVerdict::Hit), "hit");
        assert_eq!(
            verdict_label(&ScreenVerdict::Indeterminate),
            "indeterminate"
        );
        assert_eq!(
            verdict_label(&ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + Duration::hours(1),
                policy_version: "1".into(),
            }),
            "clear"
        );
        assert_eq!(verdict_from_row("hit", now, None, None), ScreenVerdict::Hit);
        assert_eq!(
            verdict_from_row("clear", now, None, None),
            ScreenVerdict::Indeterminate
        );
        assert!(matches!(
            verdict_from_row("clear", now, Some(now), Some("1".into())),
            ScreenVerdict::Clear { .. }
        ));
        assert_eq!(
            verdict_from_row("nope", now, None, None),
            ScreenVerdict::Indeterminate
        );
        assert_eq!(proposal_status("confirmed"), ProposalStatus::Confirmed);
        assert_eq!(proposal_status("rejected"), ProposalStatus::Rejected);
        assert_eq!(proposal_status("expired"), ProposalStatus::Expired);
        assert_eq!(proposal_status("pending"), ProposalStatus::Pending);
        assert_eq!(proposal_status_name(ProposalStatus::Pending), "pending");
        assert_eq!(proposal_status_name(ProposalStatus::Confirmed), "confirmed");
        assert_eq!(proposal_status_name(ProposalStatus::Rejected), "rejected");
        assert_eq!(proposal_status_name(ProposalStatus::Expired), "expired");
        assert_ne!(inbox_subject_id("a", "1"), inbox_subject_id("a", "2"));
        assert!(aml_leg_from_payload(&json!({})).is_none());
    }
}
