# Opinions — Phase 0 (Foundations) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the repo with CI quality gates and build the pure money core (newtypes, double-entry ledger, CPMM AMM, market state machine, vote scoring, resolution math) plus schema v1 and the conversation-service skeleton — the foundation every later phase depends on.

**Architecture:** Clean/hexagonal Rust workspace where `crates/domain` is pure (zero I/O, zero framework deps) and everything else depends inward; Postgres schema v1 from the ER model; a separate Python FastAPI + LangGraph service skeleton with a fake-LLM seam and the agent-run recorder. Spec: `docs/spec.md` §2, §3.2, §4.2, §5.5, §5.6, §7.5, §10.3.

**Tech Stack:** Rust stable (edition 2021), proptest, thiserror, uuid; Postgres 16 (sqlx-cli migrations), Redis 7, NATS 2 via docker compose; Python 3.12, uv, FastAPI, LangGraph, Pydantic v2, pytest.

## Global Constraints

Copied from spec §2.5, §5.5, §7.5, D12–D18 — every task implicitly includes these:

- **All money is integer micro-USDC (`i64`, checked `u128` intermediates under enforced input bounds); all shares are integer micro-shares. No floats anywhere in ledger or AMM code paths.**
- **Rounding always favors the pool/house, never the user:** each rounding operation (fee ceil, retained-reserve ceil, payout floor) errs <1 micro against the user; a trade touches ≤2 such operations (≤2 micro per fill). Direction uniform; both bounds property-tested. (R1: do not claim a 1-micro aggregate bound.)
- Dependency rule: `domain` has **zero** deps on tokio/sqlx/axum/serde-with-I/O; `domain` may use only `thiserror`, `uuid` (v4 gated behind a feature not used by domain logic), `proptest` (dev).
- `#![forbid(unsafe_code)]` in every crate; `clippy::unwrap_used`/`expect_used` denied on non-test code; CI runs rustfmt check + clippy `-D warnings`.
- Coverage (R1-corrected mechanics): `cargo llvm-cov --fail-under-lines 100` on `crates/domain`, enforced in CI on **stable** from this phase onward; **branch coverage runs on a scheduled nightly-toolchain job** (reviewed, non-blocking until stable supports branch gates); `cargo mutants` ≥90% kill on `crates/domain` computed as killed/(killed+missed) by `scripts/mutation_gate.py` (nightly job, non-blocking in Phase 0, blocking from Phase 1).
- Python: LLM calls only through an injectable runnable (fake in tests); every structured-output schema includes `rationale: str`; **no PydanticAI**; numbers in outbound messages are injected, never generated.
- Every graph run writes `agent_runs` + `agent_steps` rows (spec §10.3).
- Commits: conventional-commit style, one task = at least one commit, tests committed with implementation.

**Worker protocol:** tasks 1–6 are sequential (each consumes the previous crate surface). Task 7 depends on Task 0 only; Task 8 depends on Task 7. Do not start a task with failing tests on main.

---

### Task 0: Workspace scaffold, toolchain, CI gates, infra compose

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`, `justfile`, `docker-compose.yml`, `.github/workflows/ci.yml`, `crates/domain/Cargo.toml`, `crates/domain/src/lib.rs`, `deny.toml`, `scripts/mutation_gate.py`, `scripts/fixtures/pass.json`, `scripts/fixtures/fail.json`, `scripts/fixtures/timeout.json`, `scripts/fixtures/empty.json`

**Interfaces:**
- Produces: a workspace where `cargo test -p domain` runs, plus `just ci` mirroring the CI pipeline locally.

- [ ] **Step 1: Write workspace + crate manifests**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/domain"]

[workspace.package]
edition = "2021"
license = "UNLICENSED"

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
todo = "deny"
dbg_macro = "deny"
float_arithmetic = "deny"   # the no-floats rule, mechanically enforced
```

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy", "llvm-tools-preview"]
```

`crates/domain/Cargo.toml`:
```toml
[package]
name = "domain"
version = "0.1.0"
edition.workspace = true

[dependencies]
thiserror = "2"
uuid = { version = "1", default-features = false }

[dev-dependencies]
proptest = "1"
uuid = { version = "1", features = ["v4"] }

[lints]
workspace = true
```

`crates/domain/src/lib.rs`:
```rust
//! Pure money core: no I/O, no framework types, deterministic by construction.
pub mod money;
pub mod ledger;
pub mod amm;
pub mod market;
pub mod scoring;
pub mod resolution;
```
(Module files are created by Tasks 1–6; for this task create each as an empty file so the crate compiles.)

- [ ] **Step 2: Write infra + task runner**

`docker-compose.yml`:
```yaml
services:
  postgres:
    image: postgres:16-alpine
    environment: { POSTGRES_USER: opinions, POSTGRES_PASSWORD: opinions, POSTGRES_DB: opinions }
    ports: ["5432:5432"]
  redis:
    image: redis:7-alpine
    ports: ["6379:6379"]
  nats:
    image: nats:2-alpine
    command: ["-js"]
    ports: ["4222:4222"]
```

`justfile` (R1: recipes need indented bodies — `name: ; cmd` is not valid just syntax):
```just
ci: fmt clippy test coverage deny audit gate-test

gate-test:
    python3 scripts/mutation_gate.py scripts/fixtures/pass.json 90
    ! python3 scripts/mutation_gate.py scripts/fixtures/fail.json 90
    ! python3 scripts/mutation_gate.py scripts/fixtures/timeout.json 90
    ! python3 scripts/mutation_gate.py scripts/fixtures/empty.json 90

fmt:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

coverage:
    cargo llvm-cov -p domain --fail-under-lines 100

branch-coverage:
    # nightly-only; scheduled CI job, reviewed not gating (R1/B2)
    cargo +nightly llvm-cov -p domain --branch

mutants:
    # R2 (codex M5): do NOT mask failures. cargo-mutants exit codes: 0 = all caught,
    # 2 = some mutants missed (expected, gate decides), anything else = invalid run.
    cargo mutants -p domain -o mutants.out; ec=$?; \
    if [ "$ec" -ne 0 ] && [ "$ec" -ne 2 ]; then echo "cargo-mutants failed (exit $ec)"; exit "$ec"; fi; \
    python3 scripts/mutation_gate.py mutants.out/outcomes.json 90

deny:
    cargo deny check

audit:
    cargo audit

infra-up:
    docker compose up -d

migrate:
    sqlx migrate run --source migrations --database-url $DATABASE_URL
```

`scripts/mutation_gate.py` (checked in with Task 0; R2/codex M5: reject invalid runs, don't just count):
```python
#!/usr/bin/env python3
"""Gate: killed/(killed+missed) >= threshold%, from cargo-mutants outcomes.json.

Fails on: unviable/timeout/failure summaries beyond a tolerance, zero mutants,
or an unrecognized schema — a passing rate over an invalid run is worthless.
Unit-tested against fixtures in scripts/fixtures/ (pass, fail, timeout, empty).
"""
import json, sys

data = json.load(open(sys.argv[1]))
threshold = float(sys.argv[2])
rows = data.get("outcomes")
if not isinstance(rows, list) or not rows:
    print("invalid or empty outcomes.json — refusing to gate"); sys.exit(1)
counts = {}
for o in rows:
    counts[o.get("summary", "UNKNOWN")] = counts.get(o.get("summary", "UNKNOWN"), 0) + 1
caught = counts.pop("CaughtMutant", 0)
missed = counts.pop("MissedMutant", 0)
counts.pop("Unviable", None)  # unviable mutants are expected noise
if counts:  # timeouts, baseline failures, unknown summaries → invalid run
    print(f"non-gateable outcomes present: {counts} — fix the run first"); sys.exit(1)
if caught + missed == 0:
    print("no gateable mutants"); sys.exit(1)
rate = 100.0 * caught / (caught + missed)
print(f"mutation kill rate: {rate:.1f}% (caught={caught}, missed={missed})")
sys.exit(0 if rate >= threshold else 1)
```

Tool pinning (R2/codex M5, exact-pins R3/codex M2): the CI `taiki-e/install-action` steps pin **exact patch versions** — the implementer resolves the current exacts at build time (e.g. `cargo-llvm-cov@0.6.x`, `cargo-mutants@25.x.y`, `just@1.x.y` — record the chosen exacts in the commit) so the parsed JSON schema and gate flags cannot drift silently; `rust-toolchain.toml` likewise pins an exact stable version (e.g. `channel = "1.89.0"`), not rolling `stable`.

Gate-script fixtures (R3/codex M2 — named, created, and exercised in Task 0, not promised):
- `scripts/fixtures/pass.json` (12 caught / 1 missed → 92.3%, exit 0 at 90)
- `scripts/fixtures/fail.json` (8 caught / 2 missed → 80%, exit 1 at 90)
- `scripts/fixtures/timeout.json` (contains a `Timeout` summary → exit 1, "non-gateable")
- `scripts/fixtures/empty.json` (`{"outcomes": []}` → exit 1)
- `justfile` recipe `gate-test` runs all four assertions; `just ci` includes `gate-test`; the CI `rust` job therefore runs it on every push/PR.

`.github/workflows/ci.yml` (R1: install every tool CI relies on; audit job added; line gate on stable; branch+mutants on schedule):
```yaml
name: ci
on:
  push: { branches: [main] }
  pull_request: {}
  schedule: [{ cron: "0 6 * * *" }]   # nightly: branch coverage + mutation gate
jobs:
  rust:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: "rustfmt, clippy, llvm-tools-preview" }
      - uses: taiki-e/install-action@v2
        with: { tool: "cargo-llvm-cov@0.6, just@1" }
      - run: just fmt
      - run: just clippy
      - run: just test
      - run: just coverage
  deny:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
  audit:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: rustsec/audit-check@v2
        with: { token: "${{ secrets.GITHUB_TOKEN }}" }
  nightly-gates:
    if: github.event_name == 'schedule'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
        with: { components: "llvm-tools-preview" }
      - uses: dtolnay/rust-toolchain@stable
      - uses: taiki-e/install-action@v2
        with: { tool: "cargo-llvm-cov@0.6, cargo-mutants@25, just@1" }
      - run: just branch-coverage   # nightly toolchain; reviewed, non-gating for now
      - run: just mutants           # kill-rate gate script; non-blocking Phase 0, blocking Phase 1+
        continue-on-error: true
```

`deny.toml`: default template from `cargo deny init` (licenses: allow MIT/Apache-2.0/BSD; bans: warn on duplicates; advisories: deny unsound/yanked).

`.gitignore`: `target/`, `.env`, `__pycache__/`, `.venv/`, `*.pyc`, `node_modules/`.

- [ ] **Step 3: Verify the empty workspace passes the pipeline**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: PASS (0 tests). `cargo llvm-cov` on an empty crate reports 100% vacuously — acceptable until Task 1 adds lines.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: workspace scaffold, CI quality gates, infra compose"
```

---

### Task 1: Money newtypes with pool-favoring fee math

**Files:**
- Create: `crates/domain/src/money.rs`

**Interfaces:**
- Produces:
  - `MicroUsd(pub i64)`, `MicroShares(pub i64)`, `BasisPoints(pub u16)` — `Copy + Eq + Ord + Debug`
  - `MicroUsd::checked_add/checked_sub -> Option<MicroUsd>`
  - `apply_fee(gross: MicroUsd, fee: BasisPoints) -> Result<FeeSplit, MoneyError>` where `FeeSplit { net: MicroUsd, fee: MicroUsd }`, fee = **ceil**(gross·bps/10 000), `net + fee == gross` exactly
  - `MoneyError` (`thiserror`): `NegativeAmount`, `Overflow`, `FeeTooHigh` (bps > 10 000)

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn fee_rounds_up_against_user() {
        // 1% of 101 micro = 1.01 -> fee must be 2 (ceil), net 99
        let s = apply_fee(MicroUsd(101), BasisPoints(100)).unwrap();
        assert_eq!(s.fee, MicroUsd(2));
        assert_eq!(s.net, MicroUsd(99));
    }

    #[test]
    fn zero_fee_passes_through() {
        let s = apply_fee(MicroUsd(500), BasisPoints(0)).unwrap();
        assert_eq!((s.net, s.fee), (MicroUsd(500), MicroUsd(0)));
    }

    #[test]
    fn negative_and_overfee_rejected() {
        assert!(matches!(apply_fee(MicroUsd(-1), BasisPoints(100)), Err(MoneyError::NegativeAmount)));
        assert!(matches!(apply_fee(MicroUsd(1), BasisPoints(10_001)), Err(MoneyError::FeeTooHigh)));
    }

    proptest! {
        #[test]
        fn split_always_reassembles(gross in 0i64..=i64::MAX / 20_000, bps in 0u16..=10_000) {
            let s = apply_fee(MicroUsd(gross), BasisPoints(bps)).unwrap();
            prop_assert_eq!(s.net.0 + s.fee.0, gross);
            prop_assert!(s.fee.0 >= 0 && s.net.0 >= 0);
        }
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p domain money` → FAIL (types not defined).

- [ ] **Step 3: Implement**

```rust
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MicroUsd(pub i64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MicroShares(pub i64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BasisPoints(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSplit { pub net: MicroUsd, pub fee: MicroUsd }

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MoneyError {
    #[error("amount must be non-negative")] NegativeAmount,
    #[error("arithmetic overflow")] Overflow,
    #[error("fee above 100%")] FeeTooHigh,
}

impl MicroUsd {
    #[must_use] pub fn checked_add(self, o: Self) -> Option<Self> { self.0.checked_add(o.0).map(Self) }
    #[must_use] pub fn checked_sub(self, o: Self) -> Option<Self> { self.0.checked_sub(o.0).map(Self) }
}

/// Fee rounds UP (ceil) so rounding always favors the house; `net + fee == gross` exactly.
pub fn apply_fee(gross: MicroUsd, fee: BasisPoints) -> Result<FeeSplit, MoneyError> {
    if gross.0 < 0 { return Err(MoneyError::NegativeAmount); }
    if fee.0 > 10_000 { return Err(MoneyError::FeeTooHigh); }
    let g = i128::from(gross.0);
    let f = (g * i128::from(fee.0) + 9_999) / 10_000; // ceil
    let fee_amt = i64::try_from(f).map_err(|_| MoneyError::Overflow)?;
    Ok(FeeSplit { net: MicroUsd(gross.0 - fee_amt), fee: MicroUsd(fee_amt) })
}
```

- [ ] **Step 4: Run tests** — `cargo test -p domain money` → PASS. Then `just coverage` → 100%.

- [ ] **Step 5: Commit** — `git commit -m "feat(domain): money newtypes with pool-favoring fee math"`

---

### Task 2: Double-entry ledger core

**Files:**
- Create: `crates/domain/src/ledger.rs`

**Interfaces:**
- Consumes: `MicroUsd` from Task 1.
- Produces:
  - `AccountId(pub uuid::Uuid)`; `OwnerType { User, Pool, Fees, House, Escrow, External }`; `Currency { Usdc, UsdcCredit }` (R2/codex B2: currency is a **domain** dimension, not just a DB column)
  - **`External` is the contra class representing the outside world, exactly one External account per currency** (R1/codex B1, R2/codex B2): deposits are `External → User`, withdrawals the reverse, genesis/house funding is an explicit `External → House` transaction. `Balances::open` rejects a second External for the same currency (`DuplicateExternal`).
  - `Entry { account: AccountId, amount: MicroUsd }` (signed; nonzero)
  - `TxnKind { Deposit, Trade, Payout, Withdrawal, Seed, Reversal, CreditGrant, CreditConvert }`
  - `Transaction::new(kind: TxnKind, entries: Vec<Entry>) -> Result<Transaction, LedgerError>` — rejects unless len ≥ 2, all entries nonzero, and **the overall sum is exactly zero (i128)** — a necessary structural condition checkable without account metadata (`Unbalanced`). The stronger per-currency validation lives in `Balances::apply` (only it knows each account's currency): **entries must sum to exactly zero within every currency touched** (`UnbalancedCurrency`) — an overall-zero transaction can still illegally convert `usdc_credit` into `usdc` in two legs. A legitimate cross-currency credit conversion is four legs, each currency internally balanced: `user_credit → external_credit` (burn) + `external_cash → user_cash` (issue).
  - `Balances` (in-memory map for domain logic/tests): `apply(&mut self, txn: &Transaction) -> Result<(), LedgerError>` — **first aggregates entries per account with checked i128 arithmetic** (R2/codex B1: computing repeated-account entries from the same stale balance can mint money — `A:-7, A:-6, B:+13` against `A=10` must fail, not settle at `A=4`), then validates per-currency zero sums, then checks every post-balance: atomic, rejecting `InsufficientFunds { account }` if any **internal** account would go below zero, applying nothing. **`External` accounts may go negative** (per currency, their negative balance equals net system inflow of that currency).
  - `LedgerError`: `Unbalanced { sum_micro: i128 }`, `UnbalancedCurrency { currency: Currency, sum_micro: i128 }`, `TooFewEntries`, `ZeroEntry`, `InsufficientFunds { account: AccountId }`, `UnknownAccount`, `DuplicateExternal { currency: Currency }`, `Overflow`
  - Invariant helper: `Balances::internal_total(&self, currency: Currency) -> MicroUsd` — sum over internal accounts of that currency; property: `internal_total(c) == -balance(external(c))` for every currency, always.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::money::MicroUsd;
    use proptest::prelude::*;
    use uuid::Uuid;

    fn acct() -> AccountId { AccountId(Uuid::new_v4()) }

    #[test]
    fn unbalanced_transaction_rejected() {
        let e = vec![Entry { account: acct(), amount: MicroUsd(-5) },
                     Entry { account: acct(), amount: MicroUsd(4) }];
        assert!(matches!(Transaction::new(TxnKind::Trade, e), Err(LedgerError::Unbalanced { sum_micro: -1 })));
    }

    #[test]
    fn apply_is_atomic_on_insufficient_funds() {
        let (a, b) = (acct(), acct());
        let mut bal = Balances::default();
        bal.open(a, OwnerType::User, Currency::Usdc).unwrap();
        bal.open(b, OwnerType::Fees, Currency::Usdc).unwrap();
        let txn = Transaction::new(TxnKind::Trade, vec![
            Entry { account: a, amount: MicroUsd(-10) },
            Entry { account: b, amount: MicroUsd(10) },
        ]).unwrap();
        assert!(matches!(bal.apply(&txn), Err(LedgerError::InsufficientFunds { .. })));
        assert_eq!(bal.balance(a).unwrap(), MicroUsd(0)); // nothing applied
        assert_eq!(bal.balance(b).unwrap(), MicroUsd(0));
    }

    #[test]
    fn duplicate_account_entries_are_aggregated_before_validation() {
        // R2/codex B1: A:-7, A:-6, B:+13 against A=10 must fail as a whole,
        // not partially settle from stale per-entry balances.
        let (ext, a, b) = (acct(), acct(), acct());
        let mut bal = Balances::default();
        bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
        bal.open(a, OwnerType::User, Currency::Usdc).unwrap();
        bal.open(b, OwnerType::User, Currency::Usdc).unwrap();
        bal.apply(&Transaction::new(TxnKind::Deposit, vec![
            Entry { account: ext, amount: MicroUsd(-10) },
            Entry { account: a,   amount: MicroUsd(10) },
        ]).unwrap()).unwrap();
        let txn = Transaction::new(TxnKind::Trade, vec![
            Entry { account: a, amount: MicroUsd(-7) },
            Entry { account: a, amount: MicroUsd(-6) },
            Entry { account: b, amount: MicroUsd(13) },
        ]).unwrap();
        assert!(matches!(bal.apply(&txn), Err(LedgerError::InsufficientFunds { .. })));
        assert_eq!(bal.balance(a).unwrap(), MicroUsd(10)); // untouched
        assert_eq!(bal.balance(b).unwrap(), MicroUsd(0));
    }

    #[test]
    fn cross_currency_two_leg_transaction_rejected() {
        // Overall sum is zero but converts credit into cash — must be UnbalancedCurrency.
        let (uc, ucash) = (acct(), acct());
        let mut bal = Balances::default();
        bal.open(uc, OwnerType::User, Currency::UsdcCredit).unwrap();
        bal.open(ucash, OwnerType::User, Currency::Usdc).unwrap();
        let txn = Transaction::new(TxnKind::CreditConvert, vec![
            Entry { account: uc,    amount: MicroUsd(-500) },
            Entry { account: ucash, amount: MicroUsd(500) },
        ]).unwrap();
        assert!(matches!(bal.apply(&txn), Err(LedgerError::UnbalancedCurrency { .. })));
    }

    #[test]
    fn deposit_from_external_is_representable() {
        let (ext, user) = (acct(), acct());
        let mut bal = Balances::default();
        bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
        bal.open(user, OwnerType::User, Currency::Usdc).unwrap();
        let txn = Transaction::new(TxnKind::Deposit, vec![
            Entry { account: ext,  amount: MicroUsd(-1_000_000) },
            Entry { account: user, amount: MicroUsd(1_000_000) },
        ]).unwrap();
        bal.apply(&txn).unwrap(); // external may go negative — this must NOT be InsufficientFunds
        assert_eq!(bal.balance(user).unwrap(), MicroUsd(1_000_000));
        assert_eq!(bal.balance(ext).unwrap(), MicroUsd(-1_000_000));
        assert_eq!(bal.internal_total(Currency::Usdc), MicroUsd(1_000_000));
    }

    proptest! {
        /// Conservation: after any accepted transaction sequence, internal_total == -balance(external),
        /// and no internal account is ever negative.
        #[test]
        fn conservation_over_random_transfers(deposit in 1i64..1_000_000_000, moves in proptest::collection::vec((0usize..4, 0usize..4, 1i64..1_000_000), 1..50)) {
            let ext = acct();
            let accts: Vec<AccountId> = (0..4).map(|_| acct()).collect();
            let mut bal = Balances::default();
            bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
            for a in &accts { bal.open(*a, OwnerType::User, Currency::Usdc).unwrap(); }
            let fund = Transaction::new(TxnKind::Deposit, vec![
                Entry { account: ext, amount: MicroUsd(-deposit) },
                Entry { account: accts[0], amount: MicroUsd(deposit) },
            ]).unwrap();
            bal.apply(&fund).unwrap();
            for (from, to, amt) in moves {
                if from == to { continue; }
                if let Ok(txn) = Transaction::new(TxnKind::Trade, vec![
                    Entry { account: accts[from], amount: MicroUsd(-amt) },
                    Entry { account: accts[to],   amount: MicroUsd(amt)  },
                ]) { let _ = bal.apply(&txn); }
            }
            prop_assert_eq!(bal.internal_total(Currency::Usdc).0, deposit);
            prop_assert_eq!(bal.balance(ext).unwrap().0, -deposit);
            for a in &accts { prop_assert!(bal.balance(*a).unwrap().0 >= 0); }
        }
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p domain ledger` → FAIL.

- [ ] **Step 3: Implement** — `Balances` as `HashMap<AccountId, (OwnerType, Currency, i64)>` internally (std only). `apply` pipeline: (1) **aggregate entry amounts per AccountId in i128** (duplicate accounts in one transaction are legal but must be summed before validation — R2/codex B1); (2) validate per-currency zero sums over the aggregated map; (3) compute every post-balance with checked arithmetic, rejecting overflow, unknown account, or a negative result **on any non-External account**; (4) write. There is no genesis backdoor: every funding path, including tests and house capitalization, is an ordinary balanced transaction against the `External` account of that currency. Required tests beyond Step 1: the exact `A:-7, A:-6, B:+13` duplicate-account case (must be `InsufficientFunds`, and totals conserved), a credit-conversion four-leg transaction, per-currency imbalance rejection (`usdc_credit` debit vs `usdc` credit in two legs → `UnbalancedCurrency`), `DuplicateExternal`, and a property test with repeated-account multi-entry transactions asserting conservation + atomicity.

- [ ] **Step 4: Run tests** — `cargo test -p domain ledger` → PASS; `just coverage` → 100% (cover every error arm; add small direct tests for `TooFewEntries`, `ZeroEntry`, `UnknownAccount`, `Overflow` — e.g. an account at `i64::MAX`).

- [ ] **Step 5: Commit** — `git commit -m "feat(domain): double-entry ledger with atomic balanced transactions"`

---

### Task 3: CPMM AMM over complete sets (integer, pool-favoring)

**Files:**
- Create: `crates/domain/src/amm.rs`

**Interfaces:**
- Consumes: `MicroUsd`, `MicroShares`, `BasisPoints`, `apply_fee` (Task 1).
- Produces:
  - `Pool { yes: MicroShares, no: MicroShares, fee: BasisPoints }` with `Pool::new` rejecting non-positive reserves
  - `Side { Yes, No }`
  - `quote_buy(pool, side, collateral: MicroUsd) -> Result<BuyQuote, AmmError>`; `BuyQuote { shares_out: MicroShares, fee: MicroUsd, pool_after: Pool, avg_price_micro_per_share: i64 }`
  - `quote_sell(pool, side, shares: MicroShares) -> Result<SellQuote, AmmError>`; `SellQuote { collateral_out: MicroUsd, fee: MicroUsd, pool_after: Pool }` (`collateral_out` is post-fee)
  - `price_micro(pool, side) -> i64` (display only — never used in settlement)
  - `AmmError`: `EmptyPool`, `AmountTooSmall`, `InputTooLarge`, `Overflow` — **no `DrainsPool` variant** (R3 addendum: the smaller sell root approaches the opposite reserve asymptotically from below, so post-guard `c < no` holds for every valid input; an unreachable error arm would break the 100%-coverage gate. No-drain is proven as a property instead: after any valid sell, every reserve stays ≥ 1.)
  - Internal: `isqrt_u128(x: u128) -> u128` (floor square root, Newton's method)

**Input bounds, closed under state transitions (R1/codex M1, tightened in R2/codex M1):** `MAX_RESERVE = 10^15` micro-shares and `MAX_AMOUNT = 10^15` micro (= $10^9 — far beyond any realistic pool). `Pool::new`, `quote_buy`, and `quote_sell` reject larger *inputs* with `AmmError::InputTooLarge`, **and every quote additionally rejects if any RESULTING reserve would exceed `MAX_RESERVE`** — so the cap is an invariant of reachable pool states, not just of one call (a capped buy of `MAX_AMOUNT` into a `MAX_RESERVE` pool would otherwise mint a `~2·MAX_RESERVE` reserve that the next quote rejects). Under these closed caps every intermediate (`b = y + s + n ≤ 3·10^15`, `b² ≤ 9·10^30`, `4·s·n ≤ 4·10^30`) fits `u128` (max ≈ 3.4·10^38) with huge headroom; all products/discriminants are computed in **checked `u128`** (values non-negative by construction), converting back to `i64` only after range checks. Property tests exercise the bounds: reserves/amounts at `MAX_*`, `MAX_* − 1`, just above (expect `InputTooLarge`), and buys/sells whose *result* would breach the cap (expect `InputTooLarge`, state unchanged). The `sell_cannot_drain_pool` test accepts `DrainsPool | InputTooLarge | Overflow` (the huge input legitimately trips the input cap first). The `isqrt_u128` square-roundtrip property generates `x in 1..=u64::MAX as u128` (so `x*x` cannot overflow and `x*x − 1` cannot underflow) plus explicit `0, 1, 2, 3, 4, u128::MAX` unit cases.

**The math (NO side is symmetric with reserves swapped):**

- *Buy YES with gross collateral `c`:* `(net, fee) = apply_fee(c)`; mint `net` complete sets; pool takes the NO leg: `no' = no + net`; pool must retain `yes_pool' = ceil(yes·no / no')` (**ceil keeps rounding dust in the pool**); `shares_out = yes + net − yes_pool'`; error `AmountTooSmall` if `shares_out ≤ 0`.
- *Sell `s` YES for collateral `c`:* return `s` shares to pool, then burn `c` complete sets from the pool such that the invariant holds: `(yes + s − c)(no − c) = yes·no`. Expanding: `c² − c·(yes + s + no) + s·no = 0`, so with `b = yes + s + no`:
  `c = (b − isqrt(b² − 4·s·no)) / 2` (floor; the smaller root is the valid one).
  After the floor, decrement `c` while `(yes + s − c)(no − c) < yes·no` (guard loop, ≤ 2 iterations in practice). Post-guard `c < no` holds for every valid input (the exact root approaches `no` asymptotically from below), so there is no drain error arm — the no-drain guarantee is asserted as a property (`pool_after` reserves ≥ 1 after every accepted sell). Errors: `AmountTooSmall` if `c ≤ 0`. Fee then comes out of `c` via `apply_fee` (fee on the *sell proceeds*).
- *Prices:* `price_micro(Yes) = no·1_000_000 / (yes+no)` (floor). Display only.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::money::{BasisPoints, MicroShares, MicroUsd};
    use proptest::prelude::*;

    fn pool(y: i64, n: i64) -> Pool { Pool::new(MicroShares(y), MicroShares(n), BasisPoints(100)).unwrap() }

    #[test]
    fn buy_at_even_pool_returns_close_to_double() {
        // symmetric $1000 pool, buy $10 gross: ~19.6 YES out at ~51¢ avg
        let q = quote_buy(&pool(1_000_000_000, 1_000_000_000), Side::Yes, MicroUsd(10_000_000)).unwrap();
        assert!(q.shares_out.0 > 19_000_000 && q.shares_out.0 < 20_000_000);
        assert_eq!(q.fee, MicroUsd(100_000)); // 1% of $10
    }

    #[test]
    fn sell_cannot_drain_pool() {
        // over-cap input trips the input cap (i64::MAX/4 > MAX_AMOUNT — R3/codex B2)
        let r = quote_sell(&pool(1_000, 1_000), Side::Yes, MicroShares(i64::MAX / 4));
        assert!(matches!(r, Err(AmmError::InputTooLarge)));
        // an enormous in-cap sell SUCCEEDS but can never drain: c < no by construction
        // (the smaller root approaches `no` asymptotically from below — R3 addendum)
        let q = quote_sell(&pool(1_000, 1_000), Side::Yes, MicroShares(1_000_000_000)).unwrap();
        assert!(q.pool_after.no.0 >= 1 && q.pool_after.yes.0 >= 1);
    }

    proptest! {
        /// Invariant never decreases on buys (rounding favors pool).
        #[test]
        fn buy_never_decreases_k(y in 1_000_000i64..1_000_000_000_000, n in 1_000_000i64..1_000_000_000_000, c in 1i64..10_000_000_000) {
            let p = pool(y, n);
            if let Ok(q) = quote_buy(&p, Side::Yes, MicroUsd(c)) {
                prop_assert!(i128::from(q.pool_after.yes.0) * i128::from(q.pool_after.no.0)
                          >= i128::from(y) * i128::from(n));
                prop_assert!(q.shares_out.0 > 0);
            }
        }

        /// Immediate round-trip is never profitable, even at zero fee (pure rounding).
        #[test]
        fn round_trip_never_profits(y in 1_000_000i64..1_000_000_000_000, n in 1_000_000i64..1_000_000_000_000, c in 1i64..10_000_000_000) {
            let p = Pool::new(MicroShares(y), MicroShares(n), BasisPoints(0)).unwrap();
            if let Ok(b) = quote_buy(&p, Side::Yes, MicroUsd(c)) {
                if let Ok(s) = quote_sell(&b.pool_after, Side::Yes, b.shares_out) {
                    prop_assert!(s.collateral_out.0 <= c);
                }
            }
        }

        /// Marginal prices are a coherent probability pair within 1 micro.
        #[test]
        fn prices_sum_to_one(y in 1_000i64..1_000_000_000_000, n in 1_000i64..1_000_000_000_000) {
            let p = pool(y, n);
            let s = price_micro(&p, Side::Yes) + price_micro(&p, Side::No);
            prop_assert!((999_998..=1_000_000).contains(&s));
        }

        /// NO-side buy is exactly the mirrored YES-side buy.
        #[test]
        fn no_side_is_symmetric(y in 1_000_000i64..1_000_000_000, n in 1_000_000i64..1_000_000_000, c in 1i64..1_000_000_000) {
            let a = quote_buy(&pool(y, n), Side::No, MicroUsd(c));
            let b = quote_buy(&pool(n, y), Side::Yes, MicroUsd(c));
            match (a, b) {
                (Ok(qa), Ok(qb)) => {
                    prop_assert_eq!(qa.shares_out, qb.shares_out);
                    prop_assert_eq!(qa.pool_after.yes, qb.pool_after.no);
                    prop_assert_eq!(qa.pool_after.no, qb.pool_after.yes);
                }
                (Err(_), Err(_)) => {}
                (a, b) => prop_assert!(false, "asymmetry: {a:?} vs {b:?}"),
            }
        }
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p domain amm` → FAIL.

- [ ] **Step 3: Implement** exactly the math above. `isqrt_u128`: Newton iteration seeded from `1 << ((bits+1)/2)`, loop until stable, return floor root; unit-test it separately on `0, 1, 2, 3, 4, u128::MAX`, and `x*x`/`x*x−1` for random `x` (property test).

- [ ] **Step 4: Run tests + coverage** — `cargo test -p domain amm && just coverage` → PASS at 100% (cover every error arm explicitly).

- [ ] **Step 5: Commit** — `git commit -m "feat(domain): integer CPMM over complete sets with pool-favoring rounding"`

---

### Task 4: Market lifecycle state machine

**Files:**
- Create: `crates/domain/src/market.rs`

**Interfaces:**
- Produces:
  - `MarketState { Draft, Scheduled, Live, Closing, Closed, Resolving, Resolved, Paid, Voided }` — one canonical enum (R3: the earlier draft published two conflicting lists); `Voided` is the executable terminal for D21's void rule (R2/codex M4).
  - `MarketEvent { Approve, GoLive, EnterCloseWindow, Close, StartIntegritySweep, Resolve, Pay, VoidLowParticipation, VoidByAdmin }`
  - `transition(state: MarketState, event: MarketEvent) -> Result<MarketState, TransitionError>` — pure function, exhaustive match, no catch-all arm (`match (state, event)` covering every pair; new states/events force compile errors at every site)
  - Legal edges (spec §4.2 + D21): `Draft→Scheduled (Approve)`, `Scheduled→Live (GoLive)`, `Live→Closing (EnterCloseWindow)`, `Closing→Closed (Close)`, `Closed→Resolving (StartIntegritySweep)`, `Closed→Resolved (Resolve)` (instant path), `Resolving→Resolved (Resolve)`, `Resolved→Paid (Pay)`, **`Closed→Voided` / `Resolving→Voided` (`VoidLowParticipation`**, automatic per D21 anti-void rules**)**, and **`VoidByAdmin` from every pre-`Paid` state** (`Draft|Scheduled|Live|Closing|Closed|Resolving|Resolved → Voided`). `Voided` and `Paid` are terminal: every event from them is `Illegal`.
  - **Void settlement (R3/codex B3 — historical replay is NOT non-negativity-safe):** reversing a market's ledger history can debit sellers who already withdrew their proceeds, so unrestricted `Reversal` replay is unrepresentable under Task 2's rules. Void instead settles **current-state, from escrow**: every outstanding share redeems at the neutral value (`r_yes = r_no = 500_000` micro — every complete set pays exactly $1) through the standard Task 6 `settle_market` machinery, inheriting its conservation + dust guarantees. Fees are not auto-refunded (a goodwill fee-refund from the fee account is a config knob). The §10.2 manual full unwind remains an *admin fraud tool* whose clawback legs may legitimately fail on insufficient funds and queue as receivables — it is not the void path. Implemented in Phase 1; Phase 0 owns the lifecycle edges only.
  - `TransitionError { Illegal { from: MarketState, event: MarketEvent } }`

- [ ] **Step 1: Write the failing tests** — walk the two legal resolution paths end-to-end (with and without integrity sweep) asserting each hop; walk both void paths (`Closed→Voided` automatic, `Live→Voided` admin); assert `transition(Paid, anything)` and `transition(Voided, anything)` are `Illegal`; a table of representative illegal pairs (`Draft→Close`, `Live→Pay`, `Resolved→GoLive`, `Resolved→VoidLowParticipation`) returns `Illegal`; the exhaustive match means the illegal-pair table must cover every remaining arm for 100% coverage.

```rust
#[test]
fn happy_path_without_sweep() {
    use MarketEvent::*; use MarketState::*;
    let path = [(Draft, Approve, Scheduled), (Scheduled, GoLive, Live),
                (Live, EnterCloseWindow, Closing), (Closing, Close, Closed),
                (Closed, Resolve, Resolved), (Resolved, Pay, Paid)];
    for (from, ev, to) in path {
        assert_eq!(transition(from, ev), Ok(to), "{from:?} --{ev:?}--> {to:?}");
    }
}
```

- [ ] **Step 2: Run to verify failure**, **Step 3: implement the exhaustive match**, **Step 4: tests + 100% coverage (every match arm hit — the illegal-pair table must enumerate every remaining arm)**, **Step 5: Commit** — `git commit -m "feat(domain): exhaustive market lifecycle state machine"`

---

### Task 5: Vote scoring (integer basis points)

**Files:**
- Create: `crates/domain/src/scoring.rs`

**Interfaces:**
- Consumes: `Side` (Task 3).
- Produces:
  - `score_vote(side: Side, crowd_guess_pct: u8, actual_yes_bps: u16) -> Result<VoteScore, ScoringError>`
  - `VoteScore { accuracy_bp: u16, majority_bp: u16, score_bp: u16 }` (all 0..=10 000)
  - `ScoringError { GuessOutOfRange, ActualOutOfRange }` (guess > 100; actual > 10 000)
  - Math (spec §3.2 in bps): `diff = |guess·100 − actual|`; `accuracy_bp = max(0, 10_000 − diff·4)` (linear kernel hitting 0 at 2 500 bps = 25 pts); `majority_bp = 10_000` if side matches the winner else `0`; **tie rule: `actual == 5_000` awards `majority_bp = 10_000` to both sides** (documented, deliberate — nobody is "wrong" at a dead heat); `score_bp = (75·accuracy_bp + 25·majority_bp) / 100`.

- [ ] **Step 1: Write the failing tests** — exact guess → `accuracy_bp == 10_000`; guess 25 pts off → 0; 12.5 pts off → 5 000; YES voter at `actual = 5_001` gets majority, at `4_999` doesn't, at `5_000` does (tie rule, both sides); perfect YES vote at `actual = 7_000` (guess 70) → `score_bp == 10_000`; property test (R1/codex m3 form): fix one `actual` and one side, generate **two valid guesses ordered by absolute distance from it**, and assert only `accuracy_bp(closer) >= accuracy_bp(farther)` plus range `0..=10_000` — majority is held constant by construction, and the tie boundary keeps its own explicit unit tests.

- [ ] **Step 2–5:** failing run → implement (u32 intermediates suffice) → tests + 100% coverage → `git commit -m "feat(domain): published vote scoring in integer basis points"`

---

### Task 6: Resolution math (redemption + conservation with dust)

**Files:**
- Create: `crates/domain/src/resolution.rs`

**Interfaces:**
- Consumes: `MicroUsd`, `MicroShares`, `Side`.
- Produces:
  - `redemptions(actual_yes_bps: u16) -> Result<(MicroUsd, MicroUsd), ResolutionError>` — per-share redemption `(yes, no)` in micro-USD per whole share: `yes = actual_yes_bps · 100`, `no = 1_000_000 − yes` (**exact by construction: sums to 1_000_000**)
  - `position_payout(shares: MicroShares, redemption_per_share: MicroUsd) -> Result<MicroUsd, ResolutionError>` — `shares·redemption / 1_000_000`, **floor** (dust favors house)
  - `settle_market(holdings: &[(AccountId, Side, MicroShares)], actual_yes_bps: u16, escrow: MicroUsd) -> Result<Settlement, ResolutionError>` — **`holdings` must be every outstanding share balance, including the pool's own inventory and any house/LP position** (R1/codex M2: settling user positions alone mislabels unsold pool inventory as dust). `Settlement { payouts: Vec<(AccountId, MicroUsd)>, dust: MicroUsd }` with **`sum(payouts) + dust == escrow` exactly, else `Err(ConservationViolated)`** — the solvency check runs inside the domain function, not just in tests.
  - **Escrow/dust contract (R1/codex M7):** `escrow` is definitionally `minted_sets × 1_000_000` micro. Because `r_yes + r_no == 1_000_000` exactly, the exact (unfloored) claims over *all* holdings sum to exactly `escrow`; per-holding floors each lose < 1 micro, so **`0 ≤ dust < holdings.len()`** — enforced as part of conservation, not a loose bound.
  - `ResolutionError { Overflow, ConservationViolated { expected: MicroUsd, got: MicroUsd }, ActualOutOfRange, HoldingsIncomplete { minted: MicroShares, presented: MicroShares } }` — `settle_market` recomputes total presented YES and NO shares and errors if either differs from `minted_sets` (the completeness check that makes "forgot the pool" a hard error instead of silent dust)

- [ ] **Step 1: Write the failing tests** — redemptions at 0 / 5 000 / 10 000 bps sum to exactly 1_000_000; single holder of 1 whole YES share at 73.5% gets exactly 735 000 micro; a two-holder market where the pool holds unsold inventory settles the pool account too, with `dust < holdings.len()`; presenting holdings missing the pool inventory → `Err(HoldingsIncomplete)`; property test: mint `m` complete sets, split YES and NO arbitrarily across accounts *including a pool account*, settle at random `actual_yes_bps` → `sum + dust == escrow` exactly and `0 <= dust < holdings.len()`; edge tests at `holdings.len()` 1 and maximal fractional residues (every holding just under the next micro).

- [ ] **Step 2–5:** failing run → implement (i128 intermediates) → tests + 100% coverage → `git commit -m "feat(domain): resolution redemption math with exact conservation"`

---

### Task 7: Postgres schema v1

**Files:**
- Create: `migrations/0001_init.sql`

**Interfaces:**
- Consumes: `docs/er-diagram.mermaid` — **the column-level contract; implement it 1:1** (all 20 tables incl. `agent_runs`, `agent_steps` from spec §10.3, plus `pending_actions`).
- Produces: a database the Rust adapters (Phase 1) and Python service (Task 8) both target.

Critical DDL that must appear exactly (the rest follows the ER file; R1 additions marked):

```sql
create table ledger_accounts (
  id uuid primary key default gen_random_uuid(),
  -- R1: 'external' is the contra class for the outside world (may go negative)
  owner_type text not null check (owner_type in ('user','pool','fees','house','escrow','external')),
  owner_id uuid,
  -- R1: cash and non-withdrawable bonus credits are separate accounts from day one
  currency text not null default 'usdc' check (currency in ('usdc','usdc_credit')),
  created_at timestamptz not null default now()
);
-- R3 (codex M1): the domain's DuplicateExternal rule, mirrored in SQL — reconciliation
-- breaks if two contra accounts exist for one currency.
create unique index ledger_accounts_one_external_per_currency
  on ledger_accounts (currency) where owner_type = 'external';

create table ledger_transactions (
  id uuid primary key default gen_random_uuid(),
  kind text not null check (kind in
    ('deposit','trade','payout','withdrawal','seed','reversal','credit_grant','credit_convert')),
  idempotency_key text not null unique,
  created_at timestamptz not null default now()
);

create table ledger_entries (
  id bigint generated always as identity primary key,
  txn_id uuid not null references ledger_transactions(id),
  account_id uuid not null references ledger_accounts(id),
  amount_micro bigint not null check (amount_micro <> 0),
  created_at timestamptz not null default now()
);
create index ledger_entries_account_idx on ledger_entries (account_id, id);

-- R1 (codex M5): sum-to-zero cannot be a row CHECK (cross-row). Phase 0: enforced by
-- domain::ledger (the only writer is the test suite). A DEFERRED CONSTRAINT TRIGGER is a
-- BLOCKING precondition for merging any Phase-1 write adapter — and it must validate
-- sums GROUPED BY (txn_id, account currency), not the transaction-wide scalar
-- (R3/codex M1: an all-currencies scalar sum would readmit credit→cash conversion),
-- and reject a transaction header committed with zero entries.

create table markets (
  id uuid primary key default gen_random_uuid(),
  slug text not null unique,
  question text not null,
  status text not null check (status in
    ('draft','scheduled','live','closing','closed','resolving','resolved','paid','voided')),
  -- R1: locked allocation for public vote numbers (update ... returning in the vote txn)
  vote_seq_counter bigint not null default 0,
  -- R1 (D21) + R2 (codex M4): must be configured positive before GoLive — no zero default
  min_votes_to_resolve int not null check (min_votes_to_resolve > 0),
  opens_at timestamptz,
  closes_at timestamptz,
  tally_hidden_at timestamptz,
  created_at timestamptz not null default now()
);

create table outcomes (
  id uuid primary key default gen_random_uuid(),
  market_id uuid not null references markets(id),
  label text not null,
  idx int not null,
  final_vote_bps int,
  redemption_micro bigint,
  unique (market_id, idx),         -- R1 (codex M3): outcome identity within a market
  unique (id, market_id)           -- R2 (codex M2): composite FK target proving market agreement
);

create table trades (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  market_id uuid not null,
  outcome_id uuid not null,
  -- R2 (codex M2): the composite FK makes a cross-market (outcome, market) pair unrepresentable
  foreign key (outcome_id, market_id) references outcomes (id, market_id),
  run_id uuid references agent_runs(id),                       -- R1: agent causal chain
  pending_action_id uuid unique references pending_actions(id), -- R2 (codex M3): 1:1 as ER shows
  txn_id uuid references ledger_transactions(id),               -- R2 (codex M3): trade → ledger
  side text not null check (side in ('buy','sell')),
  collateral_micro bigint not null check (collateral_micro > 0),
  shares_micro bigint not null check (shares_micro > 0),
  fee_micro bigint not null check (fee_micro >= 0),
  seq bigint not null,
  created_at timestamptz not null default now(),
  unique (market_id, seq)
);

create table votes (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  market_id uuid not null,
  outcome_id uuid not null,
  foreign key (outcome_id, market_id) references outcomes (id, market_id), -- R2 (codex M2)
  run_id uuid references agent_runs(id),                        -- R1: agent causal chain
  pending_action_id uuid unique references pending_actions(id), -- R2 (grok M5): oracle chain parity
  crowd_guess_pct int not null check (crowd_guess_pct between 0 and 100),
  seq bigint not null,
  idempotency_key text not null unique,             -- R1 (codex M4): retry-safe casts
  created_at timestamptz not null default now(),
  unique (user_id, market_id),
  unique (market_id, seq)
);
-- R1 (codex M4/grok m2): seq is ALLOCATED, not raced: the Phase-1 cast-vote use case runs
--   update markets set vote_seq_counter = vote_seq_counter + 1
--     where id = $1 returning vote_seq_counter
-- in the same transaction as the vote insert (row lock serializes; idempotency_key
-- makes network retries return the original vote instead of a second number).

create table deposits (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  chain_sig text not null unique,
  amount_micro bigint not null check (amount_micro > 0),
  status text not null check (status in ('seen','confirmed','credited')),
  txn_id uuid references ledger_transactions(id),
  created_at timestamptz not null default now()
);

create table pending_actions (
  id uuid primary key default gen_random_uuid(),
  thread_id text not null,
  run_id uuid not null references agent_runs(id),        -- R2 (codex M3): FK made explicit
  kind text not null check (kind in ('trade','vote')),   -- R1: votes are in the corridor
  payload jsonb not null,
  expires_at timestamptz not null,
  consumed_at timestamptz,
  created_at timestamptz not null default now()
);
-- R2 (codex B3): at most ONE active pending action per thread, enforced by the database —
-- two replicas cannot both hold an active preview for the same phone.
create unique index pending_actions_one_active_per_thread
  on pending_actions (thread_id) where consumed_at is null;
-- R2 (codex B3) + R3 (codex B1): protocol, always inside the per-thread advisory-lock
-- transaction (pg_advisory_xact_lock(hashtext(thread_id))):
--   1. EXPIRY SWEEP (prevents the expired-row deadlock — an expired unconsumed row
--      would otherwise block the partial unique index forever):
--        update pending_actions set consumed_at = now()
--          where thread_id = $1 and consumed_at is null and expires_at <= now();
--   2. CONFIRM consume:
--        update pending_actions set consumed_at = now()
--          where id = $2 and consumed_at is null and expires_at > now()
--          returning id, kind, payload;
--      zero rows = lost the race or expired → reply "nothing pending".
--   3. CREATE pending: plain insert (safe: sweep ran, index enforces one-active).
-- Required tests (Task 8 integration + Phase 1): expiry then new preview on the same
-- thread succeeds; two concurrent connections racing confirm → exactly one consume;
-- two concurrent previews on one thread → exactly one active row.

create table agent_runs (
  id uuid primary key,
  thread_id text not null,
  channel text not null,                 -- R1: dedupe scope
  trigger text not null,
  user_id uuid,
  inbound_msg_id text,
  final_intent text,
  status text not null check (status in ('running','ok','error','guard_blocked')),
  total_tokens int,
  cost_micro bigint,
  trace_id text,
  started_at timestamptz not null,
  ended_at timestamptz,
  -- R1 (codex m4): webhook redelivery can never double-run a message
  unique (channel, inbound_msg_id)
);
create index agent_runs_thread_idx on agent_runs (thread_id, started_at);
create index agent_runs_started_idx on agent_runs (started_at);
-- R2 (codex M6): partitioning is deliberately NOT promised — PostgreSQL partitioned
-- unique keys must include the partition key, which would break PK(id) and the global
-- (channel, inbound_msg_id) dedupe. Decision: these tables stay UNPARTITIONED with
-- scheduled retention jobs (batched DELETE by started_at + archival copy + PII
-- redaction per data class) — policy in spec §10.3.

create table agent_steps (
  id uuid primary key,
  run_id uuid not null references agent_runs(id),
  seq int not null,
  node text not null,
  kind text not null check (kind in ('llm','tool','gate')),
  state_before jsonb,
  state_after jsonb,
  prompt_template_id text,
  prompt_version text,
  rendered_prompt text,
  model text,
  params jsonb,
  raw_output text,
  parsed_output jsonb,
  rationale text,
  tokens_in int,
  tokens_out int,
  latency_ms int,
  error text,
  unique (run_id, seq)
);

-- R1 (codex M3): non-negativity where the schema can state it
alter table pool_reserves add constraint pool_reserves_nonneg check (reserve_micro_shares >= 0);
alter table positions add constraint positions_nonneg check (shares_micro >= 0);

-- R2 (codex M2): pool_reserves must prove pool and outcome share a market:
-- pools carries unique(id, market_id); pool_reserves carries market_id with BOTH
-- composite FKs, making a cross-market (pool, outcome) row unrepresentable.
alter table pools add constraint pools_id_market_uk unique (id, market_id);
alter table pool_reserves add column market_id uuid not null;
alter table pool_reserves add constraint pr_pool_fk
  foreign key (pool_id, market_id) references pools (id, market_id);
alter table pool_reserves add constraint pr_outcome_fk
  foreign key (outcome_id, market_id) references outcomes (id, market_id);
```

Note: migration file order is `users → markets → outcomes → pools → pool_reserves → ledger_* → agent_runs → pending_actions → trades/votes → …` so every FK above resolves (order above is expository).

- [ ] **Step 1:** Write the full migration (every ER table; foreign keys and checks per the diagram; `outcomes.final_vote_bps` and `redemption_micro` nullable until resolve).
- [ ] **Step 2:** `just infra-up && cargo install sqlx-cli --no-default-features --features postgres` (if absent) `&& DATABASE_URL=postgres://opinions:opinions@localhost:5432/opinions just migrate`
- [ ] **Step 3:** Verify: `psql postgres://opinions:opinions@localhost:5432/opinions -c "\dt"` → 20+ tables; run the migration twice → second run is a no-op (sqlx tracks applied versions).
- [ ] **Step 4:** Commit — `git commit -m "feat(db): schema v1 from ER contract incl. agent audit tables"`

---

### Task 8: Conversation-service skeleton (FastAPI + LangGraph + recorder)

**Files:**
- Create: `services/converse/pyproject.toml`, `services/converse/src/converse/schemas.py`, `services/converse/src/converse/graph.py`, `services/converse/src/converse/recorder.py`, `services/converse/src/converse/app.py`, `services/converse/tests/test_schemas.py`, `services/converse/tests/test_graph.py`, `services/converse/tests/test_recorder.py`

**Interfaces:**
- Consumes: `agent_runs`/`agent_steps`/`pending_actions` tables (Task 7).
- Produces: `POST /webhooks/sendblue` accepting `{"number": str, "content": str, "message_id": str}` → runs the graph → returns `{"reply": str, "run_id": str}`. Duplicate `(channel, message_id)` returns the original run's reply without re-running the graph (dedupe test included). **LLM seam (R1/codex ckpt 7): one async protocol shared by every future agent node, provider-agnostic** —

  ```python
  class AgentNode(Protocol):
      async def __call__(self, rendered_prompt: str, context: dict) -> BaseModel: ...
  ```

  injected per node at graph build (`build_graph(router=..., recorder=...)`; extractors/composer slots arrive with their nodes in Phase 1 through the same signature). Tests pass fakes; production adapters wrap the model client + structured output behind this protocol so no node couples to a provider API. Real model calls are **out of scope for Phase 0** (metaprompts land in Phase 1 with evals). The pre-router lexical pending-gate (spec §7.5) is a deterministic node in this skeleton from day one: tests cover confirm-match, cancel-match, and the anything-else auto-cancel path with a pending action present.

- [ ] **Step 1: Write `pyproject.toml`** — `[project]` name `converse`, python `>=3.12`, deps: `fastapi`, `uvicorn`, `langgraph`, `langgraph-checkpoint-postgres`, `pydantic>=2`, `asyncpg`, `httpx`; dev: `pytest`, `pytest-asyncio`, `anyio`. Managed with `uv`.

- [ ] **Step 2: Write the failing schema tests, then `schemas.py`**

```python
# schemas.py — every LLM structured output carries a required rationale (spec §10.3)
# R2 (grok M4 / codex M3): there is NO confirm/cancel intent — confirmation authority
# lives in the deterministic pre-router pending gate, so the router cannot express it.
from enum import StrEnum
from pydantic import BaseModel, Field

class Intent(StrEnum):
    LIST_MARKETS = "list_markets"; PRICE = "price"; PORTFOLIO = "portfolio"
    ASK_ABOUT_MARKET = "ask_about_market"; VOTE = "vote"; PLACE_TRADE = "place_trade"
    CHITCHAT = "chitchat"; UNKNOWN = "unknown"

class RouterOutput(BaseModel):
    intent: Intent
    rationale: str = Field(min_length=1)

class TradeParams(BaseModel):
    market_ref: str
    side: str = Field(pattern="^(yes|no)$")
    amount_usd_micro: int = Field(gt=0)
    action: str = Field(pattern="^(buy|sell)$")
    rationale: str = Field(min_length=1)

class VoteParams(BaseModel):
    market_ref: str
    side: str = Field(pattern="^(yes|no)$")
    crowd_guess_pct: int = Field(ge=0, le=100)
    rationale: str = Field(min_length=1)

class ComposerOutput(BaseModel):
    text: str
    numbers_used: list[int]   # every numeric figure in `text`, for the output guard
    rationale: str = Field(min_length=1)
```

Tests: `TradeParams` rejects `amount_usd_micro=0`, missing `rationale`, `side="maybe"`; `Intent("vote")` round-trips.

- [ ] **Step 3: Write the failing graph tests, then `graph.py`** — a `StateGraph` with topology **`load_session → pending_gate → (execute_pending | clear_pending | reprompt | router)`**, then `router → (respond_stub per intent | trade_preview_stub | vote_preview_stub) → compose_stub` (R2/grok M4: the executable skeleton carries the locked topology — the pending gate is a deterministic node in front of the router from day one). `MemorySaver` checkpointer in tests (`AsyncPostgresSaver` behind `DATABASE_URL` in prod), `thread_id = phone`. The gate implements the spec §7.5 normalization: lowercase → strip punctuation/emoji → collapse whitespace → drop politeness tokens; empty/reaction-only input **re-prompts without clearing pending**. Tests: (a) fake router returning `Intent.PRICE` → reply present, `intent == "price"`; (b) same thread twice → checkpointer restores (turn counter == 2); (c) **adversarial gate suite**: with a pending action — "Confirm!" executes; "yes." executes; "cancel" clears; "👍" / empty re-prompts *keeping* pending; "yes but $20" cancels then routes; and **a malicious fake router that always returns a trade-shaped output cannot reach the execute node** (assert execute stub not called on any non-allowlisted text).

- [ ] **Step 4: Write the failing recorder test, then `recorder.py`** — `Recorder` protocol with `start_run/record_step/end_run`; `PgRecorder` (asyncpg, inserts matching Task 7 DDL) and `MemoryRecorder` for unit tests. The graph wrapper decorates every node: snapshot state before/after, write a step row with `kind` (`llm|tool|gate`). Unit test with `MemoryRecorder`: one webhook-driven run produces ≥3 steps in sequence with `state_before/state_after` populated and run status `ok`. Integration tests (marked `@pytest.mark.integration`, skipped without `DATABASE_URL`): rows actually land in `agent_runs`/`agent_steps`; **and the Task 7 pending protocol under `pg_advisory_xact_lock(hashtext(thread_id))` — expiry sweep unblocks a thread whose pending expired, two connections racing a confirm consume exactly once, two concurrent previews yield exactly one active row** (R3/codex B1; the turn-runner acquires the advisory lock around each graph turn, with the in-process asyncio lock as fast path only).

- [ ] **Step 5: Write the failing app test, then `app.py`** — FastAPI `TestClient` posts `{"number": "+15551234567", "content": "price on the coffee market?", "message_id": "m1"}` → 200, body has `reply` and `run_id`; unknown-field payload → 422.

- [ ] **Step 6: Run everything** — `cd services/converse && uv sync && uv run pytest -q` → PASS (integration tests skip cleanly without a DB; run them once with `just infra-up` + `DATABASE_URL` exported).

- [ ] **Step 7: Commit** — `git commit -m "feat(converse): FastAPI+LangGraph skeleton with fake-LLM seam and run recorder"`

---

## Roadmap after Phase 0 (each phase gets its own plan doc at phase start)

Mapping of spec §13 — Phase 0 above unblocks all of it:

1. **Phase 1 — Core write path:** application crate (PlaceTrade/CastVote/ResolveMarket use cases over ports), SQLx adapters + **the ledger sum-zero deferred constraint trigger (blocking before any write adapter merges — R1)**, **vote-seq locked allocation + idempotent casts (R1)**, Axum REST + OpenAPI generation (Python API models generated from it — closes the D15 contract), devnet deposit watcher, first e2e: webhook → preview → lexical confirm → devnet trade.
2. **Phase 2 — Core loop UX + oracle defense (R1 re-scope):** WS gateway + ticks, charts, portfolio, vote→trade gate, resolution at target latency, **minimum vote-integrity bar (phone uniqueness, device fingerprint, vote velocity, min-participation void rule)**, **installable PWA shell** — 's iOS clock is running.
3. **Phase 3 — Economy & full integrity:** rep/leaderboards, IP/ASN clustering + anomaly sweep, hidden tallies **with buy-freeze window (D22)**, LP kill-switch (D23).
4. **Phase 4 — Social & notifications.** 5. **Phase 5 — Content engine (curation + video).** 6. **Phase 6 — simswarm to 2,000 agents + chaos + ops control plane.** 7. **Phase 7 — Money hardening + compliance gate → beta.**

## Reviewer checkpoints — R1 resolutions (full reports: docs/reviews/codex-r1.md, docs/reviews/grok-r1.md)

Both R1 reviewers verdicted **fix-first**; every blocker/major has been applied to this plan and the spec:

1. **Sell-side quadratic** — confirmed correct by both reviewers (smaller root, guard loop, `c ≥ no` drain guard). Applied: overflow was the real issue → enforced input bounds + checked u128 (Task 3, codex M1).
2. **Fee on sells after curve** — confirmed coherent; documented as "fee on trade notional after curve fill."
3. **Rounding** — per-operation <1 micro, ≤2 per fill; the "1 micro aggregate" claim was corrected everywhere (codex m2).
4. **Scoring tie at 50.00%** — both-sides-win kept (both reviewers agreed); published copy must define a tie as both sides winning.
5. **Ledger negativity** — codex B1 was right that pure non-negativity made deposits unrepresentable → `External` contra account class added (Task 2); internal accounts stay non-negative.
6. **Schema** — sum-zero deferred trigger is now a *blocking* Phase-1 precondition; vote-seq locked allocation + idempotency key specified (Task 7); outcome/trade keys and non-negativity checks added (codex M3/M4/M5).
7. **LLM seam** — upgraded to a shared async protocol for all agent nodes (codex ckpt 7); confirm authority moved out of the router entirely (grok B1: pre-router lexical gate).

Remaining open risks (tracked, not blocking Phase 0): counsel outcomes (wagering structure + MSB/custody), Apple MfB approval, LP expected-loss model before real money, agent-table retention/archival jobs live before production traffic (tables are unpartitioned by decision — R2/codex M6).

**R3 addendum:** codex's final round surfaced five more items — expired-pending deadlock, the sell-drain test's expected errors, void-by-replay being non-negativity-unsafe (void is now neutral redemption from escrow), the one-External-per-currency SQL constraint + per-currency trigger grouping, and named gate fixtures with exact tool pins. All applied; see docs/reviews/codex-r3.md and docs/reviews/r3-resolution.md. Grok's final verdict: sound-to-build.
