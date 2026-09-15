//! Staging-two-factor phone provider, armed like the faucet:
//! `OPINIONS_ENV=staging` AND `STAGING_FAUCET=1`. Off ⇒ fail closed.

use std::collections::HashMap;
use std::sync::Mutex;

use application::error::StoreError;
use application::model::UserId;
use application::ports::PhoneVerification;
use async_trait::async_trait;

/// The one code the sandbox ever issues. Staging e2e and the public
/// challenge route both name this constant instead of repeating a literal;
/// a real vendor adapter issues its own code and must carry the digest with
/// it (see `http/routes/phone.rs`).
pub const SANDBOX_CODE: &str = "246801";

/// Two-factor arm identical to the staging faucet.
#[must_use]
pub fn staging_two_factor_from_env() -> bool {
    std::env::var("OPINIONS_ENV").as_deref() == Ok("staging")
        && std::env::var("STAGING_FAUCET").as_deref() == Ok("1")
}

/// In-process OTP sandbox. Codes are recorded only when armed.
#[derive(Default)]
pub struct SandboxPhone {
    armed: bool,
    codes: Mutex<HashMap<UserId, String>>,
}

impl SandboxPhone {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            armed: staging_two_factor_from_env(),
            codes: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn armed(armed: bool) -> Self {
        Self {
            armed,
            codes: Mutex::new(HashMap::new()),
        }
    }

    /// Last issued sandbox code (tests / staging inspect).
    #[must_use]
    pub fn last_code(&self, user: UserId) -> Option<String> {
        self.codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&user)
            .cloned()
    }
}

#[async_trait]
impl PhoneVerification for SandboxPhone {
    async fn start_challenge(&self, user: UserId, e164: &str) -> Result<(), StoreError> {
        if !self.armed {
            return Err(StoreError::Unavailable("phase7:phone"));
        }
        let _ = e164;
        // Deterministic sandbox code so staging e2e can complete.
        self.codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(user, SANDBOX_CODE.into());
        Ok(())
    }

    async fn verify(&self, user: UserId, code: &str) -> Result<bool, StoreError> {
        if !self.armed {
            return Err(StoreError::Unavailable("phase7:phone"));
        }
        Ok(self.last_code(user).as_deref() == Some(code))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Set or clear one variable — used both to drive the arm and to put the
    /// process environment back exactly as it was found.
    fn put(key: &str, value: Option<&str>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn two_factor_matches_the_faucet_rule() {
        let saved_env = std::env::var("OPINIONS_ENV").ok();
        let saved_faucet = std::env::var("STAGING_FAUCET").ok();
        for (env, faucet, armed) in [
            (None, None, false),
            (Some("staging"), None, false),
            (None, Some("1"), false),
            (Some("staging"), Some("1"), true),
            (Some("production"), Some("1"), false),
            (Some("staging"), Some("0"), false),
        ] {
            put("OPINIONS_ENV", env);
            put("STAGING_FAUCET", faucet);
            assert_eq!(staging_two_factor_from_env(), armed, "{env:?}/{faucet:?}");
            assert_eq!(SandboxPhone::from_env().armed, armed);
        }
        put("OPINIONS_ENV", saved_env.as_deref());
        put("STAGING_FAUCET", saved_faucet.as_deref());
    }

    #[tokio::test]
    async fn armed_sandbox_issues_and_checks_code() {
        let phone = SandboxPhone::armed(true);
        let user = UserId(uuid::Uuid::nil());
        phone.start_challenge(user, "+15550001111").await.unwrap();
        assert_eq!(phone.last_code(user).as_deref(), Some("246801"));
        assert!(phone.verify(user, "246801").await.unwrap());
        assert!(!phone.verify(user, "000000").await.unwrap());
        let cold = SandboxPhone::armed(false);
        assert!(matches!(
            cold.start_challenge(user, "+1").await,
            Err(StoreError::Unavailable("phase7:phone"))
        ));
        assert!(matches!(
            cold.verify(user, "246801").await,
            Err(StoreError::Unavailable("phase7:phone"))
        ));
    }
}
