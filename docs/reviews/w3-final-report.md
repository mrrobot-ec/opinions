# Phase 7 W3 money/compliance implementation report

## Outcome

W3's trade/deposit money path implements D32 and D36 across application,
Postgres, fake, rail, HTTP, and Converse seams. The only outstanding action is
the coordinator-owned mount/OpenAPI registration for the completed
`crates/adapters/src/http/routes/deposit_admin.rs`; see
`docs/reviews/w3-coordinator-diffs.md` and orchestration thread
`msg_9808b7f2072e`.

## Delivered invariants

- Finalized inbound observations first book `External → DepositSuspense`, bind
  the complete chain tuple plus startup rail fingerprint, and progress through
  the explicit deposit state machine.
- Held-deposit admission and source-locked refund require Finance proposal plus
  a distinct Superadmin confirmation with mandatory reason, immutable payload,
  TTL, proposal CAS, audit, decision, event, and money effects in one database
  transaction.
- Refunds use `outbound_payments(subject=deposit_refund)` and the D31 attempt
  lineage: signed bytes persist before broadcast, final receipts bind the
  persisted attempt, expired Unknown attempts require the pinned three-RPC
  non-landing proof, and replacement signatures are monotone and unique.
- A same-bytes rebroadcast of an Unknown attempt deliberately remains Unknown;
  rebroadcast alone is not evidence of landing.
- Grant lots reserve BonusReserve capacity at grant, fee allocations use the
  Trade ledger transaction's fee-credit leg, Paid moves provisional facts to
  finalized, Voided writes reversals only, and conversion remains lazy,
  atomic, and convert-before-collect under `lock_user`.
- Referral binding requires the W2 phone-verification fact and Paid hooks mint
  each qualifying referrer/referee pair once under the existing sorted user
  locks.
- `fee_bps_override` is a typed inherit/override value used by both fee context
  and preview drift; `expected_config_version` is mandatory at trade placement
  and is carried through HTTP and Converse with per-call region propagation.
- Postgres owner decoding is total and rejects unknown vocabulary; invariant
  reads include deposit suspense, BonusReserve coverage, and withdrawal
  attribution.

## Principal files

- Application: `credit_deposit.rs`, `money/credits.rs`,
  `money/referrals.rs`, `ops/config.rs`, `place_trade.rs`,
  `preview_trade.rs`, `cast_vote.rs`, `fakes/state.rs`, and
  `fakes/credit.rs`.
- Adapters: `rails/watcher.rs`, `pg/credit_tx.rs`, `pg/trade_tx.rs`,
  `pg/deposit_tx.rs`, `pg/rows.rs`, `pg/invariant_read_tx.rs`, `pg/store.rs`,
  `pg/resolve_tx.rs`, HTTP market DTO/routes, and `routes/deposit_admin.rs`.
- Converse: `graph.py`, `core_client.py`, regenerated `api_models.py`, and
  carrier/region tests.
- Contracts: `pg_contract.rs`, `withdraw_contract.rs`, `http_routes.rs`, and
  the isolated `deposit_admin_compile.rs` route compile/handler harness.

## Verification

- Application: 533 passed, 0 failed.
- Adapters all targets: 317 passed, 0 failed, including 36 Pg money contracts,
  18 withdrawal/deposit race contracts, 6 inbound watcher tests, and 3
  deposit-admin route compile/handler tests.
- Converse: 34 passed, 4 intentionally skipped.
- `cargo fmt --all -- --check`: clean.
- `cargo clippy -p application --all-targets -- -D warnings`: clean.
- `cargo clippy -p adapters --all-targets -- -D warnings`: clean, including
  the isolated deposit route compile harness.

Coverage was collected with `cargo llvm-cov -p application --lib --json`
after all 533 tests. Every W3 application addition below has 100% line and
100% function coverage (regions retain Rust's normal short-circuit branch
variance):

| File | Lines | Functions |
| --- | ---: | ---: |
| `cast_vote.rs` | 100% | 100% |
| `credit_deposit.rs` | 100% | 100% |
| `money/credits.rs` | 100% | 100% |
| `money/referrals.rs` | 100% | 100% |
| `ops/config.rs` | 100% | 100% |
| `place_trade.rs` | 100% | 100% |
| `preview_trade.rs` | 100% | 100% |

Targeted adapter coverage also reports 100% line and function coverage for
`routes/deposit_admin.rs` and `rails/watcher.rs` (regions retain the same
short-circuit branch variance).

## Coordinator-mediated changes

W3 did not edit `resolve_market.rs` or `ops/unwind_market.rs`. The coordinator
installed the Paid-only fee-finalization/referral hooks and the Voided-only
allocation reversal hook described in `docs/reviews/w3-coordinator-diffs.md`.
The deposit admin route aggregation/OpenAPI delta in that document remains the
last required coordinator action; OpenAPI and Converse model generation must
run once it is mounted.
