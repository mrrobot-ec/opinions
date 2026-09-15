//! Versioned US-state allowlist and `TRUSTED_PROXY_CIDRS` client-IP rule (D33).

use std::collections::BTreeSet;
use std::net::IpAddr;

use time::OffsetDateTime;

use crate::ports::ScreenVerdict;

/// ISO-like country token required for progress.
pub const US_COUNTRY_CODE: &str = "US";

/// Counsel-owned allowset. `None` is deny-all (absent `region_allowset`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowset {
    pub states: Option<BTreeSet<String>>,
    pub version: i64,
}

/// Country + state as resolved for a user (never the box IP for converse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub country: String,
    pub state: String,
}

/// Client-IP extraction result. Money paths deny anything but `Client`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoIp {
    Client(IpAddr),
    Missing,
    UntrustedProxyChain,
}

/// Allowset evaluation (independent of IP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeoDecision {
    Clear {
        checked_at: OffsetDateTime,
        expires_at: OffsetDateTime,
        policy_version: String,
    },
    Hit,
    Indeterminate,
}

impl Allowset {
    /// Parse catalog JSON. Absent / non-array / empty ⇒ deny-all (`states = None`).
    #[must_use]
    pub fn from_config(value: Option<&serde_json::Value>, version: i64) -> Self {
        let Some(arr) = value.and_then(serde_json::Value::as_array) else {
            return Self {
                states: None,
                version,
            };
        };
        if arr.is_empty() {
            return Self {
                states: None,
                version,
            };
        }
        let mut states = BTreeSet::new();
        for entry in arr {
            let Some(code) = entry.as_str() else {
                return Self {
                    states: None,
                    version,
                };
            };
            let trimmed = code.trim();
            if trimmed.is_empty() {
                return Self {
                    states: None,
                    version,
                };
            }
            states.insert(trimmed.to_ascii_uppercase());
        }
        Self {
            states: Some(states),
            version,
        }
    }

    #[must_use]
    pub fn is_deny_all(&self) -> bool {
        self.states.is_none()
    }
}

/// `TRUSTED_PROXY_CIDRS` rule verbatim: honor XFF only when the direct peer
/// is inside the trusted set; take the last hop not in the trusted set.
/// Unlike vote metadata, a chain that never yields an untrusted hop is
/// `UntrustedProxyChain` (deny), not a fallback to the proxy IP.
pub fn select_client_ip(
    peer: Option<IpAddr>,
    xff_hops_left_to_right: &[IpAddr],
    is_trusted: &dyn Fn(&IpAddr) -> bool,
) -> GeoIp {
    let Some(direct) = peer else {
        return GeoIp::Missing;
    };
    if !is_trusted(&direct) {
        return GeoIp::Client(direct);
    }
    xff_hops_left_to_right
        .iter()
        .rev()
        .copied()
        .find(|candidate| !is_trusted(candidate))
        .map_or(GeoIp::UntrustedProxyChain, GeoIp::Client)
}

/// Parse a raw `X-Forwarded-For` value into hops (left-to-right). Invalid
/// tokens are skipped, matching the vote-metadata parser.
#[must_use]
pub fn parse_xff(raw: &str) -> Vec<IpAddr> {
    raw.split(',')
        .filter_map(|part| part.trim().parse::<IpAddr>().ok())
        .collect()
}

/// Converse / KYC-derived region: never the box IP.
#[must_use]
pub fn evaluate_declared_region(
    region: Option<&Region>,
    allowset: &Allowset,
    now: OffsetDateTime,
    ttl: time::Duration,
) -> ScreenVerdict {
    evaluate_region(region, allowset, now, ttl)
}

fn evaluate_region(
    region: Option<&Region>,
    allowset: &Allowset,
    now: OffsetDateTime,
    ttl: time::Duration,
) -> ScreenVerdict {
    let Some(states) = allowset.states.as_ref() else {
        return ScreenVerdict::Indeterminate;
    };
    let Some(region) = region else {
        return ScreenVerdict::Indeterminate;
    };
    if !region.country.eq_ignore_ascii_case(US_COUNTRY_CODE) {
        return ScreenVerdict::Hit;
    }
    let state = region.state.trim().to_ascii_uppercase();
    if state.is_empty() {
        return ScreenVerdict::Indeterminate;
    }
    if !states.contains(&state) {
        return ScreenVerdict::Hit;
    }
    ScreenVerdict::Clear {
        checked_at: now,
        expires_at: now + ttl,
        policy_version: allowset.version.to_string(),
    }
}

/// Combine extracted IP + resolved region. Missing IP, untrusted chain,
/// resolver miss, or deny-all policy ⇒ Indeterminate (deny).
#[must_use]
pub fn evaluate_ip_geo(
    ip: GeoIp,
    resolved: Result<Option<Region>, ()>,
    allowset: &Allowset,
    now: OffsetDateTime,
    ttl: time::Duration,
) -> ScreenVerdict {
    match ip {
        GeoIp::Missing | GeoIp::UntrustedProxyChain => return ScreenVerdict::Indeterminate,
        GeoIp::Client(_) => {}
    }
    match resolved {
        // Resolver error and resolver miss are the same fact: no region.
        Err(()) | Ok(None) => ScreenVerdict::Indeterminate,
        Ok(Some(region)) => evaluate_region(Some(&region), allowset, now, ttl),
    }
}

impl From<GeoDecision> for ScreenVerdict {
    fn from(value: GeoDecision) -> Self {
        match value {
            GeoDecision::Clear {
                checked_at,
                expires_at,
                policy_version,
            } => Self::Clear {
                checked_at,
                expires_at,
                policy_version,
            },
            GeoDecision::Hit => Self::Hit,
            GeoDecision::Indeterminate => Self::Indeterminate,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(40_000)
    }

    fn trusted_10(ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => v4.octets()[0] == 10,
            IpAddr::V6(_) => false,
        }
    }

    #[test]
    fn absent_or_empty_allowset_is_deny_all() {
        for absent in [
            Allowset::from_config(None, 1),
            Allowset::from_config(Some(&json!([])), 1),
            Allowset::from_config(Some(&json!([1])), 1),
            Allowset::from_config(Some(&json!([""])), 1),
        ] {
            assert!(absent.is_deny_all());
        }
        let allowed = Allowset::from_config(Some(&json!(["ca", "NY"])), 2);
        assert_eq!(
            allowed.states.unwrap(),
            BTreeSet::from(["CA".into(), "NY".into()])
        );
        assert_eq!(allowed.version, 2);
    }

    #[test]
    fn trusted_proxy_rule_is_verbatim_and_stricter_on_all_trusted_hops() {
        let is_trusted = &trusted_10 as &dyn Fn(&IpAddr) -> bool;
        let peer: IpAddr = "203.0.113.9".parse().unwrap();
        assert_eq!(
            select_client_ip(Some(peer), &["198.51.100.7".parse().unwrap()], is_trusted),
            GeoIp::Client(peer)
        );
        assert_eq!(select_client_ip(None, &[], is_trusted), GeoIp::Missing);

        let proxy: IpAddr = "10.9.8.7".parse().unwrap();
        let hops = parse_xff("198.51.100.8, 192.0.2.4, 10.1.2.3");
        assert_eq!(
            select_client_ip(Some(proxy), &hops, is_trusted),
            GeoIp::Client("192.0.2.4".parse().unwrap())
        );
        let garbage = parse_xff("garbage, 10.1.2.3");
        assert_eq!(
            select_client_ip(Some(proxy), &garbage, is_trusted),
            GeoIp::UntrustedProxyChain
        );
        // An IPv6 hop is never inside the IPv4-only trusted set.
        let v6 = parse_xff("2001:db8::1, 10.1.2.3");
        assert_eq!(
            select_client_ip(Some(proxy), &v6, is_trusted),
            GeoIp::Client("2001:db8::1".parse().unwrap())
        );
        assert_eq!(
            select_client_ip(Some(proxy), &[], is_trusted),
            GeoIp::UntrustedProxyChain
        );
        assert!(parse_xff("").is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn declared_region_and_ip_geo_cover_every_deny() {
        let now = t0();
        let ttl = Duration::hours(1);
        let allow = Allowset::from_config(Some(&json!(["CA"])), 7);
        let ca = Region {
            country: "US".into(),
            state: "ca".into(),
        };
        assert_eq!(
            evaluate_declared_region(Some(&ca), &allow, now, ttl),
            ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + ttl,
                policy_version: "7".into(),
            }
        );
        assert_eq!(
            evaluate_declared_region(None, &allow, now, ttl),
            ScreenVerdict::Indeterminate
        );
        assert_eq!(
            evaluate_declared_region(
                Some(&Region {
                    country: "CA".into(),
                    state: "ON".into(),
                }),
                &allow,
                now,
                ttl
            ),
            ScreenVerdict::Hit
        );
        assert_eq!(
            evaluate_declared_region(
                Some(&Region {
                    country: "US".into(),
                    state: "TX".into(),
                }),
                &allow,
                now,
                ttl
            ),
            ScreenVerdict::Hit
        );
        assert_eq!(
            evaluate_declared_region(
                Some(&Region {
                    country: "US".into(),
                    state: "  ".into(),
                }),
                &allow,
                now,
                ttl
            ),
            ScreenVerdict::Indeterminate
        );
        let deny = Allowset::from_config(None, 1);
        assert_eq!(
            evaluate_declared_region(Some(&ca), &deny, now, ttl),
            ScreenVerdict::Indeterminate
        );

        let ip = GeoIp::Client("203.0.113.9".parse().unwrap());
        assert_eq!(
            evaluate_ip_geo(GeoIp::Missing, Ok(Some(ca.clone())), &allow, now, ttl),
            ScreenVerdict::Indeterminate
        );
        assert_eq!(
            evaluate_ip_geo(
                GeoIp::UntrustedProxyChain,
                Ok(Some(ca.clone())),
                &allow,
                now,
                ttl
            ),
            ScreenVerdict::Indeterminate
        );
        assert_eq!(
            evaluate_ip_geo(ip, Err(()), &allow, now, ttl),
            ScreenVerdict::Indeterminate
        );
        assert_eq!(
            evaluate_ip_geo(ip, Ok(None), &allow, now, ttl),
            ScreenVerdict::Indeterminate
        );
        assert!(matches!(
            evaluate_ip_geo(ip, Ok(Some(ca)), &allow, now, ttl),
            ScreenVerdict::Clear { .. }
        ));

        let decision = GeoDecision::Hit;
        assert_eq!(ScreenVerdict::from(decision), ScreenVerdict::Hit);
        assert_eq!(
            ScreenVerdict::from(GeoDecision::Indeterminate),
            ScreenVerdict::Indeterminate
        );
        assert!(matches!(
            ScreenVerdict::from(GeoDecision::Clear {
                checked_at: now,
                expires_at: now + ttl,
                policy_version: "1".into(),
            }),
            ScreenVerdict::Clear { .. }
        ));
    }
}
