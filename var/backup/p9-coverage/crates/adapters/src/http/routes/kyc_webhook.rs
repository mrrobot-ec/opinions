//! Authenticated KYC provider webhook (D33).

use std::sync::Arc;

use application::error::AppError;
use application::money::admin::{ComplianceAdminStore, InboxOutcome};
use application::ports::{Alerter, Clock};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use time::Duration;

use crate::inbox::{ingest, resolve_user_from_our_records, KycEffect, WebhookDelivery};

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
    // Authenticate BEFORE the provider-session lookup. `ingest` verifies the
    // signature too (it is the module's own contract and this route is not its
    // only caller), but resolving the user first would open a store
    // transaction for an unauthenticated caller AND answer 404 for an unknown
    // provider ref vs 403 for a known one — an enumeration oracle over live
    // KYC provider sessions. The empty-secret guard mirrors `ingest`: an
    // unset secret authenticates nobody, it does not key the HMAC with "".
    if state.secret.is_empty() || !crate::inbox::verify_signature(&state.secret, &body, presented) {
        return Err(ErrorResponse::from(AppError::AdminForbidden(
            "invalid webhook signature",
        )));
    }
    // The KYC fact is validated BEFORE ingestion, so a malformed but correctly
    // signed delivery is refused rather than durably accepted. Defaulting a
    // missing or unparseable `to_tier` to 2 would grant FULL KYC — the highest
    // tier, gating deposits and withdrawals — on a payload that never asked for
    // it, and a provider that renames the field or ships a string would silently
    // promote every user it mentions. `users.kyc_tier` is `0=none|1=basic|2=full`
    // (D33), so anything outside that range is unprocessable, never clamped.
    let effect = KycEffect {
        to_tier: kyc_tier(&parsed)?,
        valid_until: Some(state.clock.now() + Duration::days(30)),
        policy_version: policy_version(&parsed)?,
    };
    let user = resolve_user_from_our_records(&state.store, provider_ref).await?;
    // ONE transaction: the acceptance and the tier change commit together, so a
    // failure cannot leave the delivery durably "seen" with nothing applied and
    // turn every retry into a permanent `replay`.
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
        effect,
        state.clock.now(),
    )
    .await?;
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

/// `to_tier` must be present and be one of the three documented tiers.
fn kyc_tier(parsed: &Value) -> ApiResult<i32> {
    parsed
        .get("to_tier")
        .and_then(Value::as_i64)
        .and_then(|tier| i32::try_from(tier).ok())
        .filter(|tier| (0..=2).contains(tier))
        .ok_or_else(|| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "InvalidWebhook",
                "to_tier must be an integer in 0..=2",
            )
        })
}

/// The policy version is stamped onto the persisted KYC fact and is what makes
/// a verdict re-screenable later, so a blank one is not a usable default.
fn policy_version(parsed: &Value) -> ApiResult<String> {
    parsed
        .get("policy_version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "InvalidWebhook",
                "policy_version must be a non-blank string",
            )
        })
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
        // `policy_version` is mandatory: it is stamped onto the persisted KYC
        // fact and is what makes the verdict re-screenable later. The malformed
        // cases have their own test.
        let body = json!({
            "event_id": "e1",
            "provider": "persona",
            "provider_ref": "sess",
            "to_tier": 2,
            "policy_version": "7"
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

    async fn tier_of(store: &FakeComplianceStore, user: application::model::UserId) -> i32 {
        let mut tx = application::money::ComplianceStore::compliance_tx(store)
            .await
            .unwrap();
        let row = tx.lock_user(user).await.unwrap();
        row.kyc_tier
    }

    fn signed(secret: &[u8], body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/webhooks/kyc")
            .header("content-type", "application/json")
            .header(
                "x-kyc-signature",
                signature_hex(secret, body.as_bytes()).unwrap(),
            )
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// Store whose EFFECT transaction fails a bounded number of times, then
    /// works. The acceptance and the effect must be ONE unit of work: if they
    /// are two, the first delivery durably records "seen", the effect is lost,
    /// and — because the replay algebra has no notion of "applied" — every
    /// retry answers `replay`. The tier is never set and the provider is told
    /// 200. That failure leaves no error anywhere to find.
    #[derive(Clone)]
    struct EffectFailsOnce {
        inner: Arc<FakeComplianceStore>,
        failures_left: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl application::money::ComplianceStore for EffectFailsOnce {
        async fn compliance_tx(
            &self,
        ) -> Result<Box<dyn application::money::ComplianceTx + '_>, application::error::StoreError>
        {
            if self
                .failures_left
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |left| (left > 0).then(|| left - 1),
                )
                .is_ok()
            {
                return Err(application::error::StoreError::Unavailable(
                    "kyc effect transaction",
                ));
            }
            application::money::ComplianceStore::compliance_tx(&*self.inner).await
        }
    }

    #[async_trait::async_trait]
    impl ComplianceAdminStore for EffectFailsOnce {
        async fn admin_tx(
            &self,
        ) -> Result<
            Box<dyn application::money::admin::ComplianceAdminTx + '_>,
            application::error::StoreError,
        > {
            ComplianceAdminStore::admin_tx(&*self.inner).await
        }
    }

    /// codex-p7r1:75 — "commit a directly coupled effect in the same DB
    /// transaction where possible". If the acceptance can outlive a failed
    /// effect, the provider's retry is answered `replay` forever.
    #[tokio::test]
    async fn a_failed_effect_does_not_durably_accept_the_delivery() {
        let inner = Arc::new(FakeComplianceStore::new());
        let user = inner.add_user("atomic");
        remember_provider_user(&*inner, "sess-atomic", user)
            .await
            .unwrap();
        let secret = b"whsec";
        let store = EffectFailsOnce {
            inner: Arc::clone(&inner),
            failures_left: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        };
        let app = router(KycWebhookState {
            store,
            clock: Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH)),
            alerter: Arc::new(UnavailableMoney),
            secret: secret.to_vec(),
        });
        let body = json!({
            "event_id": "e-atomic",
            "provider": "persona",
            "provider_ref": "sess-atomic",
            "to_tier": 2,
            "policy_version": "7"
        })
        .to_string();

        let failed = app.clone().oneshot(signed(secret, &body)).await.unwrap();
        assert!(
            failed.status().is_server_error(),
            "a failed effect must surface as an error, not a success"
        );
        assert_eq!(
            tier_of(&inner, user).await,
            0,
            "nothing was applied by the failed delivery"
        );

        // The provider retries the identical delivery. It must still be
        // ACCEPTED, because the first attempt committed nothing.
        let retried = app.oneshot(signed(secret, &body)).await.unwrap();
        assert_eq!(retried.status(), StatusCode::OK);
        assert_eq!(
            ack(retried).await,
            "accepted",
            "the retry must not be answered `replay` over an effect that never happened"
        );
        assert_eq!(
            tier_of(&inner, user).await,
            2,
            "the retry applied the tier the provider asked for"
        );
    }

    /// A correctly signed payload still has to say something valid. Defaulting
    /// a missing or malformed `to_tier` to 2 grants FULL KYC — the tier that
    /// gates deposits and withdrawals — on a payload that never asked for it.
    #[tokio::test]
    async fn a_signed_delivery_with_a_malformed_effect_is_unprocessable_and_changes_nothing() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("malformed");
        remember_provider_user(&store, "sess-bad", user)
            .await
            .unwrap();
        let secret = b"whsec";
        let app = router(KycWebhookState {
            store: store.clone(),
            clock: Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH)),
            alerter: Arc::new(UnavailableMoney),
            secret: secret.to_vec(),
        });

        let base = |extra: serde_json::Value| {
            let mut value = json!({
                "event_id": "e-bad",
                "provider": "persona",
                "provider_ref": "sess-bad"
            });
            for (key, item) in extra.as_object().unwrap() {
                value[key] = item.clone();
            }
            value.to_string()
        };
        for (label, extra) in [
            ("to_tier absent", json!({ "policy_version": "7" })),
            (
                "to_tier a string",
                json!({ "to_tier": "2", "policy_version": "7" }),
            ),
            (
                "to_tier above the documented range",
                json!({ "to_tier": 3, "policy_version": "7" }),
            ),
            (
                "to_tier negative",
                json!({ "to_tier": -1, "policy_version": "7" }),
            ),
            (
                "to_tier beyond i32",
                json!({ "to_tier": 4_294_967_296_i64, "policy_version": "7" }),
            ),
            ("policy_version absent", json!({ "to_tier": 2 })),
            (
                "policy_version blank",
                json!({ "to_tier": 2, "policy_version": "   " }),
            ),
            (
                "policy_version not a string",
                json!({ "to_tier": 2, "policy_version": 7 }),
            ),
        ] {
            let body = base(extra);
            let response = app.clone().oneshot(signed(secret, &body)).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{label} must be refused"
            );
            assert_eq!(tier_of(&store, user).await, 0, "{label} changed the tier");
        }

        // ...and the well-formed delivery still works, so the guard rejects the
        // payload rather than the route.
        let good = base(json!({ "to_tier": 1, "policy_version": "7" }));
        let response = app.oneshot(signed(secret, &good)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(tier_of(&store, user).await, 1);
    }

    /// Store double that refuses EVERY transaction. Any access proves the
    /// handler reached the database before it authenticated the caller.
    #[derive(Clone, Copy)]
    struct NoStoreAccess;

    #[async_trait::async_trait]
    impl application::money::ComplianceStore for NoStoreAccess {
        async fn compliance_tx(
            &self,
        ) -> Result<Box<dyn application::money::ComplianceTx + '_>, application::error::StoreError>
        {
            Err(application::error::StoreError::Unavailable(
                "store touched before signature verification",
            ))
        }
    }

    #[async_trait::async_trait]
    impl ComplianceAdminStore for NoStoreAccess {
        async fn admin_tx(
            &self,
        ) -> Result<
            Box<dyn application::money::admin::ComplianceAdminTx + '_>,
            application::error::StoreError,
        > {
            Err(application::error::StoreError::Unavailable(
                "store touched before signature verification",
            ))
        }
    }

    fn signed_request(sig: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/webhooks/kyc")
            .header("content-type", "application/json");
        if let Some(sig) = sig {
            builder = builder.header("x-kyc-signature", sig);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    fn hook_body(provider_ref: &str) -> String {
        json!({
            "event_id": "e-oracle",
            "provider": "persona",
            "provider_ref": provider_ref,
            "to_tier": 2
        })
        .to_string()
    }

    /// D33 / `inbox.rs` contract: "Signature is verified here." An
    /// unauthenticated delivery must be refused BEFORE the provider-session
    /// lookup, so it can neither reach the database nor reveal whether a
    /// provider ref is one of ours.
    #[tokio::test]
    async fn an_unsigned_webhook_is_refused_before_any_store_access() {
        let app = router(KycWebhookState {
            store: NoStoreAccess,
            clock: Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH)),
            alerter: Arc::new(UnavailableMoney),
            secret: b"whsec".to_vec(),
        });
        for sig in [None, Some("deadbeef")] {
            let response = app
                .clone()
                .oneshot(signed_request(sig, &hook_body("any-ref")))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "an unauthenticated delivery must never open a store transaction"
            );
        }
    }

    /// The status/body pair must be identical for a provider ref we know and
    /// one we do not: otherwise an unauthenticated caller can enumerate
    /// live KYC provider sessions.
    #[tokio::test]
    async fn an_unsigned_webhook_cannot_distinguish_a_known_provider_ref() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("oracle");
        remember_provider_user(&store, "known-ref", user)
            .await
            .unwrap();
        let app = router(KycWebhookState {
            store,
            clock: Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH)),
            alerter: Arc::new(UnavailableMoney),
            secret: b"whsec".to_vec(),
        });
        let mut seen = Vec::new();
        for provider_ref in ["known-ref", "unknown-ref"] {
            let response = app
                .clone()
                .oneshot(signed_request(Some("deadbeef"), &hook_body(provider_ref)))
                .await
                .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .unwrap();
            seen.push((status, bytes.to_vec()));
        }
        assert_eq!(
            seen[0].0,
            StatusCode::FORBIDDEN,
            "a bad signature is a forbidden delivery, not a lookup miss"
        );
        assert_eq!(
            seen[0], seen[1],
            "known and unknown provider refs must be indistinguishable without a valid signature"
        );
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
