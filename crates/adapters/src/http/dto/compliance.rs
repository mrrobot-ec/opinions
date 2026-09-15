//! W2 compliance wire DTOs.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PhoneChallengeRequest {
    pub user_id: Uuid,
    pub e164: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PhoneChallengeDto {
    pub user_id: Uuid,
    pub expires_at: time::OffsetDateTime,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PhoneVerifyRequest {
    pub user_id: Uuid,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PhoneVerifyDto {
    pub user_id: Uuid,
    pub verified: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DualControlBody {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProposalDto {
    pub id: Uuid,
    pub kind: String,
    pub status: String,
    pub confirm_not_before: time::OffsetDateTime,
}

/// Ban / unban proposal body. `epoch` is the replay dimension from the
/// command matrix (`user id + epoch`): resending the same epoch with the
/// same target returns the original proposal instead of a second one.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StatusProposeRequest {
    pub reason: String,
    #[serde(default)]
    pub epoch: i64,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SelfExclusionRequest {
    pub user_id: Uuid,
    pub cooling_off_hours: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SelfExclusionDto {
    pub id: Uuid,
    pub cooling_off_until: time::OffsetDateTime,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DepositLimitRequest {
    pub limit_micro: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DepositLimitDto {
    pub limit_micro: i64,
    pub pending_limit_micro: Option<i64>,
    pub pending_effective_at: Option<time::OffsetDateTime>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct FrozenLicenseRequest {
    pub dest: String,
    pub amount_micro: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SandboxKycCompleteRequest {
    pub user_id: Uuid,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KycEventDto {
    pub user_id: Uuid,
    pub to_tier: i32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StatusDto {
    pub user_id: Uuid,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InboxAckDto {
    pub outcome: String,
}

/// Cleared AML / sanctions flag echo. The rule label is operator-facing;
/// no user-facing surface ever renders it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AmlFlagDto {
    pub id: Uuid,
    pub user_id: Uuid,
    pub rule: String,
    pub open: bool,
}

/// Counsel-shaped frozen-funds license echo (dest is never user-picked).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FrozenLicenseDto {
    pub id: Uuid,
    pub user_id: Uuid,
    pub dest: String,
    pub amount_micro: i64,
}
