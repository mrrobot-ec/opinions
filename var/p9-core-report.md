# P9 Rust core audit/fix report

## Scope and constraints

- Owned paths audited/fixed: `crates/domain/**`, `crates/application/**`, `crates/simswarm/**`.
- Dimensions: A concurrency/lock ordering; B arithmetic, conservation, rounding, AMM/isqrt, currency separation, dust, and 5000-bps void settlement; C idempotency/crash safety; F panics/errors/retries; G verification and fake/contract drift; H dead code/stubs/doc claims; clean-architecture/SOLID boundaries.
- Hard constraint honored throughout: no Git or other VCS command was run.
- Rust commands used `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core`; DB-aware commands use `DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core`.

## Baseline

- `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p domain` — green: 93 passed, 0 failed, 0 ignored; doc-tests 0.
- `DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p application` — green: 548 passed, 0 failed, 0 ignored; doc-tests 0.
- `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p simswarm` — green: 63 passed, 0 failed, 0 ignored (61 library + 2 binary); doc-tests 0.

## Real defects

### 1. Deposit-time receivable collection panicked/wrapped when the open total exceeded `i64`

`ops::receivable_collection::auto_collect` used an unchecked iterator `sum::<i64>()` over durable receivable amounts. The analogous withdrawal-path collector already used `checked_add`; the deposit path could panic in checked builds or wrap in optimized builds before deciding how much cash to collect.

Red command:

```text
DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p application ops::receivable_collection::tests::deposit_collection_rejects_an_overflowed_receivable_total -- --exact
```

Exact red result:

```text
running 1 test
test ops::receivable_collection::tests::deposit_collection_rejects_an_overflowed_receivable_total ... FAILED

thread 'ops::receivable_collection::tests::deposit_collection_rejects_an_overflowed_receivable_total' (...) panicked at .../library/core/src/iter/traits/accum.rs:206:1:
attempt to add with overflow

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 548 filtered out
```

Implementation: replaced the unchecked sum with `try_fold` + `checked_add`, returning the existing typed `AppError::Overflow` before any ledger or receivable write. The regression test remains in the suite.

Green command: identical to the red command above.

Exact green result:

```text
running 1 test
test ops::receivable_collection::tests::deposit_collection_rejects_an_overflowed_receivable_total ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 548 filtered out
```

### 2. AML velocity aggregation could panic or wrap and fail open

`money::aml::evaluate_aml` accumulated 24-hour deposit and withdrawal legs with unchecked `i64::sum`. A legitimate durable history whose mathematical total exceeds `i64::MAX` panicked in checked builds and could wrap below the velocity threshold in optimized builds, suppressing a money-egress flag. The implementation now saturates both totals at `i64::MAX`, preserving a representable evidence value and failing closed.

Red command:

```text
DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p application money::aml::tests::velocity_totals_saturate_and_flag_when_history_exceeds_i64 -- --exact
```

Exact red result:

```text
running 1 test
test money::aml::tests::velocity_totals_saturate_and_flag_when_history_exceeds_i64 ... FAILED

thread 'money::aml::tests::velocity_totals_saturate_and_flag_when_history_exceeds_i64' (...) panicked at .../library/core/src/iter/traits/accum.rs:206:1:
attempt to add with overflow

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 549 filtered out
```

Green command: identical to the red command above.

Exact green result:

```text
running 1 test
test money::aml::tests::velocity_totals_saturate_and_flag_when_history_exceeds_i64 ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 549 filtered out
```

### 3. The invariant checker itself panicked/wrapped on valid large aggregates

The invariant sweep used unchecked `i64` aggregation for external/internal account totals and Phase-7 withheld, suspense, bonus-promise, deposit-liability, and active-hold identities. Individual accounts and facts are `i64`, but a valid multi-row aggregate can exceed that range; the checker could therefore panic in debug or produce a false pass/failure after release wrapping. Aggregates and the receivable collected-plus-written-off comparison now use `i128`.

Red command:

```text
DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p application integrity::invariant_sweep::tests::aggregate_money_identities_handle_totals_larger_than_i64 -- --exact
```

Exact red result:

```text
running 1 test
test integrity::invariant_sweep::tests::aggregate_money_identities_handle_totals_larger_than_i64 ... FAILED

thread 'integrity::invariant_sweep::tests::aggregate_money_identities_handle_totals_larger_than_i64' (...) panicked at .../library/core/src/iter/traits/accum.rs:206:1:
attempt to add with overflow

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 550 filtered out
```

Green command: identical to the red command above.

Exact green result:

```text
running 1 test
test integrity::invariant_sweep::tests::aggregate_money_identities_handle_totals_larger_than_i64 ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 550 filtered out
```

The later row-boundary repair in defect 8 completes this fix: the aggregate
now arrives from the adapter as `i128` rather than overflowing in Postgres or
during decode first.

### 4. Referral binding and resolution did not follow one global class-2 user-lock order

Referral binding read both parties before it had locked both parties, and a
paid resolution acquired voters and referral participants in separate sorted
runs. Two transactions could therefore acquire overlapping user locks in
opposite orders, while a bind could change between resolution enumeration and
the Paid hook.

The repaired paths lock the referral pair in UUID order before its reads, and
resolution forms one deduplicated voter/referral union and takes one globally
sorted lock run before reading the locked facts. The fake now exposes the
unbound qualifying referee to the pre-lock enumeration so the production
adapter contract cannot silently omit it.

Red/green evidence:

| Exact test selector | Red | Green |
|---|---|---|
| `money::referrals::tests::bind_waits_for_both_participant_locks_in_uuid_order` | assertion showed the lower UUID was not the first complete lock acquisition; 0 passed, 1 failed, 551 filtered | 1 passed, 0 failed, 551 filtered |
| `resolve_market::tests::resolution_acquires_one_globally_sorted_user_lock_run` | timed assertion showed resolution starting a second lock run; 0 passed, 1 failed, 552 filtered | 1 passed, 0 failed, 552 filtered |

Both selectors were run with:

```text
DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p application <selector> -- --exact
```

### 5. Resolution payout skipped the accepted receivable lien

Resolution credited winners but did not invoke oldest-first receivable
collection in the same transaction. A cash-creating payout could therefore
leave an already-open debt untouched. `ResolveTx` now includes
`ReceivableCollectionIo`; after the payout ledger write and mandatory crash
point, unique positive user recipients are collected in UUID order before the
remaining settlement facts commit.

Exact selector:
`resolve_market::tests::payout_collects_the_recipients_open_receivable_in_the_same_transaction`.
Red was `outstanding 10000000 != 0` with 0 passed, 1 failed, 553 filtered;
green was 1 passed, 0 failed, 553 filtered.

### 6. Currency cancellation could make a corrupt ledger pass its invariant sweep

`AccountBalanceRow` omitted currency and the sweep summed every currency
together. Equal and opposite USDC/USDC-credit drift could cancel, while the
Phase-7 withheld/suspense/reserve projections also accidentally counted
non-cash balances. The row now carries `Currency`, contra identities are
checked independently per currency, and cash-only identities select USDC.

Red/green evidence:

| Exact test selector | Red | Green |
|---|---|---|
| `fakes::ops::tests::invariant_snapshot_rejects_opposite_per_currency_drifts` | opposite per-currency drift incorrectly passed; 0 passed, 1 failed, 554 filtered | 1 passed, 0 failed, 555 filtered |
| `integrity::invariant_sweep::tests::money_identity_facts_ignore_non_cash_currency_balances` | cash projection was 10 instead of 3; 0 passed, 1 failed, 555 filtered | 1 passed, 0 failed, 555 filtered |

### 7. Unwind shortfall aggregation could panic or wrap

The unwind path summed individual shortfalls with unchecked `i64` iterator
addition. It now uses `try_fold` plus `checked_add` and returns
`AppError::Overflow` without writing partial compensation.

Exact selector:
`ops::unwind_market::tests::unwind_rejects_a_shortfall_total_larger_than_i64`.
Red panicked in iterator accumulation with 0 passed, 1 failed, 556 filtered;
green was 1 passed, 0 failed, 556 filtered.

### 8. Account-balance snapshots narrowed a valid aggregate before the checker saw it

Even after the sweep used `i128`, `AccountBalanceRow.balance_micro` was still
`i64`, and the Postgres projection cast `sum(bigint)` back to `bigint`. Two
valid entries on one append-only account could exceed `i64` and make the
sweep error at its read boundary. The application row and fake now carry
`i128`; the coordinated Postgres projection crosses as exact text and parses
to `i128`.

Exact selector:
`integrity::invariant_sweep::tests::account_balance_snapshot_rows_keep_wide_aggregates`.
Red was an 8-byte row value versus the required 16 bytes, with 0 passed,
1 failed, 557 filtered; green was 1 passed, 0 failed, 557 filtered.

### 9. Deposit admission did not perform AML-at-request evaluation

The observation/admission machine could credit a deposit without evaluating
the pinned structuring and velocity policy. A new `DepositAmlIo` role is part
of `DepositAdmissionTx`; admission calls it after `lock_user` and the status
CAS, but before conversion, collection, enforcement, or credit. The durable
candidate identity is the deposit ID, `source_address` is the counterparty,
and an existing candidate on replay/held reevaluation is not counted again.
Any open flag commits a compliance hold and leaves the inflow in suspense.

Exact selector:
`credit_deposit::tests::deposit_admission_holds_a_shared_source_structuring_flag`.
Red credited the fourth shared-source in-band deposit (`Some(100000003)`
instead of `None`) with 0 passed, 1 failed, 558 filtered. Green was 1 passed,
0 failed, 558 filtered and additionally proved replay plus reevaluation leave
exactly four candidate legs and retain the source counterparty.

### 10. Missing required deposit policies silently weakened the gate

`deposit_kyc_tier` defaulted to 1 when absent/malformed and
`pause_deposits` used the optional-feature boolean reader, which mapped a
missing/malformed row to false. The gate now rejects an absent/non-integer KYC
policy and uses a dedicated required-boolean read for the pause policy;
optional switches such as `feature_referrals` retain their safe optional
semantics. The fake seeds the required catalog values and exposes explicit
removal fixtures.

Red/green evidence (same application command form as above):

| Exact test selector | Red | Green |
|---|---|---|
| `money::enforcement::tests::gate_snapshot_rejects_a_missing_required_deposit_kyc_tier` | unexpectedly returned `Ok`; 0 passed, 1 failed, 560 filtered | exact typed invariant error; 1 passed, 0 failed, 560 filtered |
| `money::enforcement::tests::gate_snapshot_rejects_a_missing_required_deposit_pause_flag` | unexpectedly returned `Ok`; 0 passed, 1 failed, 560 filtered | exact typed invariant error; 1 passed, 0 failed, 560 filtered |

### 11. Simswarm could fabricate both WebSocket and close-to-paid evidence

The runner recorded an HTTP trade success directly as `ws-delivery`, so an
SLO could pass without a WebSocket frame. The live composition now resolves
the five manifest markets, connects the real `/ws` endpoint, waits for their
snapshots, records only real trade frames, deduplicates `outbox_seq`, and
measures the server's durable `created_at` timestamp to client receipt time.
No FIFO correlation with concurrent HTTP sends remains. Separately, observing
Paid without first observing Closed used to invent a zero-millisecond
close-to-paid sample; that path now records nothing.

Red/green evidence:

| Exact simswarm selector | Red | Green |
|---|---|---|
| `engine::runner::tests::http_success_is_not_a_websocket_delivery_sample` | HTTP success produced a WS sample; 0 passed, 1 failed, 61 filtered | 1 passed, 0 failed, 61 filtered |
| `engine::runner::tests::websocket_frames_are_the_only_ws_latency_evidence` | real-frame series was empty instead of `[20, 20]`; 0 passed, 1 failed, 63 filtered | 1 passed, 0 failed, 63 filtered |
| `engine::runner::tests::paid_without_an_observed_closed_state_does_not_invent_a_timer` | a fabricated close-to-paid sample existed; 0 passed, 1 failed, 63 filtered | 1 passed, 0 failed, 63 filtered |

These used:

```text
CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p simswarm <selector> -- --exact
```

### 12. The pinned 120-tick spike required only one WebSocket sample

`SeriesContract` documented one real WS sample per 60 spike ticks but returned
one for every nonzero spike. A single frame could therefore green the p95 for
the pinned 120-tick profile. The floor is now
`ceil(close_spike_ticks / 60)` and conversion failure is fail-closed; neither
the spike nor the `ThinSeries` gate was lowered.

Exact selector: `trace::tests::pinned_spike_requires_two_real_websocket_samples`.
Red was expected 2 versus derived 1, with 0 passed, 1 failed, 63 filtered;
green was 1 passed, 0 failed, 63 library filtered (and 3 binary filtered).

## Verification-quality repair without a production defect

The domain `render_spec` properties compared a value to itself. They were
replaced with generated-input properties that pin dimensions and the exact
escaped/wrapped text lines. This was a verification-theatre finding rather
than a production behavior change, so no fabricated red production test was
claimed. The repository static-quality checker now accepts the owned tests.

## Cross-owner findings / adversarial review

- Resolution lock enumeration, account currency/wide aggregate projection,
  Postgres invariant decoding, and the weak adapter middleware assertion were
  sent to their owning lanes; their coordinated shared-workspace edits were
  observed rather than duplicated in owned files.
- The owning database lane implemented `DepositAmlIo` and
  `required_config_flag` in the shared Postgres transaction. A supplemental
  targeted `cargo check -p adapters` confirmed that the new application port
  surface integrates; no adapter source was edited by this lane.
- Confirmed read-only for the adapter/runtime lane: production did not call
  `IncidentManager::raise`; the continuous invariant loop only logged
  violations. `raise` also marked the first delivery attempted before paging,
  and retry delivery used `UNIX_EPOCH`. This was routed rather than edited
  outside the assigned paths.
- Confirmed read-only for the composition lane: production mounted the core
  router with Phase-7 503 stubs while the real phone, KYC-webhook, and
  compliance routers lacked a production composition/OpenAPI path. This was
  routed rather than edited outside the assigned paths.
- The release script and adapter lane own the live Postgres and pinned 2,000
  agent artifact. Owned simswarm tests prove truthful collection and thinness,
  but this report does not substitute an in-memory run for that live release
  evidence.

## Rejected hypotheses / checked with no finding

- Payout-holder enumeration and vote enumeration are independent queries, but no valid holder-only user exists: the D4 `PlaceTrade` gate rejects every unvoted trader before position creation. The earlier holder-only hypothesis was retracted rather than fixed.
- AMM/isqrt rounding, pool conservation, dust routing, and neutral 5000-bps
  settlement were re-read against their domain/application tests. The audit
  found no new defect; the targeted domain and resolution suites remained
  green.
- The weak `render_spec` self-comparisons were a real test-quality issue, but
  not evidence of a rendering defect: independent golden-byte/digest,
  escaping, layout, and line-cap tests already guarded production behavior.

## Changed owned files

- `crates/domain/src/render_spec.rs`
- `crates/application/src/model.rs`
- `crates/application/src/credit_deposit.rs`
- `crates/application/src/resolve_market.rs`
- `crates/application/src/integrity/invariant_sweep.rs`
- `crates/application/src/money/aml.rs`
- `crates/application/src/money/enforcement.rs`
- `crates/application/src/money/mod.rs`
- `crates/application/src/money/referrals.rs`
- `crates/application/src/ops/receivable_collection.rs`
- `crates/application/src/ops/unwind_market.rs`
- `crates/application/src/ports/market.rs`
- `crates/application/src/ports/money.rs`
- `crates/application/src/fakes/mod.rs`
- `crates/application/src/fakes/state.rs`
- `crates/application/src/fakes/ops.rs`
- `crates/application/src/fakes/market.rs`
- `crates/application/src/fakes/credit.rs`
- `crates/simswarm/src/engine/runner.rs`
- `crates/simswarm/src/main.rs`
- `crates/simswarm/src/trace.rs`
- `var/p9-core-report.md`

## Verification and remaining risk

Final owned gates:

| Command | Result |
|---|---|
| `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p domain` | 93 passed, 0 failed, 0 ignored; doc-tests 0 |
| `DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p application` | 561 passed, 0 failed, 0 ignored; doc-tests 0 |
| `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo test -p simswarm` | 64 library + 3 binary passed, 0 failed, 0 ignored; doc-tests 0 |
| `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo clippy -p domain --all-targets -- -D warnings` | green |
| `DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo clippy -p application --all-targets -- -D warnings` | green |
| `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo clippy -p simswarm --all-targets -- -D warnings` | green |
| `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo fmt --all -- --check` | green |
| `python3 scripts/test_test_quality.py` | 3 passed |
| `DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-core cargo check -p adapters` | green supplemental integration check |

No full-workspace test or clippy command was substituted for the required
owned-package gates. No test was ignored, deleted, weakened, or silenced, and
no Git or other VCS command was run.

After all results above were recorded, the exact generated target directory
`/Users/mrrobot/opinions-build/p9-core` was validated (4.3 GiB), deleted as
required, and verified absent. Those build artifacts are reproducible from the
recorded Cargo commands.

Remaining risk is external to the owned implementation: the live Postgres
contract/chaos matrix and pinned 2,000-agent release artifact remain the
adapter/release lanes' evidence, while the routed IncidentManager and router
composition findings require their owners' final verification. The owned
simswarm gate now fails thin without real WebSocket/Closed evidence, but this
audit did not claim a synthetic run as live release proof.
