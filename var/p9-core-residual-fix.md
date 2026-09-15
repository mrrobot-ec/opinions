# P9 core residual fixes

## Scope and validity correction

Audited the owned application money, resolver/fakes, AML, and simswarm runner paths against the required specs/reviews (including D4 holders-subset-voters). The original resolver regression used a direct holder-only transfer and was rejected as invalid evidence; it was replaced with `resolution_retries_when_a_referral_bind_commits_during_prelock_wait`, using two real voter/trader participants and a reachable late referral bind while resolution waits on the lower UUID user lock.

## Red/green evidence

The websocket timestamp regression first failed with `left: []`, `right: [20]`; the fixed parser accepts both the actual human-readable server timestamp and RFC3339. AML evidence first failed because the saturation field was `None` instead of `Some(Bool(true))`; explicit per-direction saturation flags now persist in evidence (saturation is only reachable after roughly $9.22T accumulated micro-USDC, so this is forensic hardening).

Missing `bonus_mint_daily_cap_micro` first returned `Ok(GrantReceipt)`; missing referral policy keys first returned successful grants and mutated lots; missing shadow caps first returned a populated gate snapshot. Fake receivables first selected by UUID (`left 3_000_000`, `right 0`); insertion-sequence metadata now matches Pg oldest-first behavior. Each fixed path has a regression proving no mutation on missing required config, while missing `feature_referrals` remains disabled and valid zero/seed values retain their semantics.

For the resolver, the corrected reachable test passes with the fix. I temporarily disabled only the post-lock participant-drift retry branch and reran the exact test; it failed at `resolve_market.rs:1115` with `participant drift must restart and wait for the late referrer lock`. Restoring the branch made the exact test pass again, proving the reachable race and the required retry behavior. The bounded four-attempt loop fails closed on persistent churn; the duplicate consecutive `CuratorDecision::Void` arm was already absent.

## Verification

Focused regressions and positive controls pass. Domain tests (93), application tests (590), and simswarm tests (65 library + 3 main) pass; targeted clippy for domain/application/simswarm with `-D warnings` passes; `cargo fmt --all -- --check` passes. An initial application clippy too-many-lines warning in the new fake test was resolved by extracting helpers without lint suppression.

## Files changed

`crates/application/src/resolve_market.rs`, `crates/application/src/fakes/ops.rs`, `crates/application/src/fakes/state.rs`, `crates/application/src/fakes/credit.rs`, `crates/application/src/money/credits.rs`, `crates/application/src/money/referrals.rs`, `crates/application/src/money/enforcement.rs`, `crates/application/src/money/aml.rs`, and `crates/simswarm/src/engine/runner.rs`.

Remaining risk is limited to bounded retry exhaustion under continuous participant churn and the intentionally extreme AML saturation boundary; no production thresholds or wire formats were changed.
