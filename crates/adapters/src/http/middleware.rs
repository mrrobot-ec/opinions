//! Fail-closed admin RBAC (plan D26).
//!
//! One layer guards every `/admin/**` route: `ADMIN_TOKENS_JSON` is parsed at
//! startup (reject empty/duplicate/malformed; SHA-256 digests only — the
//! environment never holds a raw token), presented tokens are hashed and
//! compared in constant time, and a single capability matrix maps each admin
//! route to the role set that may call it. Anything unmapped is DENIED.
//! Dual-control endpoints additionally require distinct token ids — that
//! check is per-endpoint use-case logic (W1/W2), not this layer's.
//!
//! The authenticated principal is inserted into request extensions as a
//! typed [`AdminContext::Admin`] so downstream handlers thread it into the
//! shared use cases. Token digests are redacted vocabulary: they never
//! appear in logs or error bodies.

use std::collections::BTreeSet;

use application::model::{AdminContext, AdminRole};
use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use super::error::ErrorResponse;

/// Header carrying the raw bearer token (hashed before any comparison).
pub const ADMIN_TOKEN_HEADER: &str = "x-admin-token";

/// One validated `ADMIN_TOKENS_JSON` entry.
#[derive(Debug, Clone)]
pub struct AdminTokenEntry {
    /// Operator-facing label; also the dual-control principal id.
    pub id: String,
    /// Explicit role SET (grok r2 N4: one bearer may carry several roles).
    pub roles: BTreeSet<AdminRole>,
    digest: [u8; 32],
}

/// The startup-validated admin token registry. Empty means every admin
/// request is 401 — fail closed, never open.
#[derive(Debug, Clone, Default)]
pub struct AdminTokens {
    entries: Vec<AdminTokenEntry>,
}

fn parse_role(name: &str) -> Result<AdminRole, String> {
    match name {
        "curator" => Ok(AdminRole::Curator),
        "ops" => Ok(AdminRole::Ops),
        "finance" => Ok(AdminRole::Finance),
        "superadmin" => Ok(AdminRole::Superadmin),
        other => Err(format!("ADMIN_TOKENS_JSON: unknown role {other:?}")),
    }
}

fn parse_digest(hex: &str) -> Result<[u8; 32], String> {
    let normalized = hex.to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("ADMIN_TOKENS_JSON: sha256 must be 64 hex characters".to_string());
    }
    let mut out = [0_u8; 32];
    for (i, chunk) in normalized.as_bytes().chunks_exact(2).enumerate() {
        // Both nibbles were validated hex above; a miss decodes as zero
        // rather than reintroducing an unreachable error arm.
        let hi = char::from(chunk[0]).to_digit(16).unwrap_or(0);
        let lo = char::from(chunk[1]).to_digit(16).unwrap_or(0);
        #[allow(clippy::cast_possible_truncation)]
        {
            out[i] = ((hi << 4) | lo) as u8;
        }
    }
    Ok(out)
}

fn digest_hex(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    digest
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(*byte >> 4)]),
                char::from(HEX[usize::from(*byte & 0x0f)]),
            ]
        })
        .collect()
}

/// Constant-time 32-byte comparison: no early exit, single accumulator.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// THE comparison for every bearer secret in this adapter, admin or not.
///
/// `presented == expected` on a `String`/`str` short-circuits at the first
/// differing byte and its length check leaks the length outright, so the
/// response time is a function of how much of the secret the caller guessed
/// right. That is the oracle this crate already refuses to give an admin token
/// (see [`AdminTokens::authenticate`]); the demo token gates trades, votes,
/// withdrawal requests, self-exclusion and the WebSocket's per-user
/// notification stream, so it earns the same treatment.
///
/// Hashing first makes the comparison fixed-width regardless of the two
/// inputs' lengths, so neither the length nor the position of the first
/// difference is observable.
#[must_use]
pub fn secret_eq(presented: &str, expected: &str) -> bool {
    let presented: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
    let expected: [u8; 32] = Sha256::digest(expected.as_bytes()).into();
    ct_eq(&presented, &expected)
}

#[derive(serde::Deserialize)]
struct RawEntry {
    id: String,
    roles: Vec<String>,
    sha256: String,
}

impl AdminTokens {
    /// Parses and validates the `ADMIN_TOKENS_JSON` document: a non-empty
    /// array of `{ "id", "roles": [..], "sha256": "<hex64>" }`.
    ///
    /// # Errors
    /// Malformed JSON, an empty array, an empty id or role set, an unknown
    /// role, a malformed digest, or any duplicate id/digest.
    pub fn parse(json: &str) -> Result<Self, String> {
        let raw: Vec<RawEntry> = serde_json::from_str(json)
            .map_err(|error| format!("ADMIN_TOKENS_JSON is malformed: {error}"))?;
        if raw.is_empty() {
            return Err("ADMIN_TOKENS_JSON must not be empty".to_string());
        }
        let mut entries = Vec::with_capacity(raw.len());
        let mut ids = BTreeSet::new();
        let mut digests = BTreeSet::new();
        for entry in raw {
            if entry.id.trim().is_empty() {
                return Err("ADMIN_TOKENS_JSON: token id must not be empty".to_string());
            }
            if !ids.insert(entry.id.clone()) {
                return Err(format!("ADMIN_TOKENS_JSON: duplicate id {:?}", entry.id));
            }
            if entry.roles.is_empty() {
                return Err(format!(
                    "ADMIN_TOKENS_JSON: token {:?} has no roles",
                    entry.id
                ));
            }
            let mut roles = BTreeSet::new();
            for role in &entry.roles {
                if !roles.insert(parse_role(role)?) {
                    return Err(format!(
                        "ADMIN_TOKENS_JSON: token {:?} repeats role {role:?}",
                        entry.id
                    ));
                }
            }
            let digest = parse_digest(&entry.sha256)?;
            if !digests.insert(digest) {
                return Err(format!(
                    "ADMIN_TOKENS_JSON: token {:?} duplicates another digest",
                    entry.id
                ));
            }
            entries.push(AdminTokenEntry {
                id: entry.id,
                roles,
                digest,
            });
        }
        Ok(Self { entries })
    }

    /// Production startup validation: `ADMIN_TOKENS_JSON` must be present
    /// and valid — legacy `ADMIN_TOKEN` fallback is gone (D26, 6.0b).
    ///
    /// # Errors
    /// A missing variable or any [`Self::parse`] rejection.
    pub fn from_env_required() -> Result<Self, String> {
        match std::env::var("ADMIN_TOKENS_JSON") {
            Ok(value) => Self::parse(&value),
            Err(_) => Err(
                "ADMIN_TOKENS_JSON is required (legacy ADMIN_TOKEN was removed in Phase 6)"
                    .to_string(),
            ),
        }
    }

    /// Constructor surface for `AppState`: a valid document is used; an
    /// absent or invalid one denies every admin request (fail closed —
    /// `main` rejects invalid documents at startup via
    /// [`Self::from_env_required`]).
    #[must_use]
    pub fn from_env_or_deny() -> Self {
        std::env::var("ADMIN_TOKENS_JSON")
            .ok()
            .and_then(|value| Self::parse(&value).ok())
            .unwrap_or_default()
    }

    /// Test/bootstrap surface: one raw token carrying every role.
    #[must_use]
    pub fn single_test_token(token: &str) -> Self {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        Self {
            entries: vec![AdminTokenEntry {
                id: "test-admin".to_string(),
                roles: BTreeSet::from([
                    AdminRole::Curator,
                    AdminRole::Ops,
                    AdminRole::Finance,
                    AdminRole::Superadmin,
                ]),
                digest,
            }],
        }
    }

    /// Constant-time lookup of the presented raw token. Every entry is
    /// compared; no early exit.
    #[must_use]
    pub fn authenticate(&self, presented: &str) -> Option<&AdminTokenEntry> {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let mut matched: Option<&AdminTokenEntry> = None;
        for entry in &self.entries {
            if ct_eq(&entry.digest, &digest) {
                matched = Some(entry);
            }
        }
        matched
    }
}

/// Route capabilities (D26): one per admin surface. Dual-control endpoints
/// split propose/confirm so the matrix itself pins the two principals' roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    MarketLifecycle,
    ContentCuration,
    ModerationAction,
    FinanceRead,
    AuditRead,
    ConfigRead,
    ConfigWrite,
    InvariantRead,
    UnwindPropose,
    UnwindConfirm,
    UnwindDecide,
    RemedialCredit,
    ReceivableWriteOff,
    Faucet,
    WithdrawalEligibilityRead,
    WithdrawApprove,
    WithdrawDeny,
    AmlClear,
    BanPropose,
    BanConfirm,
    ShadowLimit,
    SelfExclusionLift,
    CreditGrant,
    BonusReserveTopUp,
    DepositAdmit,
    DepositRefund,
    FrozenFundsLicense,
    FeeOverride,
}

/// THE capability matrix: `/admin/**` route template + method → capability.
/// Fail-closed contract: an admin route with no row here is 403 for every
/// token.
const ROUTE_CAPABILITIES: &[(&str, &str, Capability)] = &[
    (
        "POST",
        "/admin/markets/{id}/advance",
        Capability::MarketLifecycle,
    ),
    (
        "POST",
        "/admin/markets/{id}/resolve",
        Capability::MarketLifecycle,
    ),
    ("GET", "/admin/markets/flagged", Capability::MarketLifecycle),
    ("GET", "/admin/fees/summary", Capability::FinanceRead),
    (
        "GET",
        "/admin/comments/reported",
        Capability::ModerationAction,
    ),
    (
        "POST",
        "/admin/comments/{id}/moderate",
        Capability::ModerationAction,
    ),
    ("GET", "/admin/drafts", Capability::ContentCuration),
    ("POST", "/admin/drafts", Capability::ContentCuration),
    ("PATCH", "/admin/drafts/{id}", Capability::ContentCuration),
    (
        "POST",
        "/admin/drafts/{id}/approve",
        Capability::ContentCuration,
    ),
    (
        "POST",
        "/admin/drafts/{id}/reject",
        Capability::ContentCuration,
    ),
    (
        "POST",
        "/admin/drafts/{id}/publish_now",
        Capability::ContentCuration,
    ),
    (
        "GET",
        "/admin/drafts/{id}/publish_status",
        Capability::ContentCuration,
    ),
    ("GET", "/admin/config", Capability::ConfigRead),
    ("POST", "/admin/config", Capability::ConfigWrite),
    ("POST", "/admin/config/proposals", Capability::ConfigWrite),
    (
        "POST",
        "/admin/config/proposals/{id}/confirm",
        Capability::ConfigWrite,
    ),
    (
        "POST",
        "/admin/config/proposals/{id}/reject",
        Capability::ConfigWrite,
    ),
    ("GET", "/admin/invariants", Capability::InvariantRead),
    ("GET", "/admin/audit", Capability::AuditRead),
    (
        "GET",
        "/admin/users/{id}/withdrawal_eligibility",
        Capability::WithdrawalEligibilityRead,
    ),
    ("POST", "/admin/deposits", Capability::Faucet),
    (
        "POST",
        "/admin/markets/{id}/unwind/propose",
        Capability::UnwindPropose,
    ),
    (
        "POST",
        "/admin/markets/{id}/unwind/confirm",
        Capability::UnwindConfirm,
    ),
    (
        "POST",
        "/admin/markets/{id}/unwind/reject",
        Capability::UnwindDecide,
    ),
    (
        "POST",
        "/admin/markets/{id}/remedial_credit/propose",
        Capability::RemedialCredit,
    ),
    (
        "POST",
        "/admin/markets/{id}/remedial_credit/confirm",
        Capability::RemedialCredit,
    ),
    (
        "POST",
        "/admin/receivables/{id}/write_off/propose",
        Capability::ReceivableWriteOff,
    ),
    (
        "POST",
        "/admin/receivables/{id}/write_off/confirm",
        Capability::ReceivableWriteOff,
    ),
    (
        "POST",
        "/admin/withdrawals/{id}/approve",
        Capability::WithdrawApprove,
    ),
    (
        "POST",
        "/admin/withdrawals/{id}/deny",
        Capability::WithdrawDeny,
    ),
    (
        "POST",
        "/admin/withdrawals/{id}/approve/propose",
        Capability::WithdrawApprove,
    ),
    (
        "POST",
        "/admin/withdrawals/{id}/approve/confirm",
        Capability::WithdrawApprove,
    ),
    (
        "POST",
        "/admin/aml/{id}/clear/propose",
        Capability::AmlClear,
    ),
    (
        "POST",
        "/admin/aml/{id}/clear/confirm",
        Capability::AmlClear,
    ),
    (
        "POST",
        "/admin/users/{id}/ban/propose",
        Capability::BanPropose,
    ),
    (
        "POST",
        "/admin/users/{id}/ban/confirm",
        Capability::BanConfirm,
    ),
    (
        "POST",
        "/admin/users/{id}/unban/propose",
        Capability::BanPropose,
    ),
    (
        "POST",
        "/admin/users/{id}/unban/confirm",
        Capability::BanConfirm,
    ),
    ("POST", "/admin/users/{id}/shadow", Capability::ShadowLimit),
    (
        "POST",
        "/admin/users/{id}/unshadow",
        Capability::ShadowLimit,
    ),
    (
        "POST",
        "/admin/self_exclusions/{id}/lift/propose",
        Capability::SelfExclusionLift,
    ),
    (
        "POST",
        "/admin/self_exclusions/{id}/lift/confirm",
        Capability::SelfExclusionLift,
    ),
    (
        "POST",
        "/admin/credits/grant/propose",
        Capability::CreditGrant,
    ),
    (
        "POST",
        "/admin/credits/grant/confirm",
        Capability::CreditGrant,
    ),
    (
        "POST",
        "/admin/bonus_reserve/topup/propose",
        Capability::BonusReserveTopUp,
    ),
    (
        "POST",
        "/admin/bonus_reserve/topup/confirm",
        Capability::BonusReserveTopUp,
    ),
    (
        "POST",
        "/admin/deposits/{id}/admit/propose",
        Capability::DepositAdmit,
    ),
    (
        "POST",
        "/admin/deposits/{id}/admit/confirm",
        Capability::DepositAdmit,
    ),
    (
        "POST",
        "/admin/deposits/{id}/refund/propose",
        Capability::DepositRefund,
    ),
    (
        "POST",
        "/admin/deposits/{id}/refund/confirm",
        Capability::DepositRefund,
    ),
    (
        "POST",
        "/admin/frozen_funds/{id}/license/propose",
        Capability::FrozenFundsLicense,
    ),
    (
        "POST",
        "/admin/frozen_funds/{id}/license/confirm",
        Capability::FrozenFundsLicense,
    ),
    (
        "POST",
        "/admin/markets/{id}/fee_override/propose",
        Capability::FeeOverride,
    ),
    (
        "POST",
        "/admin/markets/{id}/fee_override/confirm",
        Capability::FeeOverride,
    ),
];

/// Role → capability grants. Roles are SETS — there is no hierarchy; every
/// grant is spelled out. Per-KEY config authority (D24 catalog) and
/// dual-control token-id distinctness are enforced inside the use cases.
#[must_use]
pub fn role_grants(role: AdminRole, capability: Capability) -> bool {
    use Capability as Cap;
    match role {
        AdminRole::Curator => matches!(
            capability,
            Cap::MarketLifecycle
                | Cap::ContentCuration
                | Cap::ModerationAction
                | Cap::ConfigRead
                | Cap::ConfigWrite
        ),
        AdminRole::Ops => matches!(
            capability,
            Cap::ConfigRead
                | Cap::ConfigWrite
                | Cap::InvariantRead
                | Cap::AuditRead
                | Cap::WithdrawalEligibilityRead
                | Cap::BanPropose
                | Cap::ShadowLimit
        ),
        AdminRole::Finance => matches!(
            capability,
            Cap::FinanceRead
                | Cap::ConfigRead
                | Cap::ConfigWrite
                | Cap::AuditRead
                | Cap::UnwindConfirm
                | Cap::UnwindDecide
                | Cap::RemedialCredit
                | Cap::ReceivableWriteOff
                | Cap::Faucet
                | Cap::WithdrawalEligibilityRead
                | Cap::WithdrawApprove
                | Cap::WithdrawDeny
                | Cap::AmlClear
                | Cap::SelfExclusionLift
                | Cap::CreditGrant
                | Cap::BonusReserveTopUp
                | Cap::DepositAdmit
                | Cap::DepositRefund
                | Cap::FrozenFundsLicense
                | Cap::FeeOverride
        ),
        AdminRole::Superadmin => matches!(
            capability,
            Cap::MarketLifecycle
                | Cap::ContentCuration
                | Cap::ModerationAction
                | Cap::FinanceRead
                | Cap::ConfigRead
                | Cap::ConfigWrite
                | Cap::InvariantRead
                | Cap::AuditRead
                | Cap::UnwindPropose
                | Cap::UnwindDecide
                | Cap::RemedialCredit
                | Cap::ReceivableWriteOff
                | Cap::Faucet
                | Cap::WithdrawalEligibilityRead
                | Cap::WithdrawApprove
                | Cap::WithdrawDeny
                | Cap::AmlClear
                | Cap::BanConfirm
                | Cap::SelfExclusionLift
                | Cap::CreditGrant
                | Cap::BonusReserveTopUp
                | Cap::DepositAdmit
                | Cap::DepositRefund
                | Cap::FrozenFundsLicense
                | Cap::FeeOverride
        ),
    }
}

/// The matrix row for one matched route, if any.
#[must_use]
pub fn required_capability(method: &Method, route_template: &str) -> Option<Capability> {
    ROUTE_CAPABILITIES
        .iter()
        .find(|(m, template, _)| *m == method.as_str() && *template == route_template)
        .map(|(_, _, capability)| *capability)
}

fn unauthorized() -> Response {
    ErrorResponse::new(
        StatusCode::UNAUTHORIZED,
        "Unauthorized",
        "missing or invalid x-admin-token",
    )
    .into_response()
}

fn forbidden(message: &str) -> Response {
    ErrorResponse::new(StatusCode::FORBIDDEN, "Forbidden", message).into_response()
}

/// The fail-closed layer mounted ahead of every `/admin/**` route.
/// Deliberately NON-generic (one shared registry, one instantiation): the
/// state is the token registry itself, not the whole `AppState<S>`.
///
/// Order of checks: authenticate (401) → matrix row exists (403) → some role
/// on the token grants the capability (403). On success the typed
/// [`AdminContext::Admin`] principal is inserted into request extensions.
pub async fn admin_rbac(
    State(tokens): State<std::sync::Arc<AdminTokens>>,
    matched: MatchedPath,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(presented) = request
        .headers()
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return unauthorized();
    };
    let Some(entry) = tokens.authenticate(presented) else {
        return unauthorized();
    };
    let Some(capability) = required_capability(request.method(), matched.as_str()) else {
        // Fail closed: an admin route without a matrix row is nobody's.
        return forbidden("no capability mapping for this admin route");
    };
    // Deterministic role choice for the downstream AdminContext: the first
    // (ordered) role granting the capability.
    let Some(role) = entry
        .roles
        .iter()
        .copied()
        .find(|role| role_grants(*role, capability))
    else {
        return forbidden("token roles do not grant this route's capability");
    };
    let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
    request.extensions_mut().insert(AdminContext::Admin {
        token_digest: digest_hex(&digest),
        role,
    });
    next.run(request).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn sha256_hex(token: &str) -> String {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        digest_hex(&digest)
    }

    fn doc(entries: &[(&str, &[&str], &str)]) -> String {
        let entries: Vec<serde_json::Value> = entries
            .iter()
            .map(|(id, roles, token)| {
                serde_json::json!({
                    "id": id,
                    "roles": roles,
                    "sha256": sha256_hex(token),
                })
            })
            .collect();
        serde_json::to_string(&entries).unwrap()
    }

    #[test]
    fn a_valid_document_authenticates_by_digest_only() {
        let tokens = AdminTokens::parse(&doc(&[
            ("ops-1", &["ops"], "s3cret"),
            ("fin-1", &["finance", "superadmin"], "other"),
        ]))
        .unwrap();
        let entry = tokens.authenticate("s3cret").unwrap();
        assert_eq!(entry.id, "ops-1");
        assert_eq!(
            entry.roles,
            std::collections::BTreeSet::from([AdminRole::Ops])
        );
        let multi = tokens.authenticate("other").unwrap();
        assert_eq!(multi.roles.len(), 2);
        assert!(tokens.authenticate("wrong").is_none());
        assert!(tokens.authenticate("").is_none());
    }

    #[test]
    fn startup_validation_rejects_empty_duplicate_and_malformed_documents() {
        for (label, document) in [
            ("malformed", "not json".to_string()),
            ("empty array", "[]".to_string()),
            ("missing fields", r#"[{"id":"x"}]"#.to_string()),
            ("wrong types", r#"[{"id":1,"roles":"ops","sha256":7}]"#.to_string()),
            ("not an array", r#"{"id":"x"}"#.to_string()),
            ("empty id", doc(&[("", &["ops"], "a")])),
            ("no roles", doc(&[("x", &[], "a")])),
            ("unknown role", doc(&[("x", &["root"], "a")])),
            ("duplicate role", r#"[{"id":"x","roles":["ops","ops"],"sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]"#.to_string()),
            ("duplicate id", doc(&[("x", &["ops"], "a"), ("x", &["finance"], "b")])),
            ("duplicate digest", doc(&[("x", &["ops"], "a"), ("y", &["finance"], "a")])),
            ("short digest", r#"[{"id":"x","roles":["ops"],"sha256":"abcd"}]"#.to_string()),
            ("non-hex digest", format!(r#"[{{"id":"x","roles":["ops"],"sha256":"{}"}}]"#, "zz".repeat(32))),
        ] {
            assert!(AdminTokens::parse(&document).is_err(), "{label} must be rejected");
        }
        // Uppercase hex digests are normalized, not rejected.
        let upper = sha256_hex("tok").to_ascii_uppercase();
        let tokens = AdminTokens::parse(&format!(
            r#"[{{"id":"x","roles":["ops"],"sha256":"{upper}"}}]"#
        ))
        .unwrap();
        assert!(tokens.authenticate("tok").is_some());
        // serde's sequence form of an entry is legal input, same validation.
        let seq = AdminTokens::parse(&format!(r#"[["x",["ops"],"{upper}"]]"#)).unwrap();
        assert!(seq.authenticate("tok").is_some());
    }

    #[test]
    fn env_parsing_is_fail_closed_for_state_and_fail_loud_for_main() {
        // Serialized via the env var name being unique to this test.
        std::env::remove_var("ADMIN_TOKENS_JSON");
        assert!(AdminTokens::from_env_required().is_err());
        assert!(AdminTokens::from_env_or_deny().entries.is_empty());
        std::env::set_var("ADMIN_TOKENS_JSON", "broken");
        assert!(AdminTokens::from_env_required().is_err());
        assert!(AdminTokens::from_env_or_deny().entries.is_empty());
        std::env::set_var("ADMIN_TOKENS_JSON", doc(&[("ops-1", &["ops"], "tok")]));
        assert!(AdminTokens::from_env_required().is_ok());
        assert_eq!(AdminTokens::from_env_or_deny().entries.len(), 1);
        std::env::remove_var("ADMIN_TOKENS_JSON");
    }

    #[test]
    fn the_matrix_is_exhaustive_over_methods_and_fail_closed_elsewhere() {
        assert_eq!(
            required_capability(&Method::POST, "/admin/markets/{id}/advance"),
            Some(Capability::MarketLifecycle)
        );
        assert_eq!(
            required_capability(&Method::GET, "/admin/markets/{id}/advance"),
            None
        );
        assert_eq!(required_capability(&Method::POST, "/admin/unknown"), None);
        // Every matrix row is granted by at least one role and every listed
        // route is under /admin/.
        for (method, template, capability) in ROUTE_CAPABILITIES {
            assert!(template.starts_with("/admin/"), "{template}");
            assert!(["GET", "POST", "PATCH"].contains(method));
            let grants = [
                AdminRole::Curator,
                AdminRole::Ops,
                AdminRole::Finance,
                AdminRole::Superadmin,
            ]
            .into_iter()
            .filter(|role| role_grants(*role, *capability))
            .count();
            assert!(grants >= 1, "{template} is granted to no role");
        }
    }

    #[test]
    fn dual_control_split_pins_propose_and_confirm_to_distinct_roles() {
        // D30: propose superadmin / confirm finance — the matrix itself keeps
        // one role from doing both legs.
        assert!(role_grants(
            AdminRole::Superadmin,
            Capability::UnwindPropose
        ));
        assert!(!role_grants(AdminRole::Finance, Capability::UnwindPropose));
        assert!(role_grants(AdminRole::Finance, Capability::UnwindConfirm));
        assert!(!role_grants(
            AdminRole::Superadmin,
            Capability::UnwindConfirm
        ));
        // Audit reads need audit-read, which curators do NOT have (D26).
        assert!(!role_grants(AdminRole::Curator, Capability::AuditRead));
        assert!(role_grants(AdminRole::Ops, Capability::AuditRead));
        // The faucet is finance/superadmin only.
        assert!(!role_grants(AdminRole::Curator, Capability::Faucet));
        assert!(!role_grants(AdminRole::Ops, Capability::Faucet));
    }

    #[test]
    fn constant_time_compare_and_digest_hex_are_exact() {
        let a = [7_u8; 32];
        let mut b = a;
        assert!(ct_eq(&a, &b));
        b[31] ^= 1;
        assert!(!ct_eq(&a, &b));
        assert_eq!(sha256_hex("admin-token"), {
            let digest: [u8; 32] = Sha256::digest(b"admin-token").into();
            digest_hex(&digest)
        });
        assert!(parse_digest(&"ab".repeat(32)).is_ok());
        assert!(parse_digest("AB0").is_err());
    }

    #[test]
    fn test_token_constructor_carries_every_role() {
        let tokens = AdminTokens::single_test_token("t");
        let entry = tokens.authenticate("t").unwrap();
        assert_eq!(entry.roles.len(), 4);
        assert_eq!(entry.id, "test-admin");
        // Debug stays digest-bearing but token-free; Clone shares the set.
        let printed = format!("{tokens:?}");
        assert!(printed.contains("test-admin") && !printed.contains("\"t\""));
        assert!(tokens.clone().authenticate("t").is_some());
        assert_eq!(format!("{:?}", Capability::Faucet), "Faucet");
        // `Capability`'s `PartialEq` is what `required_capability` and
        // `role_grants` dispatch on, so it must DISCRIMINATE — comparing a
        // capability with itself proves nothing. Two capabilities that share a
        // role grant (superadmin holds both) must still be distinct values.
        assert_ne!(Capability::Faucet, Capability::AuditRead);
        assert!(role_grants(AdminRole::Superadmin, Capability::Faucet));
        assert!(role_grants(AdminRole::Superadmin, Capability::AuditRead));
    }

    async fn unmapped_handler() -> &'static str {
        "reached only without the fail-closed layer"
    }

    /// A mapped route's handler: proves the layer inserted the typed
    /// principal with the FIRST granting role (audit-read → Ops here).
    async fn principal_echo(
        axum::Extension(actor): axum::Extension<AdminContext>,
    ) -> axum::Json<serde_json::Value> {
        let AdminContext::Admin { token_digest, role } = actor else {
            return axum::Json(serde_json::json!({ "actor": "machine" }));
        };
        axum::Json(serde_json::json!({
            "role": role.name(),
            "digest_len": token_digest.len(),
        }))
    }

    #[tokio::test]
    async fn the_layer_authenticates_and_inserts_the_first_granting_role() {
        use axum::routing::get;
        use tower::ServiceExt;

        // The echo handler's machine arm exists only for its own totality.
        let machine = principal_echo(axum::Extension(AdminContext::Machine)).await;
        assert_eq!(machine.0["actor"], "machine");

        let tokens = std::sync::Arc::new(AdminTokens::single_test_token("admin"));
        let app = axum::Router::new()
            .route("/admin/audit", get(principal_echo))
            .route_layer(axum::middleware::from_fn_with_state(tokens, admin_rbac))
            .with_state(());
        // Missing header and unknown token: 401 before the handler.
        for header in [None, Some("wrong")] {
            let mut builder = axum::http::Request::builder()
                .method("GET")
                .uri("/admin/audit");
            if let Some(value) = header {
                builder = builder.header(ADMIN_TOKEN_HEADER, value);
            }
            let response = app
                .clone()
                .oneshot(builder.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        // A known token whose roles lack the capability: 403, never the
        // handler (curator has no audit-read, D26).
        let curator_only = std::sync::Arc::new(
            AdminTokens::parse(&doc(&[("curator-1", &["curator"], "curator-token")])).unwrap(),
        );
        let curator_app = axum::Router::new()
            .route("/admin/audit", get(principal_echo))
            .route_layer(axum::middleware::from_fn_with_state(
                curator_only,
                admin_rbac,
            ))
            .with_state(());
        let response = curator_app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/admin/audit")
                    .header(ADMIN_TOKEN_HEADER, "curator-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // Authorized: the handler sees Admin{role: ops} — the first role in
        // canonical order granting audit-read (curator lacks it, D26).
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/admin/audit")
                    .header(ADMIN_TOKEN_HEADER, "admin")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4_096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["role"], "ops");
        assert_eq!(body["digest_len"], 64);
    }

    #[tokio::test]
    async fn an_admin_route_without_a_matrix_row_is_denied_for_every_token() {
        use axum::routing::get;
        use tower::ServiceExt;

        // The handler itself works — proving the DENIAL below comes from the
        // layer, not from a broken route.
        assert_eq!(
            unmapped_handler().await,
            "reached only without the fail-closed layer"
        );
        // Fail-closed contract: a wave mounting a new /admin route WITHOUT a
        // matrix row gets 403 for even an all-roles token — never a handler.
        let tokens = std::sync::Arc::new(AdminTokens::single_test_token("admin"));
        let app = axum::Router::new()
            .route("/admin/unmapped", get(unmapped_handler))
            .route_layer(axum::middleware::from_fn_with_state(tokens, admin_rbac))
            .with_state(());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/admin/unmapped")
                    .header(ADMIN_TOKEN_HEADER, "admin")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
