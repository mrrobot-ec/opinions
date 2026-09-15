//! Ops config-plane wire DTOs (plan D24/D25) — Task 6.0a registration
//! shells; wave W1 owns the real handlers and any field growth.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// One committed config entry.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfigEntryDto {
    pub key: String,
    pub value: serde_json::Value,
}

/// The complete committed snapshot plus its generation watermark.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfigSnapshotDto {
    pub generation: i64,
    pub entries: Vec<ConfigEntryDto>,
}

/// Typed patch over the prospective snapshot (D24 write path). Non-sensitive
/// keys apply directly; sensitive keys are rejected toward the proposal flow.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetConfigRequest {
    /// Map of key → new value.
    pub patch: serde_json::Value,
    /// Optimistic base; a moved generation is a typed conflict.
    #[serde(default)]
    pub expected_base_generation: Option<i64>,
    pub reason: String,
    pub idempotency_key: String,
}

/// Result of a direct (non-sensitive) apply.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfigAppliedDto {
    pub generation: i64,
    pub changed_keys: Vec<String>,
}

/// Two-phase proposal creation for sensitive keys (D24). Exactly one of
/// `patch` / `revert_of_generation` must be present: the latter pre-fills an
/// emergency-revert patch from that generation's `config_changes` history
/// (`old` values; pause keys stripped) under the SAME dual control.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateConfigProposalRequest {
    #[serde(default)]
    pub patch: Option<serde_json::Value>,
    #[serde(default)]
    pub revert_of_generation: Option<i64>,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfigProposalDto {
    pub id: Uuid,
    pub status: String,
    pub base_generation: i64,
    pub patch: serde_json::Value,
    pub reason: String,
    pub expires_at: time::OffsetDateTime,
    pub resulting_generation: Option<i64>,
}

/// Confirm/reject body: the acting principal comes from RBAC, the reason is
/// recorded in the audit fact.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SettleConfigProposalRequest {
    #[serde(default)]
    pub reason: Option<String>,
}
