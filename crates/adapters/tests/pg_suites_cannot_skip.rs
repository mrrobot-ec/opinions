//! Gate: a `PostgreSQL` suite must not be able to skip itself.
//!
//! Every Pg suite in this directory once resolved `DATABASE_URL` on its own and
//! `return`ed early whenever anything about the setup failed — an unset
//! variable, a refused connection, a failed `create database`, a failed
//! migration. Each of those turned the suite into a no-op that still printed
//! `ok`. Measured before the fix: the nine Pg suites reported **75 passing
//! tests in 0.01s** with `DATABASE_URL` unset, without executing one statement.
//!
//! The structural fix is that `tests/common::database_url()` is the ONLY reader
//! of the variable and it panics rather than returning an `Option`, so no
//! caller can be written to skip. This gate keeps it that way: it re-reads
//! every Pg suite's source at compile time and fails if the escape hatch grows
//! back — a direct `std::env::var("DATABASE_URL")`, or a setup helper that
//! yields `None`/returns early instead of failing.
//!
//! Adding a new Pg suite means adding it to `SUITES`. That is deliberate: an
//! unlisted suite is an ungated suite.

/// Every Pg-touching suite source, read at compile time.
const SUITES: &[(&str, &str)] = &[
    ("alert_contract.rs", include_str!("alert_contract.rs")),
    (
        "compliance_contract.rs",
        include_str!("compliance_contract.rs"),
    ),
    ("credit_coverage.rs", include_str!("credit_coverage.rs")),
    ("migration_0006.rs", include_str!("migration_0006.rs")),
    ("migration_0007.rs", include_str!("migration_0007.rs")),
    (
        "migration_0011_deposit_identity.rs",
        include_str!("migration_0011_deposit_identity.rs"),
    ),
    ("pg_contract.rs", include_str!("pg_contract.rs")),
    ("phase2_contract.rs", include_str!("phase2_contract.rs")),
    ("w2_ops_contract.rs", include_str!("w2_ops_contract.rs")),
    ("withdraw_contract.rs", include_str!("withdraw_contract.rs")),
    (
        "resolve_lock_order.rs",
        include_str!("resolve_lock_order.rs"),
    ),
    (
        "migration_all_invariants.rs",
        include_str!("migration_all_invariants.rs"),
    ),
    ("kyc_inbox_race.rs", include_str!("kyc_inbox_race.rs")),
    (
        "deposit_aml_contract.rs",
        include_str!("deposit_aml_contract.rs"),
    ),
    (
        "invariant_currency_detector.rs",
        include_str!("invariant_currency_detector.rs"),
    ),
];

const COMMON: &str = include_str!("common/mod.rs");

#[test]
fn only_the_shared_helper_reads_database_url() {
    // The shared helper is the single reader, and it has no non-panicking arm.
    assert!(
        COMMON.contains("std::env::var(\"DATABASE_URL\")"),
        "tests/common must be the one place DATABASE_URL is read"
    );
    assert!(
        COMMON.contains("panic!("),
        "tests/common::database_url must fail loud, never return an Option"
    );
    for (name, source) in SUITES {
        assert!(
            !source.contains("std::env::var(\"DATABASE_URL\")"),
            "{name} reads DATABASE_URL directly; route it through common::database_url() \
             so an absent database cannot be turned into a silent skip"
        );
        assert!(
            source.contains("mod common;"),
            "{name} is a Pg suite but does not use the fail-loud shared helper"
        );
    }
}

#[test]
fn no_pg_suite_can_return_early_out_of_its_setup() {
    // The exact shapes that used to swallow a missing database. Each is the
    // tail of a setup helper or of the guard a test wrote around one.
    const FORBIDDEN: &[(&str, &str)] = &[
        (
            "return None",
            "a setup helper that yields None lets callers skip",
        ),
        (
            "return Ok(None)",
            "a fallible setup helper that yields None lets callers skip",
        ),
        (
            "else { return }",
            "a one-line skip guard around a setup helper",
        ),
        (
            "else {\n        return;",
            "a block skip guard around a setup helper",
        ),
        (
            ".ok()?",
            "an Option-swallowing setup step hides connect/create/migrate failure",
        ),
    ];
    for (name, source) in SUITES {
        for (pattern, why) in FORBIDDEN {
            assert!(
                !source.contains(pattern),
                "{name} contains {pattern:?}: {why}. A Pg suite that cannot reach \
                 Postgres must FAIL, never silently pass."
            );
        }
    }
}

/// The gate is only worth having if it would actually fire, so prove the
/// matcher on synthetic sources rather than trusting it.
#[test]
fn the_gate_rejects_a_suite_that_reintroduces_the_skip() {
    let regressed =
        "mod common;\nlet Ok(url) = std::env::var(\"DATABASE_URL\") else { return None };";
    assert!(regressed.contains("std::env::var(\"DATABASE_URL\")"));
    assert!(regressed.contains("return None"));
    let guarded = "let Some(pool) = pg_pool().await else { return };";
    assert!(guarded.contains("else { return }"));
    let swallowed = "let pool = connect(&url).await.ok()?;";
    assert!(swallowed.contains(".ok()?"));
    // ...and that a compliant suite trips none of them.
    let compliant = "mod common;\nlet url = common::database_url();\nlet pool = connect(&url).await.expect(\"connect\");";
    for pattern in [
        "std::env::var(\"DATABASE_URL\")",
        "return None",
        "return Ok(None)",
        "else { return }",
        ".ok()?",
    ] {
        assert!(!compliant.contains(pattern), "{pattern} false-positives");
    }
}
