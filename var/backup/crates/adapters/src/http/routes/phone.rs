//! Public phone challenge / verify routes (D32).

use std::sync::Arc;

use application::model::UserId;
use application::money::admin::ComplianceAdminStore;
use application::money::phone_verification::{start_challenge, verify_challenge};
use application::ports::{Clock, PhoneVerification};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use time::Duration;

use super::super::dto::{
    PhoneChallengeDto, PhoneChallengeRequest, PhoneVerifyDto, PhoneVerifyRequest,
};
use super::super::error::{ApiResult, ErrorResponse};

/// State for the public phone routes.
#[derive(Clone)]
pub struct PhoneApiState<S> {
    pub store: S,
    pub clock: Arc<dyn Clock>,
    pub phone: Arc<dyn PhoneVerification>,
    pub hmac_secret: Vec<u8>,
    pub hmac_key_version: i32,
    pub demo_token: String,
}

fn require_demo(headers: &HeaderMap, expected: &str) -> ApiResult<()> {
    let got = headers
        .get("x-demo-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if got == expected {
        Ok(())
    } else {
        Err(ErrorResponse::new(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "missing or invalid x-demo-token",
        ))
    }
}

/// Public challenge/verify router.
pub fn router<S>(state: PhoneApiState<S>) -> Router
where
    S: ComplianceAdminStore + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/phone/challenge", post(challenge::<S>))
        .route("/phone/verify", post(verify::<S>))
        .with_state(state)
}

async fn challenge<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<PhoneApiState<S>>,
    headers: HeaderMap,
    Json(body): Json<PhoneChallengeRequest>,
) -> ApiResult<Json<PhoneChallengeDto>> {
    require_demo(&headers, &state.demo_token)?;
    let now = state.clock.now();
    // The digest this route stores must be the digest of the code the
    // provider actually delivers. The only provider that exists is the
    // staging sandbox, whose code is the pinned constant below; a real
    // vendor adapter must hand its issued code back through `PhoneVerification`
    // before this route can be pointed at it.
    let code = crate::phone::sandbox::SANDBOX_CODE;
    let row = start_challenge(
        &state.store,
        state.phone.as_ref(),
        UserId(body.user_id),
        &body.e164,
        &state.hmac_secret,
        state.hmac_key_version,
        code,
        now,
    )
    .await?;
    Ok(Json(PhoneChallengeDto {
        user_id: body.user_id,
        expires_at: row.expires_at.unwrap_or(now + Duration::minutes(10)),
    }))
}

async fn verify<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<PhoneApiState<S>>,
    headers: HeaderMap,
    Json(body): Json<PhoneVerifyRequest>,
) -> ApiResult<Json<PhoneVerifyDto>> {
    require_demo(&headers, &state.demo_token)?;
    let row = verify_challenge(
        &state.store,
        state.phone.as_ref(),
        UserId(body.user_id),
        &body.code,
        &state.hmac_secret,
        state.clock.now(),
    )
    .await?;
    Ok(Json(PhoneVerifyDto {
        user_id: body.user_id,
        verified: row.verified_at.is_some(),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use application::fakes::{FakeClock, FakeComplianceStore, RecordingPhone};
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    #[tokio::test]
    async fn challenge_and_verify_require_demo_and_succeed() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("phone-route");
        let state = PhoneApiState {
            store,
            clock: Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH)),
            phone: Arc::new(RecordingPhone::default()),
            hmac_secret: b"secret".to_vec(),
            hmac_key_version: 1,
            demo_token: "demo".into(),
        };
        let app = router(state);
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/phone/challenge")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"user_id": user.0, "e164": "+15550001111"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let started = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/phone/challenge")
                    .header("content-type", "application/json")
                    .header("x-demo-token", "demo")
                    .body(Body::from(
                        json!({"user_id": user.0, "e164": "+15550001111"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::OK);

        let verified = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/phone/verify")
                    .header("content-type", "application/json")
                    .header("x-demo-token", "demo")
                    .body(Body::from(
                        json!({"user_id": user.0, "code": "246801"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(verified.status(), StatusCode::OK);
    }
}
