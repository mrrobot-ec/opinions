# P9 engineering audit

## Executive verdict

_Final verdict and verified gate results will be recorded after the shared-tree
integration barrier._

## Method and scope

This audit covered the Rust domain, application, adapters, runtime, migrations,
the `simswarm` library and live harnesses, the Python Converse service, the
Next.js PWA, and the repository's verification gates. Every production defect
listed below was first reproduced by a failing regression test; hypotheses that
could not be made to fail were rejected or recorded only as residual risks.
Relevant prior review dispositions were checked before changing behavior.

No Git or other VCS command was run. Existing files were copied to
`var/backup/<path>` before risky edits, and no gate, lint, assertion, coverage
scope, or threshold was weakened.

## Findings and fixes

### A. Concurrency and race correctness

The audit found four independent race classes.

1. Resolution did not hold one global class-2 user-lock order. Voters and
   referral participants were acquired in two separately sorted runs, which is
   not a globally sorted run; a real two-connection PostgreSQL probe reproduced
   SQLSTATE `40P01`. The retained regressions are
   `resolution_acquires_one_globally_sorted_user_lock_run` and the PostgreSQL
   lock contract. Resolution now forms one deduplicated participant set and
   locks it once, in UUID order.
2. Participant enumeration was still a pre-market-lock snapshot. A trade or
   referral bind could commit after enumeration but before the market lock,
   introducing a payout/collection participant whose class-2 lock was never
   held. The post-lock re-enumeration/retry repair and its deterministic race
   regression are part of the final integration barrier below.
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

The deposit/withdraw pause fences, advisory-lock class/name discipline,
oldest-first lots, settlement lock sorting, outbox `SKIP LOCKED` claim, and the
four withdrawal send windows were also traced. No additional race was proven.

### B. Money and arithmetic

Seven boundary defects were reproduced before repair:

- deposit-time receivable totals and unwind shortfall totals used unchecked
  `i64` sums and panicked in checked builds;
- the 24-hour AML deposit/withdraw velocity totals could overflow and suppress
  a threshold decision in optimized builds;
- the invariant checker itself narrowed legitimate multi-row totals to `i64`;
- the Pg account snapshot cast `sum(bigint)` back to `bigint` before the
  application could use its new `i128` boundary;
- account snapshot rows omitted currency, so equal and opposite USDC and bonus
  credit drift could cancel in the checker;
- resolution payout did not apply the already accepted, oldest-first
  receivable lien in the same transaction; and
- the in-memory receivable fake ordered by random UUID while PostgreSQL ordered
  by `(created_at,id)`, making its purported oldest-first tests incapable of
  proving the production rule.

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
fresh.

The PWA also generated a new withdrawal key after an ambiguous transport
failure. An unchanged retry could therefore create a second hold. The red unit
test observed two keys for one `(user,amount,dest,confirm_dest)` fingerprint.
The client now retains one bounded key across ambiguous failure, rotates it on
a semantic change, and clears it after a definitive success or typed refusal;
a second red test prevented the opposite bug of retaining a successful key and
replaying the first withdrawal forever.

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
semantics.

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

RBAC route capability mapping, DB-backed dual control, sanctions/KYC/geo
`Clear|Hit|Indeterminate`, trusted-proxy CIDRs, device-hash secret handling,
AML band boundaries and the compliance/rail egress split were traced. The
shared demo token and body-supplied user identity remain the explicitly locked
loopback-only Phase-1 posture; timing tests would be flaky and no new testable
bug was invented from that known limitation.

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
  effects. Its 39 new integrity tests execute status helpers, require positive
  controls and real races, and reject unmounted/stubbed money routes; and
- tautological self-comparisons in render tests and adapter middleware were
  replaced with generated-input/genuine capability assertions. A repository
  static regression now rejects self-comparison, `assert!(true)`, Python
  `assert True`, Rust `#[ignore]`, Pytest skips and Playwright skips.

The mutation gate's pass/fail/timeout/empty fixtures remain intact and no
threshold, exclusion or lint was relaxed. The gate-on-the-gate observed 3 LCOV,
4 live-loop, 1 Pg-suite, 5 shell, 39 smoke and 3 static-quality regressions
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
  places and within JS's exact range), exact destination re-entry and correct
  retry identity;
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
   and live end-to-end authorization/effect tests exist.
4. The live Playwright pair requires a seeded/running Core and web fixture. It
   now fails unarmed rather than skipping, but final live execution remains an
   explicit verification item below.
5. Browser terminal vote/redemption state is not fully reconstructible from
   REST after a reload; notifications can lack a market question; displayed
   reputation fees can diverge from a live config override.
6. Converse causal rows retain transitions but not the full rendered prompt,
   model parameters and rationale required for the strongest D18
   reconstruction claim.
7. `alert_outbox` hot predicates are indexed by 0012. Per-user withdrawal-day
   and AML-open-flag predicates still rely on primary-key/table scans; this is a
   scaling risk, not a demonstrated correctness failure at current scale.
8. Three legacy Pg contract fixtures use fixed scratch database names and can
   interfere when the same suite is run concurrently against one server. The
   repository gate serializes DB tests, so no production bug was claimed.

## Final verification

_Actual command output will be recorded here after each required gate has been
personally observed._
