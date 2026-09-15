//! Authenticated durable KYC webhook inbox (D33).
//!
//! Signature is verified here. The user is resolved from OUR records
//! (provider session → user), never from a client-supplied user id.
//! Persistence uses the W2 inbox algebra: same `(provider, event_id)` +
//! same payload hash returns the original; a different hash is a typed
//! conflict that pages.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use time::OffsetDateTime;

use application::error::{AppError, StoreError};
use application::model::UserId;
use application::money::admin::{
    hex_encode, payload_hash, ComplianceAdminStore, InboxOutcome, InboxRecord,
};
use application::money::kyc::accept_and_apply_inboxed_kyc;
use application::ports::Alerter;
use serde_json::Value;

/// HMAC-SHA256 hex of the raw body, compared in constant time.
#[must_use]
pub fn signature_hex(secret: &[u8], body: &[u8]) -> Option<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
    mac.update(body);
    Some(hex_encode(&mac.finalize().into_bytes()))
}

/// Constant-time equality for equal-length hex strings.
#[must_use]
pub fn signatures_equal(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    expected
        .bytes()
        .zip(presented.bytes())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Verify `presented` against HMAC(secret, body).
#[must_use]
pub fn verify_signature(secret: &[u8], body: &[u8], presented: &str) -> bool {
    signature_hex(secret, body).is_some_and(|expected| signatures_equal(&expected, presented))
}

/// Persist a provider-session → user mapping in OUR records.
///
/// # Errors
/// Store failures.
pub async fn remember_provider_user(
    store: &impl ComplianceAdminStore,
    provider_ref: &str,
    user: UserId,
) -> Result<(), AppError> {
    let mut tx = store.admin_tx().await?;
    tx.set_config_json(
        &format!("kyc_provider_ref:{provider_ref}"),
        serde_json::Value::String(user.0.to_string()),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Resolve the user from a provider session we previously stored.
///
/// # Errors
/// Unknown provider ref — we never trust a user id from the payload.
pub async fn resolve_user_from_our_records(
    store: &impl ComplianceAdminStore,
    provider_ref: &str,
) -> Result<UserId, AppError> {
    let mut tx = store.admin_tx().await?;
    let value = tx
        .config_json(&format!("kyc_provider_ref:{provider_ref}"))
        .await?;
    tx.commit().await?;
    value
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| raw.parse().ok())
        .map(UserId)
        .ok_or(AppError::Store(StoreError::NotFound("provider mapping")))
}

/// One provider delivery, as it arrived on the wire. Grouped so the four
/// adjacent string-ish fields cannot be transposed at a call site.
#[derive(Debug, Clone, Copy)]
pub struct WebhookDelivery<'a> {
    pub provider: &'a str,
    pub event_id: &'a str,
    pub presented_sig: &'a str,
    pub body: &'a [u8],
}

/// The KYC fact a delivery asks us to record. Validated by the caller BEFORE
/// ingestion, so a malformed authenticated delivery is never durably accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KycEffect {
    pub to_tier: i32,
    pub valid_until: Option<OffsetDateTime>,
    pub policy_version: String,
}

/// Ingest one webhook ATOMICALLY: verify, then accept the inbox row and apply
/// the KYC effect in ONE transaction.
///
/// The acceptance and the effect must share a transaction. Committing the
/// inbox row first means a crash — or merely a failure opening the effect
/// transaction — leaves the delivery durably "seen" with nothing applied, and
/// because the replay algebra has no notion of "applied", every later retry
/// returns `Replay`. The tier is never set and the provider is told 200. That
/// is the coupling `docs/reviews/codex-p7r1.md:75` accepted: "commit a directly
/// coupled effect in the same DB transaction where possible".
///
/// # Errors
/// Bad signature, unknown user, hash conflict, store.
pub async fn ingest(
    store: &impl ComplianceAdminStore,
    alerter: &dyn Alerter,
    secret: &[u8],
    delivery: WebhookDelivery<'_>,
    user: UserId,
    effect: KycEffect,
    now: OffsetDateTime,
) -> Result<InboxOutcome, AppError> {
    if secret.is_empty() || !verify_signature(secret, delivery.body, delivery.presented_sig) {
        return Err(AppError::AdminForbidden("invalid webhook signature"));
    }
    let payload: Value = serde_json::from_slice(delivery.body).unwrap_or(Value::Null);
    let record = InboxRecord {
        provider: delivery.provider.to_string(),
        event_id: delivery.event_id.to_string(),
        payload_hash: payload_hash(delivery.body),
        payload,
        user_id: Some(user),
        received_at: now,
    };
    let outcome = accept_and_apply_inboxed_kyc(
        store,
        record,
        effect.to_tier,
        Some(delivery.event_id.to_string()),
        effect.valid_until,
        effect.policy_version,
        now,
    )
    .await;
    if let Err(AppError::ProposalConflict("inbox payload hash conflict")) = &outcome {
        // D33: a same-key/different-hash event is a typed conflict AND a page.
        // A pager that is itself down must not swallow the conflict, so the
        // page failure replaces the response only when it happens.
        alerter
            .page(
                "crit",
                &format!("inbox-conflict:{}:{}", delivery.provider, delivery.event_id),
                "kyc inbox payload hash conflict",
            )
            .await
            .map_err(AppError::Store)?;
    }
    outcome
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use application::fakes::FakeComplianceStore;
    use application::ports::{Alerter, UnavailableMoney};
    use std::sync::Mutex;
    use uuid::Uuid;

    /// Records every page instead of failing, so the conflict path can be
    /// asserted on both the response AND the alert.
    #[derive(Default)]
    struct RecordingAlerter {
        pages: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl Alerter for RecordingAlerter {
        async fn page(
            &self,
            severity: &str,
            key: &str,
            _body: &str,
        ) -> Result<(), application::error::StoreError> {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((severity.to_string(), key.to_string()));
            Ok(())
        }
    }

    #[test]
    fn signature_round_trip_and_reject() {
        let secret = b"whsec";
        let body = b"{\"ok\":true}";
        let sig = signature_hex(secret, body).unwrap();
        assert!(verify_signature(secret, body, &sig));
        assert!(!verify_signature(secret, body, "00"));
        assert!(!verify_signature(secret, b"other", &sig));
        assert!(!verify_signature(b"", body, &sig));
        assert!(!signatures_equal("aa", "ab"));
        assert!(signatures_equal("aa", "aa"));
    }

    #[tokio::test]
    async fn ingest_accepts_replay_and_pages_on_conflict() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("inbox-a");
        let secret = b"whsec";
        let body = br#"{"event":"ok"}"#;
        let sig = signature_hex(secret, body).unwrap();
        let now = OffsetDateTime::UNIX_EPOCH;
        // Every accepted delivery now carries its effect: acceptance and the
        // tier change are one transaction, so `ingest` cannot be called
        // without saying what to apply.
        let effect = || KycEffect {
            to_tier: 2,
            valid_until: None,
            policy_version: "7".into(),
        };
        let evt1 = WebhookDelivery {
            provider: "persona",
            event_id: "evt-1",
            presented_sig: &sig,
            body,
        };
        let first = ingest(&store, &UnavailableMoney, secret, evt1, user, effect(), now)
            .await
            .unwrap();
        assert_eq!(first, InboxOutcome::Accepted);
        let replay = ingest(&store, &UnavailableMoney, secret, evt1, user, effect(), now)
            .await
            .unwrap();
        assert_eq!(replay, InboxOutcome::Replay);
        assert!(ingest(
            &store,
            &UnavailableMoney,
            secret,
            WebhookDelivery {
                event_id: "evt-2",
                presented_sig: "deadbeef",
                ..evt1
            },
            user,
            effect(),
            now
        )
        .await
        .is_err());
        // Same key, different payload: typed conflict AND a page. The pager
        // here succeeds, so the conflict itself is what reaches the caller.
        let other = br#"{"event":"nope"}"#;
        let other_sig = signature_hex(secret, other).unwrap();
        let alerter = RecordingAlerter::default();
        let clashing = WebhookDelivery {
            presented_sig: &other_sig,
            body: other,
            ..evt1
        };
        let conflict = ingest(&store, &alerter, secret, clashing, user, effect(), now).await;
        assert!(matches!(
            conflict,
            Err(AppError::ProposalConflict("inbox payload hash conflict"))
        ));
        assert_eq!(
            alerter
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            [(
                "crit".to_string(),
                "inbox-conflict:persona:evt-1".to_string()
            )]
        );

        // A pager that is down turns the conflict into its own store error
        // rather than reporting success.
        assert!(matches!(
            ingest(
                &store,
                &UnavailableMoney,
                secret,
                clashing,
                user,
                effect(),
                now
            )
            .await,
            Err(AppError::Store(_))
        ));
        let _ = Uuid::nil();
    }

    #[tokio::test]
    async fn unknown_provider_mapping_is_not_found() {
        let store = FakeComplianceStore::new();
        assert!(resolve_user_from_our_records(&store, "missing")
            .await
            .is_err());
        let user = store.add_user("mapped");
        remember_provider_user(&store, "sess-1", user)
            .await
            .unwrap();
        assert_eq!(
            resolve_user_from_our_records(&store, "sess-1")
                .await
                .unwrap(),
            user
        );
    }
}
