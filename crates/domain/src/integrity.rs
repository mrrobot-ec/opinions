//! Deterministic integer anomaly checks for the pre-payout sweep.

const PPM: u128 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VoteStats {
    pub total: u32,
    pub last_window: u32,
    pub prior_windows_total: u32,
    pub prior_horizon_windows: u32,
    pub young_accounts: u32,
    pub subnet_observed: u32,
    pub top_subnet_count: u32,
    pub device_observed: u32,
    pub top_device_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SweepThresholds {
    pub burst_multiplier_ppm: u32,
    pub young_account_share_max_ppm: u32,
    pub subnet_share_max_ppm: u32,
    pub device_share_max_ppm: u32,
    pub min_votes_for_ratios: u32,
    pub min_metadata_coverage_ppm: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Strength {
    Weak,
    Medium,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckResult {
    pub name: &'static str,
    pub value_ppm: u64,
    pub threshold_ppm: u64,
    pub coverage_ppm: u32,
    pub strength: Strength,
    pub flagged: bool,
    pub note: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Pass,
    Flag,
}

fn ratio_ppm(numerator: u32, denominator: u32) -> u64 {
    if denominator == 0 {
        return 0;
    }
    let value = u128::from(numerator) * PPM / u128::from(denominator);
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn coverage_ppm(observed: u32, total: u32) -> u32 {
    u32::try_from(ratio_ppm(observed, total)).unwrap_or(u32::MAX)
}

fn low_votes(name: &'static str, threshold: u32, strength: Strength) -> CheckResult {
    CheckResult {
        name,
        value_ppm: 0,
        threshold_ppm: u64::from(threshold),
        coverage_ppm: 1_000_000,
        strength,
        flagged: false,
        note: "skipped: fewer than the configured minimum votes",
    }
}

fn vote_burst(s: &VoteStats, t: &SweepThresholds) -> CheckResult {
    if s.total < t.min_votes_for_ratios {
        return low_votes("vote_burst", t.burst_multiplier_ppm, Strength::Medium);
    }
    if s.prior_windows_total == 0 {
        return CheckResult {
            name: "vote_burst",
            value_ppm: u64::from(s.last_window) * 1_000_000,
            threshold_ppm: u64::from(t.min_votes_for_ratios) * 1_000_000,
            coverage_ppm: 1_000_000,
            strength: Strength::Medium,
            flagged: s.last_window > t.min_votes_for_ratios,
            note: "zero prior baseline; compares final-window count to minimum votes",
        };
    }
    let ratio = u128::from(s.last_window)
        .saturating_mul(u128::from(s.prior_horizon_windows))
        .saturating_mul(PPM)
        / u128::from(s.prior_windows_total);
    let value_ppm = u64::try_from(ratio).unwrap_or(u64::MAX);
    CheckResult {
        name: "vote_burst",
        value_ppm,
        threshold_ppm: u64::from(t.burst_multiplier_ppm),
        coverage_ppm: 1_000_000,
        strength: Strength::Medium,
        flagged: value_ppm > u64::from(t.burst_multiplier_ppm),
        note: "final close-anchored window versus prior-window average",
    }
}

#[derive(Clone, Copy)]
struct ShareCheck {
    name: &'static str,
    count: u32,
    denominator: u32,
    threshold: u32,
    min_coverage: u32,
    strength: Strength,
    note: &'static str,
}

fn share_check(input: ShareCheck, total: u32, min_votes: u32) -> CheckResult {
    if total < min_votes {
        return low_votes(input.name, input.threshold, input.strength);
    }
    let coverage = coverage_ppm(input.denominator, total);
    if coverage < input.min_coverage {
        return CheckResult {
            name: input.name,
            value_ppm: 0,
            threshold_ppm: u64::from(input.threshold),
            coverage_ppm: coverage,
            strength: input.strength,
            flagged: false,
            note: "skipped: metadata coverage below configured minimum",
        };
    }
    let value = ratio_ppm(input.count, input.denominator);
    CheckResult {
        name: input.name,
        value_ppm: value,
        threshold_ppm: u64::from(input.threshold),
        coverage_ppm: coverage,
        strength: input.strength,
        flagged: value > u64::from(input.threshold),
        note: input.note,
    }
}

#[must_use]
pub fn sweep_checks(s: &VoteStats, t: &SweepThresholds) -> Vec<CheckResult> {
    vec![
        vote_burst(s, t),
        share_check(
            ShareCheck {
                name: "young_account_share",
                count: s.young_accounts,
                denominator: s.total,
                threshold: t.young_account_share_max_ppm,
                min_coverage: 0,
                strength: Strength::Medium,
                note: "account age at cast time",
            },
            s.total,
            t.min_votes_for_ratios,
        ),
        share_check(
            ShareCheck {
                name: "subnet_concentration",
                count: s.top_subnet_count,
                denominator: s.subnet_observed,
                threshold: t.subnet_share_max_ppm,
                min_coverage: t.min_metadata_coverage_ppm,
                strength: Strength::Weak,
                note: "weak IP-prefix proxy: IPv4 /24, IPv6 /64",
            },
            s.total,
            t.min_votes_for_ratios,
        ),
        share_check(
            ShareCheck {
                name: "device_concentration",
                count: s.top_device_count,
                denominator: s.device_observed,
                threshold: t.device_share_max_ppm,
                min_coverage: t.min_metadata_coverage_ppm,
                strength: Strength::Weak,
                note: "weak client-asserted device identifier",
            },
            s.total,
            t.min_votes_for_ratios,
        ),
    ]
}

#[must_use]
pub fn verdict(results: &[CheckResult]) -> Verdict {
    if results.iter().filter(|result| result.flagged).count() >= 2 {
        Verdict::Flag
    } else {
        Verdict::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> SweepThresholds {
        SweepThresholds {
            burst_multiplier_ppm: 2_000_000,
            young_account_share_max_ppm: 500_000,
            subnet_share_max_ppm: 600_000,
            device_share_max_ppm: 600_000,
            min_votes_for_ratios: 4,
            min_metadata_coverage_ppm: 500_000,
        }
    }

    fn clean() -> VoteStats {
        VoteStats {
            total: 10,
            last_window: 2,
            prior_windows_total: 8,
            prior_horizon_windows: 4,
            young_accounts: 5,
            subnet_observed: 10,
            top_subnet_count: 6,
            device_observed: 10,
            top_device_count: 6,
        }
    }

    #[test]
    fn boundaries_pass_and_strict_excess_flags() {
        let checks = sweep_checks(&clean(), &thresholds());
        assert!(checks.iter().all(|check| !check.flagged));
        let mut breached = clean();
        breached.last_window = 5;
        breached.young_accounts = 6;
        breached.top_subnet_count = 7;
        breached.top_device_count = 7;
        let checks = sweep_checks(&breached, &thresholds());
        assert!(checks.iter().all(|check| check.flagged));
        assert_eq!(verdict(&checks), Verdict::Flag);
    }

    #[test]
    fn one_signal_is_pass_with_note() {
        let mut stats = clean();
        stats.young_accounts = 6;
        let checks = sweep_checks(&stats, &thresholds());
        assert_eq!(checks.iter().filter(|check| check.flagged).count(), 1);
        assert_eq!(verdict(&checks), Verdict::Pass);
    }

    #[test]
    fn zero_baseline_rule_is_strict() {
        let mut stats = clean();
        stats.prior_windows_total = 0;
        stats.last_window = 4;
        assert!(!sweep_checks(&stats, &thresholds())[0].flagged);
        stats.last_window = 5;
        assert!(sweep_checks(&stats, &thresholds())[0].flagged);
    }

    #[test]
    fn sparse_metadata_and_low_votes_skip_checks() {
        let mut stats = clean();
        stats.subnet_observed = 4;
        stats.device_observed = 0;
        let checks = sweep_checks(&stats, &thresholds());
        assert!(!checks[2].flagged);
        assert_eq!(checks[2].coverage_ppm, 400_000);
        assert!(!checks[3].flagged);
        assert_eq!(checks[3].coverage_ppm, 0);

        stats.total = 3;
        let checks = sweep_checks(&stats, &thresholds());
        assert!(checks.iter().all(|check| !check.flagged));
        assert!(checks.iter().all(|check| check.value_ppm == 0));
    }

    #[test]
    fn empty_results_pass_and_ratio_zero_is_safe() {
        assert_eq!(verdict(&[]), Verdict::Pass);
        assert_eq!(ratio_ppm(1, 0), 0);
    }
}
