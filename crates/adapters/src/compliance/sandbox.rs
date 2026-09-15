//! Sandbox `KycProvider` / `SanctionsScreen` / `GeoResolver`, armed by the
//! same two factors as the staging faucet: `OPINIONS_ENV=staging` AND
//! `STAGING_FAUCET=1`. Unarmed every call is
//! [`StoreError::Unavailable`], so a production process that mounts this by
//! accident denies money mutations instead of clearing them.
//!
//! The geo adapter does **not** invent a verdict: it feeds the simulated
//! region through the real D33 predicate, so an absent `region_allowset`
//! still means deny-all here exactly as it does in production.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use application::error::StoreError;
use application::model::UserId;
use application::money::geo::{evaluate_ip_geo, Allowset, GeoIp, Region};
use application::ports::{Clock, GeoResolver, KycProvider, SanctionsScreen, ScreenVerdict};
use async_trait::async_trait;
use time::Duration;

pub use crate::phone::sandbox::staging_two_factor_from_env;

/// How long a sandbox Clear stays fresh. Short enough that the send-boundary
/// re-screen (D33) is exercised by any staging run that idles.
pub const SANDBOX_SCREEN_TTL: Duration = Duration::hours(24);

/// Policy version stamped on every sandbox verdict; never a production
/// counsel version, so a leaked staging fact is identifiable.
pub const SANDBOX_POLICY_VERSION: &str = "sandbox";

/// One armed sandbox providing all three screening roles.
pub struct SandboxCompliance {
    armed: bool,
    clock: Arc<dyn Clock>,
    allowset: Allowset,
    /// The region every client IP resolves to in this staging run. `None`
    /// simulates a resolver miss (deny).
    region: Option<Region>,
    ttl: Duration,
    hits: Mutex<HashSet<UserId>>,
}

impl SandboxCompliance {
    /// Build from the process environment plus the live `region_allowset`
    /// catalog value (absent ⇒ deny-all, per D24/D33).
    #[must_use]
    pub fn from_env(
        clock: Arc<dyn Clock>,
        region_allowset: Option<&serde_json::Value>,
        region_allowset_version: i64,
        region: Option<Region>,
    ) -> Self {
        Self::new(
            staging_two_factor_from_env(),
            clock,
            Allowset::from_config(region_allowset, region_allowset_version),
            region,
        )
    }

    #[must_use]
    pub fn new(
        armed: bool,
        clock: Arc<dyn Clock>,
        allowset: Allowset,
        region: Option<Region>,
    ) -> Self {
        Self {
            armed,
            clock,
            allowset,
            region,
            ttl: SANDBOX_SCREEN_TTL,
            hits: Mutex::new(HashSet::new()),
        }
    }

    /// Staging fixture: make this user screen as a sanctions Hit, so the
    /// red-team legs (frozen funds, no auto-refund) have a subject.
    pub fn plant_hit(&self, user: UserId) {
        self.hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(user);
    }

    fn is_hit(&self, user: UserId) -> bool {
        self.hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&user)
    }

    fn guard(&self, area: &'static str) -> Result<(), StoreError> {
        if self.armed {
            Ok(())
        } else {
            Err(StoreError::Unavailable(area))
        }
    }
}

#[async_trait]
impl KycProvider for SandboxCompliance {
    async fn start_verification(&self, user: UserId) -> Result<String, StoreError> {
        self.guard("phase7:kyc")?;
        // The reference is what the webhook route maps back to OUR user, so
        // it has to be stable per user and unguessable-shaped.
        Ok(format!("sandbox-kyc:{}", user.0))
    }
}

#[async_trait]
impl SanctionsScreen for SandboxCompliance {
    async fn screen(&self, user: UserId, context: &str) -> Result<ScreenVerdict, StoreError> {
        self.guard("phase7:sanctions")?;
        let _ = context;
        if self.is_hit(user) {
            return Ok(ScreenVerdict::Hit);
        }
        let now = self.clock.now();
        Ok(ScreenVerdict::Clear {
            checked_at: now,
            expires_at: now + self.ttl,
            policy_version: SANDBOX_POLICY_VERSION.to_string(),
        })
    }
}

#[async_trait]
impl GeoResolver for SandboxCompliance {
    async fn resolve(&self, ip: IpAddr) -> Result<ScreenVerdict, StoreError> {
        self.guard("phase7:geo")?;
        let now = self.clock.now();
        Ok(evaluate_ip_geo(
            GeoIp::Client(ip),
            Ok(self.region.clone()),
            &self.allowset,
            now,
            self.ttl,
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use application::fakes::FakeClock;
    use serde_json::json;
    use uuid::Uuid;

    fn clock() -> Arc<dyn Clock> {
        Arc::new(FakeClock::at(
            time::OffsetDateTime::UNIX_EPOCH + Duration::days(20_200),
        ))
    }

    fn ca() -> Region {
        Region {
            country: "US".into(),
            state: "CA".into(),
        }
    }

    fn ip() -> IpAddr {
        "203.0.113.10".parse().unwrap()
    }

    /// Set or clear one variable — drives the arm and restores the process
    /// environment exactly as it was found.
    fn put(key: &str, value: Option<&str>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[tokio::test]
    async fn an_unarmed_sandbox_denies_every_role() {
        let cold = SandboxCompliance::new(
            false,
            clock(),
            Allowset::from_config(Some(&json!(["CA"])), 3),
            Some(ca()),
        );
        let user = UserId(Uuid::new_v4());
        assert!(matches!(
            cold.start_verification(user).await,
            Err(StoreError::Unavailable("phase7:kyc"))
        ));
        assert!(matches!(
            cold.screen(user, "withdraw").await,
            Err(StoreError::Unavailable("phase7:sanctions"))
        ));
        assert!(matches!(
            cold.resolve(ip()).await,
            Err(StoreError::Unavailable("phase7:geo"))
        ));
    }

    #[tokio::test]
    async fn an_armed_sandbox_clears_until_a_hit_is_planted() {
        let armed = SandboxCompliance::new(
            true,
            clock(),
            Allowset::from_config(Some(&json!(["ca"])), 3),
            Some(ca()),
        );
        let user = UserId(Uuid::new_v4());
        assert_eq!(
            armed.start_verification(user).await.unwrap(),
            format!("sandbox-kyc:{}", user.0)
        );
        let verdict = armed.screen(user, "withdraw").await.unwrap();
        assert!(matches!(
            verdict,
            ScreenVerdict::Clear { ref policy_version, .. } if policy_version == SANDBOX_POLICY_VERSION
        ));
        armed.plant_hit(user);
        assert_eq!(
            armed.screen(user, "withdraw").await.unwrap(),
            ScreenVerdict::Hit
        );

        // Geo goes through the real predicate: allowed state clears, and the
        // version stamped is the allowset's, not the sandbox's.
        assert!(matches!(
            armed.resolve(ip()).await.unwrap(),
            ScreenVerdict::Clear { ref policy_version, .. } if policy_version == "3"
        ));
    }

    #[tokio::test]
    async fn geo_still_denies_on_an_absent_allowset_a_missed_region_or_a_blocked_state() {
        let user_region =
            |allowset, region| SandboxCompliance::new(true, clock(), allowset, region);
        // Absent region_allowset = deny-all (D24 seed).
        let deny_all = user_region(Allowset::from_config(None, 1), Some(ca()));
        assert_eq!(
            deny_all.resolve(ip()).await.unwrap(),
            ScreenVerdict::Indeterminate
        );
        // Resolver miss.
        let missing = user_region(Allowset::from_config(Some(&json!(["CA"])), 1), None);
        assert_eq!(
            missing.resolve(ip()).await.unwrap(),
            ScreenVerdict::Indeterminate
        );
        // Out-of-allowset state is a Hit, not an Indeterminate.
        let blocked = user_region(Allowset::from_config(Some(&json!(["NY"])), 1), Some(ca()));
        assert_eq!(blocked.resolve(ip()).await.unwrap(), ScreenVerdict::Hit);
    }

    #[test]
    fn the_env_constructor_shares_the_faucet_arm() {
        let saved_env = std::env::var("OPINIONS_ENV").ok();
        let saved_faucet = std::env::var("STAGING_FAUCET").ok();
        put("OPINIONS_ENV", Some("staging"));
        put("STAGING_FAUCET", Some("1"));
        assert!(SandboxCompliance::from_env(clock(), Some(&json!(["CA"])), 4, Some(ca())).armed);
        put("STAGING_FAUCET", None);
        assert!(!SandboxCompliance::from_env(clock(), None, 1, None).armed);
        put("OPINIONS_ENV", saved_env.as_deref());
        put("STAGING_FAUCET", saved_faucet.as_deref());
    }
}
