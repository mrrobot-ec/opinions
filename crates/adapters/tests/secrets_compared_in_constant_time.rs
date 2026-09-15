//! Gate: every bearer secret in this adapter is compared in constant time.
//!
//! `presented == expected` on strings short-circuits at the first differing
//! byte and checks the length first, so the response time is a function of how
//! much of the secret the caller guessed correctly. `http::middleware` already
//! refuses to give that oracle for an admin token — it hashes and compares 32
//! fixed bytes with a single accumulator. The demo token deserves the same: it
//! gates trades, votes, withdrawal requests, self-exclusion, self-set deposit
//! limits, phone verification, and the WebSocket frame that hands out another
//! user's private notification stream and unread count.
//!
//! A timing assertion would be flaky and would prove nothing on a loaded CI
//! box, so this gate is STRUCTURAL: it reads the four secret-bearing sources
//! and fails if a direct comparison reappears. That is a property of the source
//! that is checkable exactly, rather than a statistic that is checkable badly.

const MIDDLEWARE: &str = include_str!("../src/http/middleware.rs");

/// The four modules that compare a caller-supplied secret.
const SECRET_BEARING: &[(&str, &str)] = &[
    (
        "http/routes/mod.rs",
        include_str!("../src/http/routes/mod.rs"),
    ),
    (
        "http/routes/phone.rs",
        include_str!("../src/http/routes/phone.rs"),
    ),
    (
        "http/routes/compliance_admin.rs",
        include_str!("../src/http/routes/compliance_admin.rs"),
    ),
    ("http/ws.rs", include_str!("../src/http/ws.rs")),
];

/// Direct comparisons of a presented secret against the configured one. Each
/// is the exact shape that was there before this gate existed.
const DIRECT_COMPARISONS: &[&str] = &[
    "got == expected",
    "got != expected",
    "expected == got",
    "token == &state.demo_token",
    "token != &state.demo_token",
    "token == state.demo_token",
    "token != state.demo_token",
    "presented == expected",
    "presented != expected",
];

#[test]
fn no_secret_is_compared_with_a_short_circuiting_operator() {
    for (name, source) in SECRET_BEARING {
        for pattern in DIRECT_COMPARISONS {
            assert!(
                !source.contains(pattern),
                "{name} contains `{pattern}`: a bearer secret must go through \
                 http::middleware::secret_eq, which hashes both sides and compares 32 fixed \
                 bytes with no early exit. A short-circuiting compare leaks how much of the \
                 secret the caller guessed."
            );
        }
    }
}

#[test]
fn every_secret_bearing_module_uses_the_shared_helper() {
    assert!(
        MIDDLEWARE.contains("pub fn secret_eq("),
        "the shared constant-time comparison must exist in http::middleware"
    );
    assert!(
        MIDDLEWARE.contains("fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool"),
        "secret_eq must be built on the fixed-width accumulator compare"
    );
    for (name, source) in SECRET_BEARING {
        assert!(
            source.contains("middleware::secret_eq"),
            "{name} compares a secret but does not use the shared constant-time helper"
        );
    }
}

/// The gate is only worth having if it fires, so prove the matcher rather than
/// trusting it: a source that reintroduces any shape must trip, and a
/// compliant one must not.
#[test]
fn the_gate_rejects_a_reintroduced_direct_comparison() {
    let regressed = "    if got == expected {\n        Ok(())\n    }";
    assert!(DIRECT_COMPARISONS
        .iter()
        .any(|pattern| regressed.contains(pattern)));
    let ws_regressed = "if token != &state.demo_token { continue; }";
    assert!(DIRECT_COMPARISONS
        .iter()
        .any(|pattern| ws_regressed.contains(pattern)));
    let compliant = "if crate::http::middleware::secret_eq(got, expected) {\n        Ok(())\n    }";
    assert!(
        !DIRECT_COMPARISONS
            .iter()
            .any(|pattern| compliant.contains(pattern)),
        "the compliant shape must not false-positive"
    );
    assert!(compliant.contains("middleware::secret_eq"));
}
