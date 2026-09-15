//! Environment-driven engine selection with all-or-none validation.
//!
//! `LLM_API_KEY` + `LLM_BASE_URL` must be set together or not at all:
//! - **neither** → typed-unavailable engines for drafts and moderation (jobs sit
//!   queued; enabling the runner without an engine fails startup preflight);
//! - **both, valid** → the real engines over the sole audited transport;
//! - **half-configured or malformed** → the typed [`LlmConfigError`]: the
//!   selected engines refuse every call instead of silently degrading, so
//!   the misconfiguration surfaces at startup preflight (moderation) and on
//!   first curator use (drafts) — never as a silently templated draft while
//!   ops believe the LLM is live.

use std::sync::Arc;
use std::time::Duration;

use application::error::StoreError;
use application::model::{DraftRequest, ModerationVerdict};
use application::ports::{
    DraftEngine, DraftEngineUnavailable, GeneratedDraft, ModerationPreflight,
};
use async_trait::async_trait;

use super::draft::LlmDraftEngine;
use super::moderation::LlmModerationPreflight;
use super::transport::{real_transport, ApiKey, HttpTransport};

const LLM_API_KEY: &str = "LLM_API_KEY";
const LLM_BASE_URL: &str = "LLM_BASE_URL";
const TRANSPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// The typed startup configuration error for a half-configured or malformed
/// LLM environment (all-or-none: set both variables or neither).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmConfigError {
    #[error("LLM_API_KEY is set but LLM_BASE_URL is missing - set both or neither")]
    MissingBaseUrl,
    #[error("LLM_BASE_URL is set but LLM_API_KEY is missing - set both or neither")]
    MissingApiKey,
    #[error("LLM_API_KEY must not be blank")]
    BlankApiKey,
    /// Deliberately message-only: the offending bytes are the credential.
    #[error("LLM_API_KEY cannot be encoded as an HTTP header value")]
    UnencodableApiKey,
    #[error("LLM_BASE_URL is malformed: {0}")]
    MalformedBaseUrl(String),
}

/// A fully validated LLM environment ([`ApiKey`]'s `Debug` stays redacted).
#[derive(Debug)]
struct LlmEnv {
    key: ApiKey,
    base_url: String,
}

/// Pure all-or-none resolution over the two variables' raw values.
fn resolve(
    key: Option<String>,
    base_url: Option<String>,
) -> Result<Option<LlmEnv>, LlmConfigError> {
    match (key, base_url) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(LlmConfigError::MissingBaseUrl),
        (None, Some(_)) => Err(LlmConfigError::MissingApiKey),
        (Some(key), Some(base_url)) => {
            if key.trim().is_empty() {
                return Err(LlmConfigError::BlankApiKey);
            }
            // The exact header the transport will build; a key it cannot
            // encode must fail HERE (startup), not per-request after jobs
            // are claimed. The error never carries the bytes.
            if reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).is_err() {
                return Err(LlmConfigError::UnencodableApiKey);
            }
            Ok(Some(LlmEnv {
                key: ApiKey::new(key),
                base_url: validate_base_url(&base_url)?,
            }))
        }
    }
}

/// Accepts an absolute `http(s)` origin (host required, no query/fragment),
/// normalized without a trailing slash.
fn validate_base_url(raw: &str) -> Result<String, LlmConfigError> {
    let trimmed = raw.trim();
    let parsed = reqwest::Url::parse(trimmed)
        .map_err(|error| LlmConfigError::MalformedBaseUrl(error.to_string()))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(LlmConfigError::MalformedBaseUrl(format!(
            "unsupported scheme {}",
            parsed.scheme()
        )));
    }
    // No separate host check: the parser rejects a host-less http(s) URL
    // (e.g. `http://`) outright, so a parsed special-scheme URL has a host.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        // Never echo the URL here: the userinfo IS a credential, and this
        // message flows into job errors and runner logs.
        return Err(LlmConfigError::MalformedBaseUrl(
            "userinfo is not allowed".to_string(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(LlmConfigError::MalformedBaseUrl(
            "query and fragment are not allowed".to_string(),
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

/// Reads a variable, treating non-unicode values as present (they then fail
/// validation loudly instead of counting as absent).
fn env_string(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

fn llm_env() -> Result<Option<LlmEnv>, LlmConfigError> {
    resolve(env_string(LLM_API_KEY), env_string(LLM_BASE_URL))
}

/// Draft engine whose configuration is broken: it never generates and never
/// silently substitutes the template (the caller decides about fallback).
struct MisconfiguredDraftEngine;

#[async_trait]
impl DraftEngine for MisconfiguredDraftEngine {
    async fn generate(
        &self,
        _request: &DraftRequest,
    ) -> Result<GeneratedDraft, DraftEngineUnavailable> {
        Err(DraftEngineUnavailable)
    }
}

/// Moderation preflight whose configuration is broken: every call (including
/// the availability probe) surfaces the typed configuration error, which
/// fails startup preflight when the runner flag is enabled.
struct MisconfiguredModerationPreflight {
    reason: String,
}

#[async_trait]
impl ModerationPreflight for MisconfiguredModerationPreflight {
    async fn screen(&self, _body: &str) -> Result<ModerationVerdict, StoreError> {
        Err(StoreError::Backend(format!("llm config: {}", self.reason)))
    }
}

/// Joins a validated environment with the (possibly failed) transport
/// construction; a transport failure poisons the engine like any other
/// configuration error.
fn draft_engine_for(
    transport: Result<Arc<dyn HttpTransport>, StoreError>,
    env: &LlmEnv,
) -> Arc<dyn DraftEngine> {
    match transport {
        Ok(transport) => Arc::new(LlmDraftEngine::new(
            transport,
            &env.base_url,
            env.key.clone(),
        )),
        Err(_) => Arc::new(MisconfiguredDraftEngine),
    }
}

/// See [`draft_engine_for`].
fn moderation_preflight_for(
    transport: Result<Arc<dyn HttpTransport>, StoreError>,
    env: &LlmEnv,
) -> Arc<dyn ModerationPreflight> {
    match transport {
        Ok(transport) => Arc::new(LlmModerationPreflight::new(
            transport,
            &env.base_url,
            env.key.clone(),
        )),
        Err(error) => Arc::new(MisconfiguredModerationPreflight {
            reason: error.to_string(),
        }),
    }
}

/// Environment-aware draft engine selection (wired by frozen `main`).
///
/// The injected engine is retained for API compatibility with the frozen
/// wiring, but the application layer owns fallback selection so it can record
/// `fallback_from = llm` durably. Returning the template directly here would
/// mislabel a keyless LLM request as a primary template success.
#[must_use]
pub fn draft_engine_from_env(_fallback: Arc<dyn DraftEngine>) -> Arc<dyn DraftEngine> {
    match llm_env() {
        Ok(Some(env)) => draft_engine_for(real_transport(TRANSPORT_TIMEOUT), &env),
        Ok(None) | Err(_) => Arc::new(MisconfiguredDraftEngine),
    }
}

/// Environment-aware moderation preflight selection (wired by frozen `main`).
#[must_use]
pub fn moderation_preflight_from_env() -> Arc<dyn ModerationPreflight> {
    match llm_env() {
        Ok(None) => Arc::new(application::fakes::UnavailableModerationPreflight),
        Ok(Some(env)) => moderation_preflight_for(real_transport(TRANSPORT_TIMEOUT), &env),
        Err(error) => Arc::new(MisconfiguredModerationPreflight {
            reason: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;

    use application::moderation_escalate::AVAILABILITY_PROBE;
    use domain::drafting::DraftTier;

    use super::*;

    /// Serializes every test that mutates the process environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<T>(key: Option<&str>, base_url: Option<&str>, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Snapshot the process environment so a dev/CI process that started
        // with live LLM variables gets them back after every test.
        let saved_key = std::env::var_os(LLM_API_KEY);
        let saved_base_url = std::env::var_os(LLM_BASE_URL);
        let apply = |name: &str, value: Option<&str>| match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        };
        apply(LLM_API_KEY, key);
        apply(LLM_BASE_URL, base_url);
        let result = body();
        let restore = |name: &str, value: Option<std::ffi::OsString>| match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        };
        restore(LLM_API_KEY, saved_key);
        restore(LLM_BASE_URL, saved_base_url);
        result
    }

    #[test]
    fn with_env_restores_the_pre_test_process_environment() {
        let setup = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var(LLM_API_KEY, "pre-existing");
        std::env::remove_var(LLM_BASE_URL);
        drop(setup);
        with_env(Some("temp"), Some("http://llm.test"), || {
            assert_eq!(std::env::var(LLM_API_KEY).unwrap(), "temp");
        });
        let check = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(std::env::var(LLM_API_KEY).unwrap(), "pre-existing");
        assert!(std::env::var_os(LLM_BASE_URL).is_none());
        std::env::remove_var(LLM_API_KEY);
        drop(check);
    }

    #[test]
    fn resolve_is_all_or_none() {
        assert!(resolve(None, None).unwrap().is_none());
        assert_eq!(
            resolve(Some("k".into()), None).unwrap_err(),
            LlmConfigError::MissingBaseUrl
        );
        assert_eq!(
            resolve(None, Some("http://llm.test".into())).unwrap_err(),
            LlmConfigError::MissingApiKey
        );
        assert_eq!(
            resolve(Some("  ".into()), Some("http://llm.test".into())).unwrap_err(),
            LlmConfigError::BlankApiKey
        );
        let env = resolve(Some("sk".into()), Some("https://llm.test/".into()))
            .unwrap()
            .unwrap();
        assert_eq!(env.base_url, "https://llm.test");
        assert_eq!(env.key.expose_secret(), "sk");
    }

    #[test]
    fn malformed_base_urls_are_typed_config_errors() {
        for raw in [
            "llm.test",
            "ftp://llm.test",
            "http://",
            "http://llm.test?x=1",
            "http://llm.test#frag",
            "http://ops:s3cret-cred@llm.test",
            "http://ops@llm.test",
            "",
        ] {
            let error = resolve(Some("sk".into()), Some(raw.into())).unwrap_err();
            assert!(
                matches!(error, LlmConfigError::MalformedBaseUrl(_)),
                "{raw}: {error:?}"
            );
            // A credential smuggled into the URL must never survive into the
            // error text (it would land in job errors and runner logs).
            assert!(!error.to_string().contains("s3cret-cred"), "{error}");
        }
        // The error text names the variable for the operator.
        let rendered = LlmConfigError::MissingBaseUrl.to_string();
        assert!(rendered.contains("LLM_BASE_URL"));
        assert!(rendered.contains("both or neither"));
    }

    #[test]
    fn header_invalid_api_keys_are_typed_config_errors_without_echoing_the_secret() {
        // A key the transport could never encode as a header must fail at
        // RESOLUTION (startup), not per-request after jobs are claimed —
        // otherwise the local probe passes and every real screen burns an
        // attempt to terminal failure.
        for key in ["sk-top\nsecret", "sk-top\rsecret", "sk-top\0secret"] {
            let error = resolve(Some(key.into()), Some("http://llm.test".into())).unwrap_err();
            assert_eq!(error, LlmConfigError::UnencodableApiKey, "{key:?}");
            assert!(!error.to_string().contains("sk-top"), "{error}");
        }
    }

    #[tokio::test]
    async fn header_invalid_key_env_poisons_both_engines_including_the_probe() {
        let fallback = application::content::draft_engine();
        let (draft, preflight) = with_env(Some("sk\nbad"), Some("http://llm.test"), || {
            (
                draft_engine_from_env(Arc::clone(&fallback)),
                moderation_preflight_from_env(),
            )
        });
        assert!(!Arc::ptr_eq(&fallback, &draft));
        assert_eq!(
            draft
                .generate(&DraftRequest {
                    topic: "t".to_string(),
                    tier: DraftTier::Daily,
                })
                .await
                .unwrap_err(),
            DraftEngineUnavailable
        );
        // The poisoned preflight fails the AVAILABILITY PROBE itself, so an
        // enabled runner refuses at startup and never claims a job.
        let error = preflight.screen(AVAILABILITY_PROBE).await.unwrap_err();
        assert!(
            matches!(
                error,
                StoreError::Backend(ref m)
                    if m.starts_with("llm config:") && m.contains("LLM_API_KEY")
            ),
            "{error:?}"
        );
        assert!(!error.to_string().contains("bad"), "{error:?}");
    }

    #[tokio::test]
    async fn absent_env_selects_unavailable_engines_so_the_application_labels_fallback() {
        let fallback = application::content::draft_engine();
        let (draft, preflight) = with_env(None, None, || {
            (
                draft_engine_from_env(Arc::clone(&fallback)),
                moderation_preflight_from_env(),
            )
        });
        assert!(!Arc::ptr_eq(&fallback, &draft));
        assert_eq!(
            draft
                .generate(&DraftRequest {
                    topic: "rail".into(),
                    tier: DraftTier::Daily,
                })
                .await,
            Err(DraftEngineUnavailable)
        );
        assert_eq!(
            preflight.screen(AVAILABILITY_PROBE).await.unwrap_err(),
            StoreError::Unavailable("phase5:moderation-preflight")
        );
    }

    #[tokio::test]
    async fn full_valid_env_selects_the_real_engines() {
        let fallback = application::content::draft_engine();
        let (draft, preflight) = with_env(Some("sk-live"), Some("http://127.0.0.1:9/"), || {
            (
                draft_engine_from_env(Arc::clone(&fallback)),
                moderation_preflight_from_env(),
            )
        });
        // A distinct engine was selected …
        assert!(!Arc::ptr_eq(&fallback, &draft));
        // … and the configured preflight answers the probe locally.
        assert_eq!(
            preflight.screen(AVAILABILITY_PROBE).await.unwrap(),
            ModerationVerdict::Visible
        );
        // A real body must reach the wire: port 9 never accepts, and the
        // typed transport error proves the real transport was wired in.
        let error = preflight.screen("a real body").await.unwrap_err();
        assert!(matches!(error, StoreError::Backend(ref m) if m.starts_with("llm transport")));
    }

    #[tokio::test]
    async fn half_configured_env_poisons_both_engines_with_the_typed_error() {
        let fallback = application::content::draft_engine();
        let (draft, preflight) = with_env(Some("sk-live"), None, || {
            (
                draft_engine_from_env(Arc::clone(&fallback)),
                moderation_preflight_from_env(),
            )
        });
        assert!(!Arc::ptr_eq(&fallback, &draft));
        assert_eq!(
            draft
                .generate(&DraftRequest {
                    topic: "t".to_string(),
                    tier: DraftTier::Daily,
                })
                .await
                .unwrap_err(),
            DraftEngineUnavailable
        );
        // The probe carries the typed config error: enabling the runner with
        // a half-configured environment fails startup preflight.
        assert_eq!(
            preflight.screen(AVAILABILITY_PROBE).await.unwrap_err(),
            StoreError::Backend(format!("llm config: {}", LlmConfigError::MissingBaseUrl))
        );
    }

    #[tokio::test]
    async fn transport_construction_failure_poisons_both_engines() {
        let env = resolve(Some("sk".into()), Some("http://llm.test".into()))
            .unwrap()
            .unwrap();
        let draft = draft_engine_for(Err(StoreError::Backend("no tls".to_string())), &env);
        assert_eq!(
            draft
                .generate(&DraftRequest {
                    topic: "t".to_string(),
                    tier: DraftTier::Daily,
                })
                .await
                .unwrap_err(),
            DraftEngineUnavailable
        );
        let preflight =
            moderation_preflight_for(Err(StoreError::Backend("no tls".to_string())), &env);
        assert_eq!(
            preflight.screen(AVAILABILITY_PROBE).await.unwrap_err(),
            StoreError::Backend("llm config: backend failure: no tls".to_string())
        );
    }

    #[tokio::test]
    async fn malformed_url_env_poisons_the_preflight_with_the_typed_error() {
        let preflight = with_env(Some("sk-live"), Some("not a url"), || {
            moderation_preflight_from_env()
        });
        let error = preflight.screen(AVAILABILITY_PROBE).await.unwrap_err();
        assert!(
            matches!(error, StoreError::Backend(ref m) if m.starts_with("llm config: LLM_BASE_URL is malformed")),
            "{error:?}"
        );
    }
}
