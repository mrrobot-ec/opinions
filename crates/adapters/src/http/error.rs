//! HTTP error envelope — maps `AppError` / validation failures to statuses.

use application::error::{AppError, StoreError};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// One envelope for every error response: `{ "code", "message" }`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Status + body pair returned by handlers.
pub type ApiResult<T> = Result<T, ErrorResponse>;

/// Own type so we can implement `From<AppError>` / `IntoResponse` without
/// orphan-rule violations on foreign tuples.
#[derive(Debug)]
pub struct ErrorResponse {
    pub status: StatusCode,
    pub body: ApiError,
}

impl ErrorResponse {
    #[must_use]
    pub fn new(status: StatusCode, code: &str, message: &str) -> Self {
        Self {
            status,
            body: ApiError::new(code, message),
        }
    }
}

impl From<AppError> for ErrorResponse {
    fn from(err: AppError) -> Self {
        let (status, code) = match &err {
            AppError::MarketNotOpen => (StatusCode::CONFLICT, "MarketNotOpen"),
            AppError::TradingFrozen => (StatusCode::LOCKED, "TradingFrozen"), // 423
            AppError::VoteRequired => (StatusCode::BAD_REQUEST, "VoteRequired"),
            AppError::InsufficientShares => {
                (StatusCode::UNPROCESSABLE_ENTITY, "InsufficientShares")
            }
            AppError::PositionCapExceeded { .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "PositionCapExceeded")
            }
            AppError::SeedFeeBelowMinimum { .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "SeedFeeBelowMinimum")
            }
            AppError::InsufficientFunds => (StatusCode::PAYMENT_REQUIRED, "InsufficientFunds"), // 402
            AppError::AlreadyVoted => (StatusCode::CONFLICT, "AlreadyVoted"), // 409
            AppError::VotingClosed => (StatusCode::LOCKED, "VotingClosed"),   // 423
            AppError::InvalidCrowdGuess => (StatusCode::UNPROCESSABLE_ENTITY, "InvalidCrowdGuess"),
            AppError::PhoneVerificationRequired => {
                (StatusCode::FORBIDDEN, "PhoneVerificationRequired")
            }
            AppError::VoteVelocityExceeded => {
                (StatusCode::TOO_MANY_REQUESTS, "VoteVelocityExceeded")
            }
            AppError::AccountTooYoungNearClose => {
                (StatusCode::UNPROCESSABLE_ENTITY, "AccountTooYoungNearClose")
            }
            AppError::CommentNotVisible => (StatusCode::CONFLICT, "CommentNotVisible"),
            AppError::CommentAlreadyVoted => (StatusCode::CONFLICT, "CommentAlreadyVoted"),
            AppError::InvalidCommentVote => {
                (StatusCode::UNPROCESSABLE_ENTITY, "InvalidCommentVote")
            }
            AppError::ThreadTooDeep => (StatusCode::UNPROCESSABLE_ENTITY, "ThreadTooDeep"),
            AppError::ReporterNotQualified => (StatusCode::FORBIDDEN, "ReporterNotQualified"),
            AppError::ReportVelocityExceeded => {
                (StatusCode::TOO_MANY_REQUESTS, "ReportVelocityExceeded")
            }
            AppError::CommentBlocked(_) => (StatusCode::UNPROCESSABLE_ENTITY, "CommentBlocked"),
            AppError::InvalidDraft(_) => (StatusCode::UNPROCESSABLE_ENTITY, "InvalidDraft"),
            AppError::DraftNotPending => (StatusCode::CONFLICT, "DraftNotPending"),
            AppError::DraftNotApproved => (StatusCode::CONFLICT, "DraftNotApproved"),
            AppError::DraftExpired => (StatusCode::CONFLICT, "DraftExpired"),
            AppError::PendingDraftLimit => (StatusCode::CONFLICT, "PendingDraftLimit"),
            AppError::NoSlotFree => (StatusCode::CONFLICT, "NoSlotFree"),
            AppError::DailySeedBudgetExceeded => (StatusCode::CONFLICT, "DailySeedBudgetExceeded"),
            AppError::JobNotReady => (StatusCode::CONFLICT, "JobNotReady"),
            AppError::ArtifactNotFound => (StatusCode::NOT_FOUND, "ArtifactNotFound"),
            AppError::IllegalTransition => (StatusCode::CONFLICT, "IllegalTransition"), // 409
            // Financial lifecycle events must not use /advance (Task 1.4b).
            AppError::UseResolveMarket => (StatusCode::UNPROCESSABLE_ENTITY, "UseResolveMarket"), // 422
            AppError::NeedsCuratorDecision => (StatusCode::CONFLICT, "NeedsCuratorDecision"), // 409
            AppError::CuratorOverrideNotAllowed => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "CuratorOverrideNotAllowed",
            ),
            AppError::UnderReview => (StatusCode::LOCKED, "UnderReview"),
            AppError::CuratorRequired => (StatusCode::CONFLICT, "CuratorRequired"),
            AppError::LpPaused => (StatusCode::SERVICE_UNAVAILABLE, "LpPaused"),
            // Phase 6 ops vocabulary (D24/D25/D25a/D30).
            AppError::StaleConfig { .. } => (StatusCode::CONFLICT, "StaleConfig"), // 409
            AppError::IdempotencyConflict => (StatusCode::CONFLICT, "IdempotencyConflict"), // 409
            AppError::TradingPaused => (StatusCode::LOCKED, "TradingPaused"),      // 423
            AppError::VotingPaused => (StatusCode::LOCKED, "VotingPaused"),        // 423
            AppError::ProposalConflict(_) => (StatusCode::CONFLICT, "ProposalConflict"), // 409
            AppError::ReceivableOpen { .. } => (StatusCode::CONFLICT, "ReceivableOpen"), // 409
            AppError::AdminForbidden(_) => (StatusCode::FORBIDDEN, "AdminForbidden"),
            AppError::ConfigInvalid { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "ConfigInvalid"),
            AppError::ExpectedConfigVersionRequired => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "ExpectedConfigVersionRequired",
            ),
            AppError::MoneyForbidden(_) => (StatusCode::FORBIDDEN, "MoneyForbidden"),
            AppError::DepositsPaused => (StatusCode::LOCKED, "DepositsPaused"),
            AppError::ComplianceHold { .. } => (StatusCode::CONFLICT, "ComplianceHold"),
            AppError::InsufficientBonusReserve { .. } => {
                (StatusCode::CONFLICT, "InsufficientBonusReserve")
            }
            AppError::ReferralIneligible => {
                (StatusCode::UNPROCESSABLE_ENTITY, "ReferralIneligible")
            }
            AppError::Overflow => (StatusCode::UNPROCESSABLE_ENTITY, "Overflow"),
            AppError::Amm(_) => (StatusCode::UNPROCESSABLE_ENTITY, "AmmError"),
            AppError::Resolution(_) => (StatusCode::UNPROCESSABLE_ENTITY, "ResolutionError"),
            AppError::Scoring(_) => (StatusCode::UNPROCESSABLE_ENTITY, "ScoringError"),
            AppError::Store(StoreError::NotFound(_)) => (StatusCode::NOT_FOUND, "NotFound"),
            AppError::Store(StoreError::DuplicateKey) => (StatusCode::CONFLICT, "DuplicateKey"),
            AppError::Store(StoreError::Conflict(_)) => (StatusCode::CONFLICT, "Conflict"),
            AppError::Store(StoreError::Ledger(_)) => {
                (StatusCode::PAYMENT_REQUIRED, "InsufficientFunds")
            }
            AppError::Store(StoreError::Invariant(_)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Invariant")
            }
            AppError::Store(StoreError::Integrity(_)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Integrity")
            }
            AppError::Store(StoreError::Backend(_)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Backend")
            }
            AppError::Store(StoreError::Unavailable(_)) => {
                (StatusCode::SERVICE_UNAVAILABLE, "Unavailable")
            }
        };
        Self {
            status,
            body: ApiError::new(code, &err.to_string()),
        }
    }
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use domain::amm::AmmError;
    use domain::ledger::LedgerError;
    use domain::resolution::ResolutionError;
    use domain::scoring::ScoringError;

    use super::*;

    #[test]
    fn every_application_error_has_a_stable_http_mapping() {
        assert_eq!(
            ErrorResponse::from(AppError::AccountTooYoungNearClose).status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ErrorResponse::from(AppError::AdminForbidden("ops.pause")).status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ErrorResponse::from(AppError::ConfigInvalid {
                key: "trading.fee_bps".to_string(),
                reason: "out of range",
            })
            .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        for (error, status, code) in [
            (
                AppError::ExpectedConfigVersionRequired,
                StatusCode::UNPROCESSABLE_ENTITY,
                "ExpectedConfigVersionRequired",
            ),
            (
                AppError::MoneyForbidden("account is banned"),
                StatusCode::FORBIDDEN,
                "MoneyForbidden",
            ),
            (
                AppError::DepositsPaused,
                StatusCode::LOCKED,
                "DepositsPaused",
            ),
            (
                AppError::ComplianceHold {
                    reason: "sanctions hit",
                },
                StatusCode::CONFLICT,
                "ComplianceHold",
            ),
            (
                AppError::InsufficientBonusReserve {
                    reserve_micro: 4,
                    promised_micro: 5,
                },
                StatusCode::CONFLICT,
                "InsufficientBonusReserve",
            ),
            (
                AppError::ReferralIneligible,
                StatusCode::UNPROCESSABLE_ENTITY,
                "ReferralIneligible",
            ),
        ] {
            let response = ErrorResponse::from(error);
            assert_eq!(response.status, status);
            assert_eq!(response.body.code, code);
            assert!(!response.body.message.is_empty());
        }
        let errors = [
            AppError::MarketNotOpen,
            AppError::TradingFrozen,
            AppError::VoteRequired,
            AppError::InsufficientShares,
            AppError::PositionCapExceeded {
                cap_micro: 42,
                tier: 1,
            },
            AppError::SeedFeeBelowMinimum {
                base_bps: 1,
                min_bps: 2,
            },
            AppError::InsufficientFunds,
            AppError::AlreadyVoted,
            AppError::VotingClosed,
            AppError::InvalidCrowdGuess,
            AppError::PhoneVerificationRequired,
            AppError::VoteVelocityExceeded,
            AppError::AccountTooYoungNearClose,
            AppError::CommentNotVisible,
            AppError::CommentAlreadyVoted,
            AppError::InvalidCommentVote,
            AppError::ThreadTooDeep,
            AppError::ReporterNotQualified,
            AppError::ReportVelocityExceeded,
            AppError::CommentBlocked("empty"),
            AppError::InvalidDraft("empty"),
            AppError::DraftNotPending,
            AppError::DraftNotApproved,
            AppError::DraftExpired,
            AppError::PendingDraftLimit,
            AppError::NoSlotFree,
            AppError::DailySeedBudgetExceeded,
            AppError::JobNotReady,
            AppError::ArtifactNotFound,
            AppError::IllegalTransition,
            AppError::UseResolveMarket,
            AppError::NeedsCuratorDecision,
            AppError::CuratorOverrideNotAllowed,
            AppError::UnderReview,
            AppError::CuratorRequired,
            AppError::LpPaused,
            AppError::StaleConfig {
                preview_generation: 1,
                current_generation: 2,
            },
            AppError::IdempotencyConflict,
            AppError::TradingPaused,
            AppError::VotingPaused,
            AppError::ProposalConflict("base generation moved"),
            AppError::ReceivableOpen {
                outstanding_micro: 50_000_000,
            },
            AppError::AdminForbidden("ops.pause"),
            AppError::ConfigInvalid {
                key: "trading.fee_bps".to_string(),
                reason: "out of range",
            },
            AppError::Overflow,
            AppError::Amm(AmmError::AmountTooSmall),
            AppError::Resolution(ResolutionError::ActualOutOfRange),
            AppError::Scoring(ScoringError::GuessOutOfRange),
            AppError::Store(StoreError::NotFound("row")),
            AppError::Store(StoreError::DuplicateKey),
            AppError::Store(StoreError::Conflict("row")),
            AppError::Store(StoreError::Ledger(LedgerError::Overflow)),
            AppError::Store(StoreError::Invariant("state")),
            AppError::Store(StoreError::Integrity("constraint".to_string())),
            AppError::Store(StoreError::Backend("offline".to_string())),
            AppError::Store(StoreError::Unavailable("phase5:test")),
        ];
        for error in errors {
            let response = ErrorResponse::from(error);
            assert!(response.status.is_client_error() || response.status.is_server_error());
            assert!(!response.body.code.is_empty());
            assert!(!response.body.message.is_empty());
            let _ = response.into_response();
        }
    }
}
