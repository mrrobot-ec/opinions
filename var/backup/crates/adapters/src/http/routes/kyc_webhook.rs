//! Authenticated KYC provider webhook (D33).

use std::sync::Arc;

use application::money::admin::{ComplianceAdminStore, InboxOutcome};
use application::money::kyc::apply_inboxed_kyc;
use application::ports::{Alerter, Clock};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use time::Duration;

use crate::inbox::{ingest, resolve_user_from_our_records, WebhookDelivery};

use super::super::dto::InboxAckDto;
use super::super::error::{ApiResult, ErrorResponse};

/// Webhook adapter state.
#[derive(Clone)]
pub struct KycWebhookState<S> {
    pub store: S,
    pub clock: Arc<dyn Clock>,
    pub alerter: Arc<dyn Alerter>,
    pub secret: Vec<u8>,
}

/// Provider-authenticated ingest router.
pub fn router<S>(state: KycWebhookState<S>) -> Router
where
    S: ComplianceAdminStore + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/webhooks/kyc", post(ingest_kyc::<S>))
        .with_state(state)
}

async fn ingest_kyc<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<KycWebhookState<S>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Json<InboxAckDto>> {
    let presented = headers
        .get("x-kyc-signature")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let event_id = parsed
        .get("event_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "InvalidWebhook",
                "event_id required",
            )
        })?;
    let provider = parsed
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("kyc");
    let provider_ref = parsed
        .get("provider_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "InvalidWebhook",
                "provider_ref required",
            )
        })?;
    let user = resolve_user_from_our_records(&state.store, provider_ref).await?;
    let outcome = ingest(
        &state.store,
        state.alerter.as_ref(),
        &state.secret,
        WebhookDelivery {
            provider,
            event_id,
            presented_sig: presented,
            body: &body,
        },
        user,
        state.clock.now(),
    )
    .await?;
    if outcome == InboxOutcome::Accepted {
        let to_tier = parsed.get("to_tier").and_then(Value::as_i64).unwrap_or(2);
        let policy = parsed
            .get("policy_version")
            .and_then(Value::as_str)
            .unwrap_or("1")
            .to_string();
        apply_inboxed_kyc(
            &state.store,
            user,
            i32::try_from(to_tier).unwrap_or(2),
            Some(provider_ref.to_string()),
            Some(state.clock.now() + Duration::days(30)),
            policy,
            parsed,
            state.clock.now(),
        )
        .await
        .map_err(application::error::AppError::from)?;
    }
    // `ingest` turns a same-key/different-hash event into a typed 409 plus a
    // page, so only Accepted and Replay ever reach this line.
    let name = if outcome == InboxOutcome::Accepted {
        "accepted"
    } else {
        "replay"
    };
    Ok(Json(InboxAckDto {
        outcome: name.into(),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::inbox::{remember_provider_user, signature_hex};
    use application::fakes::{FakeClock, FakeComplianceStore};
    use application::ports::UnavailableMoney;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    #[tokio::test]
    async fn unsigned_webhook_is_rejected_and_known_user_is_accepted() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("hook");
        remember_provider_user(&store, "sess", user).await.unwrap();
        let secret = b"whsec";
        let state = KycWebhookState {
            store,
            clock: Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH)),
            alerter: Arc::new(UnavailableMoney),
            secret: secret.to_vec(),
        };
        let app = router(state);
        let body = json!({
            "event_id": "e1",
            "provider": "persona",
            "provider_ref": "sess",
            "to_tier": 2
        })
        .to_string();
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/kyc")
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let sig = signature_hex(secret, body.as_bytes()).unwrap();
        let ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/kyc")
                    .header("content-type", "application/json")
                    .header("x-kyc-signature", sig.clone())
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ack(ok).await, "accepted");

        // The same signed event again is an idempotent replay, not a second
        // tier change.
        let replay = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/kyc")
                    .header("content-type", "application/json")
                    .header("x-kyc-signature", sig)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(ack(replay).await, "replay");
    }

    async fn ack(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["outcome"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn a_webhook_without_the_fields_that_name_our_user_is_unprocessable() {
        let store = FakeComplianceStore::new();
        let state = KycWebhookState {
            store,
            clock: Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH)),
            alerter: Arc::new(UnavailableMoney),
            secret: b"whsec".to_vec(),
        };
        let app = router(state);
        for body in [
            json!({ "provider": "persona", "provider_ref": "sess" }),
            json!({ "event_id": "e1", "provider": "persona" }),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/webhooks/kyc")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body}"
            );
        }
    }
}
