# P9 engineering audit

## Executive verdict

The repository did not merely need polish: the audit reproduced real defects in
resolution lock coverage, webhook and incident atomicity, money overflow and
currency accounting, a missing lazy credit-conversion path on withdrawal,
deposit AML enforcement, browser withdrawal replay, production alert wiring,
WebSocket delivery latency, and several tests that could report success without
exercising their stated contract. Each production repair below has a retained
discriminating regression; several concurrency and database findings were also
independently refuted or confirmed before repair.

The resulting tree preserves the clean-architecture boundary, integer-only
money, fail-closed configuration, and the exact 100% union-coverage gate. The
remaining risks are explicit product/operations work—most importantly an
external pager, chain reconciliation/telemetry composition, and currently
unmounted or stubbed Phase-7 surfaces—not defects hidden behind a green claim.

## Method and scope

This audit covered the Rust domain, application, adapters, runtime, migrations,
the `simswarm` library and live harnesses, the Python Converse service, the
Next.js PWA, and the repository's verification gates. Every production defect
listed below has a retained regression that demonstrably fails against its
pre-fix implementation; hypotheses that could not be made to fail were rejected
or recorded only as residual risks. Relevant prior review dispositions were
checked before changing behavior. The incident-Pg and deposit-AML adapter
implementations had to land during a cross-lane compile break before their
tests could execute; those two tests were therefore mutation-proved afterward,
not misreported as chronological red-first cycles.

No Git or other VCS command was run. Existing files were copied to
`var/backup/<path>` before risky edits, and no gate, lint, assertion, coverage
scope, or threshold was weakened.

## Findings and fixes

### A. Concurrency and race correctness

The audit found five independent race classes and one live delivery-budget
defect.

1. Resolution did not hold one global class-2 user-lock order. Voters and
   referral participants were acquired in two separately sorted runs, which is
   not a globally sorted run; a real two-connection PostgreSQL probe reproduced
   SQLSTATE `40P01`. The retained regressions are
   `resolution_acquires_one_globally_sorted_user_lock_run` and the PostgreSQL
   lock contract. Resolution now forms one deduplicated participant set and
   locks it once, in UUID order.
2. Participant enumeration was still a pre-market-lock snapshot. A referral
   bind could commit after enumeration but before the market lock, introducing
   a grant participant whose class-2 lock was never held. Resolution now
   enumerates before locking, acquires the one sorted/deduplicated participant
   run, locks the market, re-enumerates, and restarts on drift for four bounded
   attempts. `resolution_retries_when_a_referral_participant_binds_while_the_market_lock_is_waiting`
   deterministically late-binds a lower UUID while the resolver is parked;
   `resolution_fails_closed_when_participants_never_stabilize` mutates the set
   on every attempt and proves the bounded invariant error instead of allowing
   an unsafe settlement.
3. `IncidentManager::raise` used `find_open` followed by `insert`, while the Pg
   methods were separate autocommit statements and the schema had no open-key
   uniqueness. Two concurrent raises could open and page the same incident
   twice. A red Pg concurrency test proved this; migration 0012 adds the
   deliberately partial unique index over `incident_key` for `open|acked`, and
   `insert_if_absent` now gives both the memory and Pg stores one atomic
   open-or-get contract. A total unique constraint was deliberately rejected:
   D35 requires a resolved condition to open a new row and re-page on
   recurrence.
4. Converse performed the global delivery-dedupe read before taking its
   per-thread advisory lock. Two real concurrent PostgreSQL deliveries could
   both miss; the losing run then wrote `agent_steps` for a run row that lost
   its uniqueness race, producing a foreign-key violation. The dedupe read now
   shares the turn transaction/lock. Production also uses distinct bounded
   connection pools for the long-lived turn lock and the causal recorder; the
   old single-pool composition deadlocked itself when all eight turns tried to
   record.
5. The new atomic KYC transaction initially still read its inbox key before
   any lock covering that key. Real Pg races proved both consequences: two
   identical deliveries could yield `Accepted` plus raw
   `Store(Conflict("inbox event"))` instead of `Accepted` plus `Replay`, and two
   different payloads naming different users could bypass their unrelated user
   locks and miss the typed, paged payload conflict. The retained test forces
   overlap with a Pg table-lock barrier and `pg_stat_activity`, rather than
   trusting scheduler luck. `serialize_inbox` is now the first class-1
   operation before the read; the fake holds a real per-key guard through
   commit/drop and Pg holds a namespaced transaction advisory lock.

The live swarm then exposed a separate production timing defect. Once the
simswarm client learned the server's actual human-readable timestamp format,
the WebSocket delivery gate honestly failed at p95 104 ms against its fixed
100 ms SLO. The relay polled every 100 ms—the entire SLO—leaving no budget for
the query, commit, broadcast, and socket. The retained
`websocket_relay_reserves_half_the_delivery_budget_for_processing` test failed
against that 100 ms tick; the production tick is now 50 ms. A fresh live run
recorded 497 real delivery samples at p50/p95/p99 33/57/80 ms and passed the
unchanged 100 ms gate.

The deposit/withdraw pause fences, advisory-lock class/name discipline,
oldest-first lots, settlement lock sorting, outbox `SKIP LOCKED` claim, and the
four withdrawal send windows were also traced. No additional race was proven.

### B. Money and arithmetic

Eight boundary defects were reproduced before repair:

- deposit-time receivable totals and unwind shortfall totals used unchecked
  `i64` sums and panicked in checked builds;
- the 24-hour AML deposit/withdraw velocity totals could overflow and suppress
  a threshold decision in optimized builds; the repair carries explicit
  overflow evidence as well as a saturated reporting value, so saturation
  cannot itself hide the threshold breach;
- the invariant checker itself narrowed legitimate multi-row totals to `i64`;
- the Pg account snapshot cast `sum(bigint)` back to `bigint` before the
  application could use its new `i128` boundary;
- account snapshot rows omitted currency, so equal and opposite USDC and bonus
  credit drift could cancel in the checker;
- resolution payout did not apply the already accepted, oldest-first
  receivable lien in the same transaction; and
- the in-memory receivable fake ordered by random UUID while PostgreSQL ordered
  by `(created_at,id)`, making its purported oldest-first tests incapable of
  proving the production rule. The fake now preserves committed chronology and
  applies UUID only as the deterministic timestamp tie-breaker.

The focused red selectors included
`deposit_collection_rejects_an_overflowed_receivable_total`,
`unwind_rejects_a_shortfall_total_larger_than_i64`,
`velocity_totals_saturate_and_flag_when_history_exceeds_i64`,
`aggregate_money_identities_handle_totals_larger_than_i64`,
`account_balance_snapshot_rows_keep_wide_aggregates`,
`invariant_snapshot_rejects_opposite_per_currency_drifts`, and
`payout_collects_the_recipients_open_receivable_in_the_same_transaction`.
They respectively failed by overflow panic, wrong aggregate width/currency, or
an unchanged outstanding receivable before the fixes.

The AMM quadratic, integer square root, pool-favouring buy/sell rounding, fee
split reassembly, per-currency ledger conservation, payout-plus-dust identity,
and the 5,000-bps void outcome were rechecked against boundary/property tests.
No new defect was found in those algorithms. The overflow cases above require
balances near the `i64` limit and are principally denial-of-service hardening,
but they remain real correctness bugs: the request path must return a typed
overflow, never panic or wrap.

### C. Idempotency and crash safety

The KYC webhook violated an already accepted P7R1 requirement. It committed the
provider inbox row in an admin transaction, then opened a second transaction to
append the KYC event, update the tier, and write its decision. A failure between
the commits made every provider retry a same-hash `Replay`; because the inbox
had no applied state and no consumer, the KYC effect was skipped forever while
the endpoint answered success. A fault-injecting fake reproduced the stranded
row before the fix. Inbox algebra and the KYC effect now execute in one
`ComplianceTx`: same key/hash replays without another event, a different hash
conflicts, and any downstream failure rolls the inbox insert back so a retry is
fresh. The inbox key is serialized before the algebra, so normal concurrent
redelivery also converges to one effect plus a replay, while a concurrent
different-hash delivery reaches the typed conflict that the webhook pages on.

The accepted D32 contract requires any transaction that next takes a user's
money lock to convert a ready real-money credit lot, collect its receivables,
and perform the requested mutation atomically. Trade, vote and deposit paths
did so, but `RequestWithdraw` only locked the user and proceeded to its cash
hold. The clean live Phase-7 positive control proved the defect: a 200,000-micro
lot had 200,000 finalized fee allocation after its market reached Paid, yet
`converted_at` remained null after the user's withdrawal lock. The focused Pg
regression
`pg_withdraw_lock_converts_a_ready_credit_lot_before_holding_cash` failed on
that exact assertion before repair. A narrow `LazyCreditConversion` role is now
part of `WithdrawTx`; the Pg withdrawal transaction delegates to the existing
conversion/collection implementation before revalidation and hold, in the same
database transaction. The application replay test also proves the hook runs
once, not again on a matching replay. A clean rerun produced the two conversion
ledger transactions, marked the lot converted and held the withdrawal.

The first union-coverage rerun exposed a related fake/port drift rather than
being waved through: the production conversion role had already collected the
receivable, making the withdrawal port's second collector unreachable, while
the fake implementation of the combined role was still a no-op. Repointing the
existing overflow regression at `LazyCreditConversion` failed against that
no-op. The fake now performs the same checked, oldest-first collection inside
the combined role, the duplicate cash-movement methods were removed, and the
send-time open-receivable projection retains its own Pg contract assertion.

The PWA also generated a new withdrawal key after an ambiguous transport
failure. The server's separate durable intent fingerprint still prevented a
second hold, so this was not a money-loss hole; the defect was that a normal
retry could produce a spurious key/fingerprint conflict or an ambiguous receipt
instead of replaying the same request. The red unit test observed two keys for
one `(user,amount,dest,confirm_dest)` fingerprint. The client now retains one
bounded key across ambiguous failure, rotates it on a semantic change, and
clears it after a definitive success or typed refusal; a second red test
prevented the opposite bug of retaining a successful key and replaying the
first withdrawal forever.

Request-fingerprint comparison, deposit observation identity, outbox replay,
the injected payout crash point, exactly-once settlement movement keys, and all
four withdrawal crash windows were reviewed in the Rust use cases and Pg
contracts. No further failing case survived the existing constraints and
idempotency locks.

### D. SQL and schema

All migrations, live constraints, triggers, `ON CONFLICT` targets, and partial
unique predicates were compared with their callers. The ledger balance
constraint trigger is enabled, deferred, and groups by currency; the append-only
trigger is also enabled. Every inspected conflict target has a corresponding
unique/partial-unique index.

Migration 0011 intentionally introduced
`deposits_observation_identity NOT VALID`. PostgreSQL still enforced it on all
new/updated rows, but no later migration ever recorded that existing rows had
been checked. An isolated 0001..0010 fixture proved that 0011's three
grandfathered shapes satisfy the predicate, that incomplete new/promoted rows
are rejected, and that `VALIDATE CONSTRAINT` succeeds. Migration 0012 performs
that validation and adds the incident open-key and pending-delivery indexes.
The original 0011-only regression remains to pin its historical migration
semantics. Because the shared workspace did not compile between the alert port
change and its Pg adapter, the migration test first executed after 0012 landed;
removing 0012 made it fail, so this is mutation proof rather than a claimed
chronological red-first cycle.

The invariant Pg projection now returns exact `numeric::text` and parses it to
`i128`, and unbalanced transactions are grouped per `(txn_id,currency)`. The Pg
deposit transaction implements the same AML and required-config contracts as
the fake. Remaining index observations are recorded under residual risks; they
were scaling hypotheses, not demonstrated correctness defects.

### E. Security and compliance

The audit reproduced and fixed these fail-closed violations:

- `/webhooks/kyc` looked up the provider session before checking HMAC, giving
  an unauthenticated 403/404 enumeration oracle and opening a DB transaction
  per forged request. Unsigned known and unknown refs are now indistinguishable
  and the store is untouched until signature verification succeeds.
- Converse accepted Sendblue webhooks without provider authentication and
  production could start with neither its webhook secret nor the Core token.
  Startup now validates both before allocating DB resources, and the signing
  secret comparison uses `compare_digest`.
- a malformed `REGION_ALLOWSET_VERSION` silently reverted to version 1. It is
  now a bounded, fail-loud startup value, so stale `Clear` geo verdicts cannot
  survive a malformed deployment variable once the counsel allowset is live.
- deposit admission credited before evaluating the pinned AML band/velocity
  policy. It now evaluates once under the same unit of work, holds on an open
  flag, and does not double-count replay or held reevaluation.
- required money catalog controls silently fell back to seed values. Missing
  deposit KYC/pause values, bonus issuance cap, referral minimum/amounts and
  shadow trade/deposit caps now fail closed; the intentionally optional
  `feature_referrals=false` behavior remains optional.
- the shared demo token used short-circuiting string equality in three HTTP
  helpers and the WebSocket user subscription. Those comparisons now hash to
  fixed-width digests and use the existing accumulator comparison; a
  mutation-proved source gate prevents the direct equality shapes from
  returning.

RBAC route capability mapping, DB-backed dual control, sanctions/KYC/geo
`Clear|Hit|Indeterminate`, trusted-proxy CIDRs, device-hash secret handling,
AML band boundaries and the compliance/rail egress split were traced. The
body-supplied user identity behind the shared token remains the explicitly
locked loopback-only Phase-1 posture; this audit repaired the comparison but
did not pretend that a constant-time shared development token is production
identity.

### F. Error handling and fail-closed behavior

The money overflow repairs above replace panics/wrap with typed failure. Pg test
setup now fails loudly rather than converting connection/migration errors into
successful empty tests. Incident redelivery attempts the rest of the batch
after a poison item but surfaces the first pager/store failure after the sweep;
continuation is not error swallowing. The KYC, Sendblue, Core-token, geo and
catalog repairs all turn malformed/missing security inputs into startup or
request failure.

The audit searched production request paths for `unwrap`, `expect`, `panic!`,
`todo!`, `unimplemented!`, discarded results and retry loops. Workspace lints
and the dependency boundary still deny reachable panic helpers and floats in
the Rust money code. Suspected watcher swallowing was rejected after reading
the full expression: `admit_observed(...).await?` propagates the error; `let _`
discards only its successful return value.

### G. Test quality and verification honesty

This was the largest systemic finding. The following gates could report green
without observing the property named by the gate, and each now has a retained
regression in `just gate-test`:

- an empty LCOV file satisfied the 100% union gate because it contained zero
  uncovered lines; it is now rejected as "no executable lines";
- the live-loop script fabricated/accepted lifecycle latency without a
  `resolved` frame and treated a missing tally counter as zero;
- nine Rust PostgreSQL suites returned early on a missing URL, refused
  connection, failed database creation, or failed migration: 75 tests printed
  `passed` in roughly 0.01 seconds while executing no SQL. Their shared helper
  now panics loudly, all early-return guards were removed, and a source-level
  regression prevents their return;
- three Converse Pg tests and two Playwright files used environment skips.
  They now fail with explicit arming requirements; the Pg cases run against the
  audit database and Playwright discovery finds two real tests;
- economy/social E2E scripts masked failed SQL fixture writes, a fee assertion,
  individual young-brigade rejections, mention creation, and pagination setup;
- the Phase-7 swarm smoke accepted 404/503, discarded HTTP statuses, omitted
  required withdrawal fields, and used SQL self-assertions instead of testing
  effects. Its 41 integrity tests execute status helpers, require positive
  controls and real races, and reject unmounted/stubbed money routes; and
- tautological self-comparisons in render tests and adapter middleware were
  replaced with generated-input/genuine capability assertions. A repository
  static regression now rejects self-comparison, `assert!(true)`, Python
  `assert True`, Rust `#[ignore]`, Pytest skips and Playwright skips.

The final live runs caught six additional verification defects rather than
laundering them into noise. First, the subscriber could start before its
`near-close` target existed and nondeterministically parse a 404; the full
target set is now published before the actor tasks start. Second, the mobile
money test's `Destination` locator also matched `Re-enter destination`; its
trace-proved strict-selector failure is fixed with the exact label, and the
live pair now executes two tests rather than merely discovering them. Third,
simswarm accepted only RFC3339 while the Rust server actually serializes
`time`'s human-readable `YYYY-MM-DD HH:MM:SS.fraction +HH:MM:SS` shape. That
made the WebSocket series silently empty. The parser now accepts both actual
wire forms, rejects malformed/backdated frames, and the positive control
records the expected latency; this honest series is what exposed the production
relay SLO failure described in A. Finally, the six declared proof books
reserved $1.2k while the fixture configured a $1.0k daily publish budget, so
the Phase-7 red-team was never reached. The staging-only capacity now exactly
matches the declared workload; the production budget threshold and its own
rejection tests were not changed. On the next run every withdrawal stopped at
`geo_missing_ip`: because the local peer is trusted, the helper needed an
untrusted forwarding hop before warmth, limits, AML or self-exclusion could be
exercised. The client-IP smoke-integrity test failed first, then the helper
was pinned to the TEST-NET-3 client address `203.0.113.9` and passed.
Finally, the grant positive control inserted only a `credit_grant_lots` metadata
row: it gave the user no `usdc_credit` ledger balance and provided no
`BonusReserve` cash backing. That meant a repaired conversion path would fail
on an impossible fixture rather than prove production behavior. The 41st
smoke-integrity test failed against the metadata-only seed; the fixture now
creates both currencies' accounts and balanced `seed`/`credit_grant` ledger
transactions in one transaction before inserting the lot. An isolated fresh
migration run proved per-currency balance and reserve coverage before the full
live rerun.

The mutation gate's pass/fail/timeout/empty fixtures remain intact and no
threshold, exclusion or lint was relaxed. The gate-on-the-gate observed 3 LCOV,
4 live-loop, 1 Pg-suite, 5 shell, 41 smoke and 3 static-quality regressions
green after the repairs.

### H. Dead code, stubs, and interface truthfulness

The audit compared registered routes, Rust DTOs, `openapi.json`, the generated
Python models and the browser clients. It found real drift in comment-vote
idempotency, notification/profile shapes, withdrawal fields/refusals and
self-exclusion routes; those client contracts are fixed and the Python model
generator now proves all 69 `BaseModel` schemas and six enums equivalent.

The Phase-7 production surface still contains implemented-but-unmounted phone,
KYC webhook and compliance routers, plus six genuinely unimplemented admin
commands represented by 503 stubs (credit grant, bonus-reserve top-up and
market fee override propose/confirm). This is not silently called complete in
this audit: composition/OpenAPI disposition is recorded as a residual because
mounting only part of a dual-control money surface without the required live
E2E authority would be feature work, not a contained defect repair.

OpenAPI was regenerated from the mounted router rather than hand-edited: it
truthfully describes the live deposit/withdraw corridor but does not advertise
the unmounted phone/KYC/compliance routers or pretend the six 503 commands are
implemented. The regenerated Python models remain source-equivalent to that
published contract.

No reachable `todo!` or `unimplemented!` was found in the audited production
paths. Stale observability comments and fabricated simswarm evidence were
corrected as part of J and the core fixes.

### I. Converse and web money corridor

The agent-free corridor held up. The lexical pre-router runs before the model;
only the explicit confirmation allowlist may execute a stored Core preview.
The graph carries Core's authoritative price/fee/share/config values and does
not calculate them; stale config expires and re-previews, requiring a new
lexical confirmation; the LLM/router has no execution tool. Malicious-router,
stale-confirmation and replay tests remain green.

The owned service/web repairs were:

- separate turn/recorder Pg pools and locked dedupe (A);
- mandatory Sendblue/Core secrets (E);
- comment-vote idempotency, notification/profile normalization and regenerated
  Python DTOs;
- no-cache handling for same-origin `/core-api`, with a cache namespace bump;
- exact integer withdrawal parsing (canonical positive decimal, at most six
  places and within JS's exact range), exact destination re-entry and stable
  retry identity across ambiguous transport failure;
- share-aware sell controls, micro-share formatting without a dollar sign,
  and honest "settled P&L"/refusal copy; and
- direct dependency upgrades that removed the audited Next/Playwright and
  `datamodel-code-generator` advisories without override tricks.

Focused failures included a real Pg foreign-key violation under concurrent
delivery, a one-pool-vs-required-two startup assertion, five then five browser
DTO/unit failures, four withdrawal safety failures, and dependency auditors
reporting six JS plus nine Python advisories. Final owned results are recorded
in the verification section.

### J. Observability honesty

The known gap was confirmed: production continuously ran the invariant sweep
but only `eprintln!`ed a violation; the only `IncidentManager::raise` calls were
tests, and the wired `SharedAlerter` is an in-process recording vector rather
than an external pager.

The contained part is now repaired. `sync_invariant_report` maps each failed
identity to stable `invariant_breach:<identity>:active`, dedups an ongoing
breach, resolves on recovery and pages again on recurrence. The continuous
runtime calls it, and Pg contracts cover the durable lifecycle. The incident
outbox itself is now atomic and at-least-once: a new incident is persisted with
zero attempts/no page timestamp; a successful page records the supplied real
time; a failed page stays pending; Acked-but-undelivered remains pending; and a
poison row cannot starve later rows while still making the pump return an
error.

This does not pretend to solve the external integration. The production
alerter remains a recorder because Phase 7 explicitly left the vendor backend
open. Chain balance/reconciliation and structured OpenTelemetry are likewise
not composed. Those are real launch risks, but selecting a pager, custody
reader or telemetry backend is external product/operations scope and could not
be safely invented in this audit.

### Supply-chain and dependency integrity

The repository's Rust audit tools were absent locally at the start; after
installing them, the real gates were red rather than assumed green.
`cargo audit` found RUSTSEC-2023-0071 in `rsa 0.9.10` through the old SQLx
graph, and `cargo deny` found the unmaintained `paste` crate through an unused
`utoipa-axum` dependency plus unclassified Zlib, ISC and
CDLA-Permissive-2.0 licenses. The unused OpenAPI wrapper was removed and SQLx
was upgraded from 0.8.6 to 0.9.0, removing both vulnerable/unmaintained paths.

SQLx 0.9 makes dynamic SQL trust explicit. Every new `AssertSqlSafe` boundary
was reviewed: production strings are closed constant fragments selected inside
the adapter, while test strings interpolate only internally generated scratch
database identifiers. No request/user data was converted into executable SQL.
The three license additions are exact SPDX permissions with dependency reasons;
no advisory ignore, crate exclusion, source relaxation, or ban waiver was
added. The final audit and deny results are recorded below.

## Rejected hypotheses and deliberate non-changes

- AMM/isqrt, pool conservation, dust and neutral void settlement survived the
  boundary/property review; no test failed, so no algorithm churn was made.
- A holder-only resolution participant was initially suspected, but normal
  trading requires a vote before position creation. The broader post-lock
  enumeration race remained valid for voters/referrals and was treated there.
- The settlement `zip` misalignment theory was refuted by the ordering and
  construction invariants.
- `deposits_observation_identity NOT VALID` was not treated as "unenforced";
  PostgreSQL enforces it on new writes. The later validation closes schema
  proof/drift, not a new-write bypass.
- `withdrawals.request_fingerprint` itself need not be unique: the dedicated
  `request_fingerprints` primary key plus class-1 serialization is the
  authoritative idempotency guard.
- Outbox broadcast-before-mark is documented at-least-once behavior, not an
  exactly-once promise; consumers deduplicate by durable sequence.
- Display-only Python `_fmt_usd` floating point produced no supported-range
  failure and was left unchanged. Monetary decisions remain integer-only.
- No raw HTML sink or cookie-CSRF request path was found in the PWA; React text
  escaping and explicit header tokens are the active boundaries.
- `let _ = admit_observed(...).await?` does not swallow admission failure.
- The development demo-token posture, external pager vendor, counsel geo
  allowset, onramp, Apple Messages for Business and mainnet custody are locked
  external gates and were not relitigated.

## Residual risks and owner decisions

1. The durable incident system now receives invariant failures, but
   `SharedAlerter` reaches no human and grows in process. A production pager
   adapter and operational ownership remain launch blockers.
2. Chain reconciliation, reserve coverage and structured telemetry ports are
   present but not composed; only the invariant detector received a contained
   end-to-end repair.
3. Several Phase-7 routers are unmounted and six dual-control admin commands
   have no handler. OpenAPI cannot honestly advertise them until composition
   and live end-to-end authorization/effect tests exist. The live Phase-7
   red-team therefore exits nonzero with named missing prerequisites instead of
   certifying a 404/503 as an attack refusal.
4. Browser terminal vote/redemption state is not fully reconstructible from
   REST after a reload; notifications can lack a market question; displayed
   reputation fees can diverge from a live config override.
5. Converse causal rows retain transitions but not the full rendered prompt,
   model parameters and rationale required for the strongest D18
   reconstruction claim.
6. `alert_outbox` hot predicates are indexed by 0012. Per-user withdrawal-day
   and AML-open-flag predicates still rely on primary-key/table scans; this is a
   scaling risk, not a demonstrated correctness failure at current scale.
7. Three legacy Pg contract fixtures use fixed scratch database names and can
   interfere when the same suite is run concurrently against one server. The
   repository gate serializes DB tests, so no production bug was claimed.
8. Resolution now retries participant drift four times and fails closed if the
   set never stabilizes. That protects money correctness, but sustained referral
   churn can deliberately force a resolution retry/exhaustion and require the
   scheduler or an operator to try again.
9. WebSocket latency evidence rejects a server timestamp later than the local
   receipt clock. That is honest—negative latency is not evidence—but material
   host clock skew can reduce the sample count, so production hosts still need
   normal time synchronization and sample-count monitoring.

## Final verification

All repository gates below were rerun against the final tree on 2026-08-16,
with one external Cargo target directory. The full outputs are retained under
`artifacts/p9-final-*.log`.

```text
$ just fmt
cargo fmt --all --check

$ just clippy
cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.92s

$ just test
RUST_TEST_THREADS=1 cargo test --workspace
# cargo test --workspace -- --list | rg ': test$' | wc -l
1114
# every test binary and all four doc-test suites ended with 0 failed / 0 ignored
```

The required LCOV union gate—not the llvm-cov folded summary—reported:

```text
$ just coverage
domain: 100% lines (1938 lines, 0 uncovered)
application: 100% lines (40424 lines, 0 uncovered)
adapters: 100% lines (14382 lines, 0 uncovered)
simswarm: 100% lines (3616 lines, 0 uncovered)
```

Supply-chain and architecture gates were green without an ignore or waiver:

```text
$ just deny
advisories ok, bans ok, licenses ok, sources ok

$ just audit
Loaded 1216 security advisories
Scanning Cargo.lock for vulnerabilities (265 crate dependencies)
# exit 0; no vulnerability finding

$ just deps-check
dependency rule OK (5 internal crates)
application type-leakage check OK
```

The gate-on-the-gate continued to reject its bad fixtures and execute every
integrity suite:

```text
$ just gate-test
mutation kill rate: 92.3% (caught=12, missed=1)
mutation kill rate: 80.0% (caught=8, missed=2)       # rejected fixture
non-gateable outcomes present: {'Timeout': 1}        # rejected fixture
invalid or empty outcomes.json — refusing to gate    # rejected fixture
Ran 3 LCOV tests ... OK
Ran 4 live-loop tests ... OK
Ran 1 Pg-suite test ... OK
Ran 5 shell tests ... OK
Ran 41 smoke tests ... OK
Ran 3 static-quality tests ... OK
```

The less-reviewed owned stacks also ran on their real arming requirements:

```text
$ cd services/converse && DATABASE_URL=<fresh migrated scratch DB> .venv/bin/pytest
collected 43 items
======================== 43 passed, 1 warning in 0.59s =========================

$ pnpm --dir web test
Test Files  13 passed (13)
Tests  80 passed (80)
$ pnpm --dir web exec tsc --noEmit
# exit 0
$ pnpm --dir web build
Compiled successfully
Generating static pages ... (12/12)
$ pnpm --dir web audit --prod
No known vulnerabilities found

$ just docs-build
Documentation built in 0.42 seconds
```

Finally, the clean live artifact is
`artifacts/phase7-w4-p7w4-1786852056-82962`. Its unchanged SLO gate passed with
481 trade confirmations (p50/p95/p99 6/20/74 ms) and 497 WebSocket deliveries
(33/57/80 ms); the live Playwright pair reported `2 passed (6.0m)`. The
post-Paid credit lot finalized and converted through the repaired withdrawal
lock, and every concrete Phase-7 withdrawal, deposit, AML, concurrency and
crash leg reached its assertion. The script then exited nonzero exactly at its
fail-closed certification gate, quoting eight missing production prerequisites:

```text
PHASE 7 RED-TEAM INCOMPLETE — 8 missing production prerequisites:
  credit grant issuance; referral bind surface; market fee_override;
  KYC provider webhook; inbound chain watcher; sanctions Hit fixture;
  self-exclusion surface; reconciliation detector
PHASE 6 E2E FAILED: the Phase 7 red-team cannot certify: the prerequisites
above are unbuilt, so those attacks were never executed
```

That final red is deliberate and unresolved; it is not one of the required
repository gates above and was not relabeled as a pass.

## Residual risk #4 follow-up — vote reload state fixed

The vote half of residual risk #4 was repaired on 2026-08-16. The browser now
rehydrates the authenticated local-demo viewer's boolean vote state from
`GET /markets/{id_or_slug}/my-vote`, so a refresh no longer relocks trading.
A dedicated viewer-scoped read was chosen instead of adding private state to
the public market summary and shared WebSocket snapshot: it composes with the
existing page load, survives later public snapshot merges, accepts no target
user, and returns only `has_voted`—never the live vote side, crowd guess, or
sequence. It also avoids the bounded `recent_votes` profile history entirely.

Residual risk #4 remains open for redemption-state reconstruction,
notifications that lack a market question, and displayed reputation fees that
can diverge from a live config override; those items were not changed here.
