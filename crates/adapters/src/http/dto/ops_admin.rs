//! Ops admin-plane wire DTOs (plan D26/D27/D30) — Task 6.0a registration
//! shells; wave W2 owns the real handlers and any field growth.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// One identity's result inside the invariant report (D27).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InvariantIdentityDto {
    pub identity: String,
    pub pass: bool,
    pub detail: Option<String>,
}

/// `GET /admin/invariants`: one repeatable-read snapshot verdict.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InvariantReportDto {
    pub as_of: time::OffsetDateTime,
    pub pass: bool,
    pub identities: Vec<InvariantIdentityDto>,
}

/// `GET /admin/users/{id}/withdrawal_eligibility` (D30).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WithdrawalEligibilityDto {
    pub user_id: Uuid,
    pub cash_micro: i64,
    pub open_receivables_micro: i64,
    pub eligible: bool,
}

/// Staging faucet deposit (two-factor mounted; capped; audited).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct FaucetDepositRequest {
    pub user_id: Uuid,
    pub amount_micro: i64,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FaucetDepositDto {
    pub user_id: Uuid,
    pub amount_micro: i64,
    /// `None` on a replay observed through a different idempotency key.
    pub ledger_txn: Option<Uuid>,
    pub replayed: bool,
}

/// Dual-control proposal body (unwind / remedial credit / write-off): the
/// acting principal comes from RBAC; the reason is mandatory.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DualControlRequest {
    pub reason: String,
    pub idempotency_key: String,
}

/// Unwind authority state (D30).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MarketUnwindDto {
    pub market_id: Uuid,
    pub stage: String,
    pub reason: String,
    pub confirm_not_before: time::OffsetDateTime,
    pub reversal_txn: Option<Uuid>,
}

/// Remedial credit body (grok r2 N2): dual-controlled, capped, audited.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RemedialCreditRequest {
    pub user_id: Uuid,
    pub amount_micro: i64,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RemedialCreditDto {
    pub market_id: Uuid,
    pub user_id: Uuid,
    pub amount_micro: i64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReceivableWriteOffDto {
    pub receivable_id: Uuid,
    pub amount_micro: i64,
    pub status: String,
}

/// One redacted audit row (`audit-read` capability; digests only).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditActionDto {
    pub id: Uuid,
    pub actor_role: String,
    pub actor_token_digest: String,
    pub action: String,
    pub subject: String,
    pub reason: Option<String>,
    pub at: time::OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditPageDto {
    pub actions: Vec<AuditActionDto>,
    pub next_before: Option<time::OffsetDateTime>,
}

/// `GET /admin/drafts/{id}/publish_status` — the 202 status URL target for
/// the command-based publish_now (codex r3 NEW-4).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublishStatusDto {
    pub draft_id: Uuid,
    pub status: String,
    pub result_market_id: Option<Uuid>,
    pub error: Option<String>,
    /// Poll here (also returned by the 202 publish-now authorization).
    pub status_url: String,
}
