# P9 — PostgreSQL adapters / runtime / security / observability audit

Worker: P9-DB (`task_731871fa0448`). Worktree: `/Users/mrrobot/Documents/personal/opinions`.
Ownership exercised: `crates/adapters/**`, `crates/main/**`, `migrations/**`.
**No VCS command of any kind was run at any point.**

Scratch database: `postgres://opinions:opinions@localhost:15434/opinions_p9_038a_db`.
Build dir: `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-db` (removed at hand-off).

---

## 0. TDD chronology — read this before the findings

Coordinator asked for this explicitly, so it is first rather than buried.

**Genuine chronological red-before-fix** (test written, run, observed failing, *then* the
production change):

| Finding | Red observed before the fix existed |
|---|---|
| P9-2 KYC pre-auth oracle | yes |
| P9-3 silent Pg suite skips | yes |
| P9-4 `REGION_ALLOWSET_VERSION` | yes |
| P9-12 middleware tautology | yes — the failing gate was `scripts/test_test_quality.py`, which I ran red before touching `middleware.rs` |
| P9-13 KYC two-transaction atomicity | yes |
| P9-14 concurrent identical KYC deliveries | yes |
| P9-15 concurrent same-key/different-payload deliveries | yes |
| P9-11 constant-time secret comparison | yes — the structural gate went red against my own source on first run (my explanatory comment quoted the forbidden literal) |
| P9-17 concurrent `raise` duplicate incident/page | no chronological red (the fix landed under a build-red emergency), but **mutation-proved**: see §3 |

**NOT chronological red-first — stated plainly rather than dressed up:**

- **`migration_all_invariants.rs` / migration `0012`.** The test file was authored before
  `0012` existed, but it never *ran* in that state: the workspace was broken at that moment by
  another lane's `AlertStore::insert_if_absent` addition, and unblocking that build required
  writing `PgAlertStore::insert_if_absent`, which is only correct against the `0012` index — so
  `0012` was created before the test could execute even once. The RED transcript in §3 was
  therefore produced **afterwards**, by temporarily removing `0012` and re-running. That is a
  post-fix mutation check, not a chronological red. It is strong evidence that the test
  discriminates, and weak evidence about the order I worked in. Recorded as the latter.
- **`impl DepositAmlIo for PgTx`** landed under a build-red emergency, so its Pg contract test
  came after. It now exists (`tests/deposit_aml_contract.rs`) and is **mutation-proved** rather
  than presented as a red-first cycle — see §4.
- **The per-currency detector suite** (`tests/invariant_currency_detector.rs`) is adapter
  parity evidence for a finding I had already reported as *refuted-as-reachable* and the lead
  adjudicated as a contract question. It plants state the DB trigger prevents, so it is
  defence-in-depth evidence, not a chronological red through a normal path. Also
  mutation-proved — see §6.
- **`insert_if_absent` / the concurrent-`raise` regression.** `PgAlertStore::insert_if_absent`
  had to be written to unblock a broken workspace, so its end-to-end test came after. Instead
  of implying a red I did not observe, I **mutation-proved** the test: reverting
  `insert_if_absent` to the pre-fix read-then-insert makes it fail (transcript in §3), then the
  mutation was removed. That demonstrates the test discriminates; it does not claim red-first.

---

## 1. Verdict summary

| # | Dim | Finding | Status |
|---|---|---|---|
| P9-1 | J/F | Continuous invariant sweep never raised a D35 incident; **no production path called `raise`**; wired `Alerter` is an in-memory recorder | **FIXED** (wiring) + residual |
| P9-2 | E | `/webhooks/kyc` resolved the user (opening a DB transaction) **before** verifying the HMAC — 404-vs-403 enumeration oracle | **FIXED** red→green |
| P9-3 | G | **Nine Pg suites could skip themselves**: 75 tests `ok` in 0.01s with no `DATABASE_URL`, zero SQL executed | **FIXED** red→green + gate |
| P9-4 | E | `REGION_ALLOWSET_VERSION` silently fell back to the seed on a malformed value | **FIXED** red→green |
| P9-5 | A | ResolveMarket took class-2 locks in **two** sorted runs → deadlock | **CONFIRMED** (live 40P01); fixed by core; regression mine |
| P9-6 | A/C | Unbound qualifying referee could bind between enumeration and grant | **CONFIRMED**, gated by `feature_referrals` |
| P9-7 | D | `deposits_observation_identity` `NOT VALID` | **REJECTED as defect**; validated in 0012 on lead's call |
| P9-8 | A/D | Invariant sweep currency-blind | **REJECTED as reachable**; hardened anyway on lead's call |
| P9-9 | H | Phase-7 routers unmounted; 18 bare-503 admin stubs | Documented residual |
| P9-10 | J | `Telemetry`/`ChainBalance` never composed; `chain_reconcile` has zero callers | Reported |
| P9-11 | E | Demo token compared with `==`; WS `SubscribeUser` likewise | **FIXED** + structural gate |
| P9-12 | G | `assert_eq!(Capability::Faucet, Capability::Faucet)` | **FIXED** against a real failing gate |
| P9-13 | C/F | KYC inbox acceptance and effect committed in **two** transactions | **FIXED** red→green |
| P9-14 | A/C | Concurrent identical KYC deliveries → `Conflict`, not `Replay` | **FIXED** red→green (`serialize_inbox`) |
| P9-15 | A/E | Concurrent same-key/different-payload → raw `Conflict`, bypassing the page | **FIXED** red→green |
| P9-16 | C | KYC `to_tier`/`policy_version` fail-open (missing ⇒ **full KYC**) | **FIXED** red→green |
| P9-17 | A | D35 incident dedup was advisory, not structural | **FIXED** (partial unique index) |
| P9-18 | C | `pending_delivery` excluded `acked`, so an ack retired an undelivered incident | **FIXED** |
| P9-19 | E | `PgTx` lacked `DepositAmlIo`; deposit admission had no AML evaluation | **IMPLEMENTED** + Pg contract, mutation-proved |
| P9-20 | E | `CreditIo::config_flag` mapped absent/malformed policy to `false` | **FIXED** (`required_config_flag`) |

Refuted: `rails/watcher.rs` "swallowed error" (the `?` propagates — coordinator withdrew it);
`reps_for_update`'s `ORDER BY` being a no-op for lock ordering (disproved by `EXPLAIN`).

---

## 2. Files changed

**Production**

| File | Change |
|---|---|
| `crates/adapters/src/http/routes/kyc_webhook.rs` | P9-2 verify-before-resolve; P9-13 single transaction; P9-16 `to_tier`/`policy_version` validation |
| `crates/adapters/src/inbox.rs` | `ingest` now drives the atomic `accept_and_apply_inboxed_kyc`; new `KycEffect` |
| `crates/adapters/src/pg/compliance_tx.rs` | `inbox_get`/`inbox_insert` **moved** to `ComplianceTx`; new `serialize_inbox` |
| `crates/adapters/src/pg/alert_tx.rs` | `insert_if_absent`; `pending_delivery` includes `acked` |
| `crates/adapters/src/pg/credit_tx.rs` | `impl DepositAmlIo`; `required_config_flag`; `aml_policy` |
| `crates/adapters/src/pg/withdraw_tx.rs` | `config_i64` exported to siblings (no duplicated parser) |
| `crates/adapters/src/pg/invariant_read_tx.rs` | per-currency `account_balances` + `unbalanced_txns`; `numeric`→`i128` |
| `crates/adapters/src/http/middleware.rs` | `secret_eq`; P9-12 tautology replaced |
| `crates/adapters/src/http/routes/{mod,phone,compliance_admin}.rs`, `src/http/ws.rs` | constant-time secret comparison |
| `crates/main/src/main.rs` | `region_allowset_version`; `deliver_pending(now)`; `sync_invariant_report` wiring |
| `migrations/0012_alert_dedup_and_validation.sql` | **NEW** |

**Tests (all new unless noted)**

`tests/common/mod.rs`, `tests/pg_suites_cannot_skip.rs`,
`tests/migration_0011_deposit_identity.rs`, `tests/migration_all_invariants.rs`,
`tests/resolve_lock_order.rs`, `tests/kyc_inbox_race.rs`,
`tests/secrets_compared_in_constant_time.rs`, `tests/deposit_aml_contract.rs`,
`tests/invariant_currency_detector.rs`; plus P9-3 conversions of the nine pre-existing Pg
suites and `tests/alert_contract.rs`.

Backups (there is no VCS) before every edit: `var/backup/<same-relative-path>`.

**No gate weakened.** No coverage exclusion, no test deleted or `#[ignore]`d, no threshold
lowered, and — verified by scan — **zero `#[allow]` / `#![allow]` attributes in any file I
created**. Three clippy findings on my new files were fixed properly (backticks, splitting an
over-long test, typing a constant `i64`), never suppressed.

---

## 3. Bugs fixed, with exact red and green

### P9-2 — `/webhooks/kyc` authenticated after it had already touched the database

RED:
```
an_unsigned_webhook_is_refused_before_any_store_access ... FAILED
  left: 503   right: 403
an_unsigned_webhook_cannot_distinguish_a_known_provider_ref ... FAILED
  left:  (403, {"code":"AdminForbidden","message":"... invalid webhook signature"})
  right: (404, {"code":"NotFound","message":"not found: provider mapping"})
```
The 503 is the probe: a `NoStoreAccess` double fails every `admin_tx()`, so reaching it proves
the handler hit the store pre-auth. FIX: verify the HMAC (with `ingest`'s empty-secret guard
mirrored) before resolving, placed after the pure-CPU 422 field checks. GREEN: 4 passed.

### P9-3 — nine Postgres suites could silently skip themselves

RED (`env -u DATABASE_URL`): **75 tests reported passing in 0.01s having executed no SQL** —
`alert_contract` 3, `compliance_contract` 5, `credit_coverage` 1, `migration_0006` 2,
`migration_0007` 1, `migration_0011_deposit_identity` 1, `pg_contract` 36, `w2_ops_contract` 8,
`withdraw_contract` 18. Not only an unset variable: `justfile:2` exports a default, but the
`.ok()?` chains meant a **down database** produced the same silent green under `just test`.

FIX is structural, not per-call-site: `tests/common/mod.rs` is the one reader and it **panics** —
no `Option` arm exists, so no caller can be written to skip. Helpers return concrete types,
every `.ok()?` became `.expect(...)`, all 40 guards removed.

GREEN, same command:
```
DATABASE_URL is required by this PostgreSQL suite and is unset or blank. … This suite refuses
to skip: a Pg suite that silently passes without a database is verification theatre.
test result: FAILED. 0 passed; 3 failed
```

GATE: `tests/pg_suites_cannot_skip.rs` re-reads every suite via `include_str!` and rejects a
direct `std::env::var("DATABASE_URL")`, a helper yielding `None`/`Ok(None)`, an
`else { return }` guard, or a `.ok()?` setup step. **Mutation-checked**: appending a probe
line containing `.ok()?` to `alert_contract.rs` gave `FAILED. 1 passed; 2 failed`; probe
removed. A third test proves the matcher on synthetic sources and that a compliant suite
false-positives on none of them.

The lead's independent `scripts/test_pg_contract_suites.py` now reports `Ran 1 test … OK`.
The two gates are complementary: theirs globs `tests/*.rs` (missing `tests/common/`) and does
not match the one-line `else { return }` or bare `.ok()?`.

### P9-4 — `REGION_ALLOWSET_VERSION` failed open

RED: `malformed "v7" must fail startup, not silently become the seed` — `0 passed; 1 failed`.
FIX: `env_value(..)?` plus the D24 bound (`>= 1`). GREEN: `4 passed`.
Impact stated precisely: the version stamps `Allowset::version`, which is what makes a cached
`Clear` stale when counsel changes the allowset — falling back to 1 after a bump to 7 lets
verdicts from a superseded allowset keep passing. Latent today because
`SandboxCompliance::from_env` is called with `region_allowset = None` (deny-all).
Behaviour change named: `REGION_ALLOWSET_VERSION=""` is now a startup error.

### P9-13 / P9-14 / P9-15 / P9-16 — the KYC webhook

**P9-13, two transactions.** `accept_inbox` committed the inbox row in its own `admin_tx`;
`apply_inboxed_kyc` then opened a second `compliance_tx`. A crash or a failure to open the
second left the delivery durably "seen" with nothing applied, and because `inbox_algebra` has
no notion of "applied", every retry returned `Replay`: the tier is never set and the provider
is told **200**. `docs/reviews/codex-p7r1.md:75` accepted same-transaction coupling; it never
shipped. FIX (application lane provided `accept_and_apply_inboxed_kyc`; I rewired
`inbox.rs`/the route and dropped the second transaction). Regression: a store double whose
*effect* transaction fails once — first delivery must error and change nothing, the retry must
be **`accepted`** and apply the tier.

**P9-14, concurrent identical deliveries.** RED:
```
delivery b failed; an identical duplicate must be a Replay, never an error the provider has
to retry: Store(Conflict("inbox event"))
```

**P9-15, concurrent same key / different payloads / different users.** This refutes "the user
lock serializes it" — different `users` rows share no lock. RED:
```
the loser must reach the typed algebra verdict the webhook pages on, not a raw primary-key
collision: got Store(Conflict("inbox event"))
```
Why the typed-vs-raw distinction is not pedantry: `ingest` pages **only** on
`ProposalConflict("inbox payload hash conflict")`. Two deliveries claiming one event id with
divergent payloads is precisely the tampering-or-provider-bug signal D33 wants an operator
woken for, and that page was being skipped.

**The flakiness finding, which matters more than either race.** P9-15 *passed* on its first
run. Rather than accept it I measured: **PASS=12 FAIL=3 over 15 runs**. There was no
serialization mechanism — it passed ~80% of the time because the two transactions usually did
not overlap the `inbox_get` → `inbox_insert` window. A single-pair gate would have been green
four runs in five over a live bug, and its green would have been attributed to whatever landed
next. On the lead's direction the harness now **forces** the overlap: a control transaction
holds `LOCK TABLE compliance_decisions IN SHARE MODE` (conflicts with the INSERT's ROW
EXCLUSIVE; leaves `SELECT`, `users` row locks and advisory locks free), both deliveries park at
the write, and the test polls `pg_stat_activity` for two lock-waiting backends — read from
Postgres, never guessed from a sleep — before releasing. It fails loudly if the barrier is
never reached, rather than degrading into the coin flip it replaced.

FIX: `ComplianceTx::serialize_inbox` (application lane's trait) implemented on `PgComplianceTx`
as `pg_advisory_xact_lock(1, hashtext('kyc-inbox:{provider}:{event_id}'))` — class 1 is the
existing idempotency namespace and the key is disjoint, so it composes with the global
class-1 → class-2 order instead of adding a lock class.
GREEN, and **deterministic**: 8 consecutive runs, `PASS=8 FAIL=0`.

**P9-16, malformed effect defaulted to full KYC.** A missing / non-integer / out-of-range
`to_tier` defaulted to **2 — full KYC**, the tier gating deposits and withdrawals, on a payload
that never asked for it; `policy_version` defaulted to `"1"`. FIX: validated *before*
ingestion so a malformed authenticated delivery is never durably accepted — `to_tier` an
integer in `0..=2`, `policy_version` a non-blank string, else 422. Regression covers absent /
string / 3 / -1 / beyond-`i32` and absent / blank / non-string, each asserting the tier is
unchanged, plus a positive control. The pre-existing happy-path fixture gained
`"policy_version"`: a stricter contract, not a weakened test.

### P9-11 — bearer secrets compared in variable time

`middleware.rs` already hashed and compared 32 fixed bytes for admin tokens; the demo token —
which gates trades, votes, withdrawal requests, self-exclusion, phone verification and the WS
frame that hands out another user's private notification stream — used `==`. A timing
assertion would be flaky and prove nothing on a loaded box, so the fix ships with a
**structural** gate instead: `secret_eq` in `middleware.rs`, all four call sites rewired
(`routes/mod.rs`, `routes/phone.rs`, `routes/compliance_admin.rs`, `ws.rs`), and
`tests/secrets_compared_in_constant_time.rs` rejecting the direct shapes, asserting each module
routes through the helper, asserting `secret_eq` is built on the fixed-width accumulator, and
mutation-proving the matcher including a no-false-positive check. It fired immediately on my
own source because an explanatory comment quoted the forbidden literal; I reworded the comment
rather than loosening the matcher.

### P9-1 / P9-17 / P9-18 — the alert path

`main.rs`'s `--continuous` loop only `eprintln!`ed a violation, and the **only** `.raise(` call
sites in the repository were tests. `alert_outbox` was never written in production, so the
every-60s `deliver_pending` pump returned 0 forever: outbox, dedup, ack/resolve and
at-least-once redelivery were all built, all tested, all dead. Against `docs/spec.md:382` and
D35.

FIXED: the loop now calls `manager.sync_invariant_report(&report, SystemClock.now())` on every
`Ok(report)` — failing identities raise and dedup, passing ones resolve or no-op, so a
recurrence after recovery re-pages. `main.rs` stays wiring; the policy is in the application
lane's function.

**P9-17**: dedup was a read-then-insert across two autocommit connections — two raisers both
see "absent" and page twice. Now structural: `alert_outbox_one_open_per_key`, a **partial**
unique index over `('open','acked')`. Partial is load-bearing: a resolved episode is kept as
its own row so a recurrence re-pages, and a total `unique (incident_key)` would forbid exactly
that (and break `alert_contract.rs`'s `count == 2`).

**P9-18**: `pending_delivery` was `status = 'open'`, so acking an undelivered incident retired
it from the at-least-once queue — the aliasing `last_paged_at` exists to prevent. Now
`status in ('open','acked')`.

**End-to-end proof, not just DDL.** `alert_contract.rs` gained
`two_concurrent_raises_open_one_incident_and_page_once`: a control transaction holds
`LOCK TABLE alert_outbox IN SHARE MODE` so both raises get past their reads and park at the
write, the test polls `pg_stat_activity` for two lock-waiting backends before releasing, and
then asserts the observable contract — both callers hold the SAME incident id, exactly one live
row for the key, and exactly **one** page through a single shared counting alerter (counting
per-manager would have hidden a duplicate page). It exercises
`raise` → `insert_if_absent` → the partial index → the loser's re-read.

MUTATION CHECK, since this one had no chronological red: reverting `insert_if_absent` to the
pre-fix read-then-insert gives
```
second raise: Integrity("error returned from database: duplicate key value violates unique
constraint \"alert_outbox_one_open_per_key\"")
test result: FAILED. 0 passed; 1 failed
```
— with 0012's index in place the old code fails outright rather than opening two incidents;
without the index it would have opened two rows and paged twice. Either way the test
discriminates. The mutation was removed and the suite is deterministic: 6 consecutive runs,
`PASS=6 FAIL=0`.

**No external pager was invented.** The alerter is still `SharedAlerter`, and `main.rs` now
says so in as many words: it records in-process, and the durable `alert_outbox` row is the
operator-visible artefact until a real pager adapter is chosen.

---

## 4. Shipped without its own Pg test — flagged, not hidden

`impl DepositAmlIo for PgTx` (`credit_tx.rs`) landed during a build-red emergency: another lane
added the port and wired `credit_deposit.rs:283`, implementing it only for the fake, so the
workspace did not compile until the Pg side existed. It mirrors `withdraw_tx.rs:395-520`:
policy read from `config_entries` with **every key required and bounds-checked**; legs derived
from `deposits ∪ withdrawals ∪ credit_grant_lots`; `source_address` as the counterparty; flags
inserted under the existing `not exists (… status='open')` guard; returns whether the user has
any open flag, so the caller **holds** admission on `true`.

One deposit-specific correctness point that has no analogue on the withdrawal side: the
deposits row **already exists** at admission time, so the derived leg set carries
`and id <> $4` to exclude the candidate. Without it every deposit counts itself as both history
and candidate and is pushed one step further into the structuring band. Replay then evaluates
an identical set.

`tests/deposit_aml_contract.rs` now covers it against real Postgres, all shared-source:
three in-band $200 legs must NOT hold, the fourth must hold and raise exactly one open flag, a
replay of the same deposit must still hold without duplicating the flag, six $25 legs (past
`n = 4`, below the $100 floor) must never flag, a disagreeing amount is
`Conflict("deposit aml candidate")`, and an unknown deposit is `NotFound("deposit")`.

MUTATION CHECK, since there was no chronological red: removing the `and id <> $4` exclusion
gives
```
leg 2 of 3 is below the structuring n; nothing may be held yet
test result: FAILED. 2 passed; 1 failed
```
— the THIRD leg flags, because the candidate counted itself as history. That is exactly the
double-count the guard exists for, and it confirms the test detects it. Mutation removed.

---

## 5. Verification

`cargo fmt -p adapters -p main -- --check` — clean.
`cargo clippy -p adapters -p main --all-targets` — clean under the repo's `-D warnings`.
`cargo test -p adapters -p main --no-fail-fast` — **exit 0, 25 test targets all `ok`, 355
tests passing, 0 failed**,
including `kyc_inbox_race` (2), `resolve_lock_order` (2), `migration_all_invariants` (2),
`migration_0011_deposit_identity` (2), `pg_suites_cannot_skip` (3),
`secrets_compared_in_constant_time` (3), `deposit_aml_contract` (3),
`invariant_currency_detector` (3), `alert_contract` (4), `pg_contract` (36),
`withdraw_contract` (18), `http_routes` (41), adapters lib (193), main (4).

`cargo fmt --all -- --check` reports a diff in `crates/application/**` — **not my files**;
another lane was mid-edit. I did not touch them. No full-workspace gate was run, per
instructions.

### Schema validation performed directly against Postgres

- Unvalidated constraints after the full chain: **none** (was
  `deposits_observation_identity`).
- `ledger_entries_balanced` present and enabled (`tgenabled='O'`, `DEFERRABLE INITIALLY
  DEFERRED`, groups by `la.currency`); never dropped or disabled anywhere in `migrations/` or
  `crates/`.
- Index inventory dumped for 20 money/ops tables. Every `ON CONFLICT` target in
  `crates/adapters/src/pg/**` has a matching unique or partial-unique index.
- `withdrawals`' combination CHECK read in full: all 15 legal coarse × review × send triples,
  the release/settle XOR, and the `status='settled' ⟺ settle_tx_id IS NOT NULL` biconditional
  are enforced in the database, not only in code.

---

## 6. Rejected hypotheses and dispositions

### P9-7 — `NOT VALID` was not a defect; the migration is hygiene

Probes (each in a rolled-back transaction) proved `NOT VALID` does **not** mean unenforced:
INSERT of a machine-status row with NULL identity → rejected; UPDATE promoting a
`quarantined_legacy` row into the machine without identity → rejected (the one path by which an
unbacked liability could enter the deposit machine); and with all three grandfathered shapes
present, `VALIDATE CONSTRAINT` **succeeds**. So 0011's stated rationale is inaccurate — nothing
would have been retro-failed — but the marker cost nothing behaviourally.

I originally recommended **against** a `VALIDATE` migration: zero correctness gain, and a new
table scan at `MIGRATOR.run` that can fail the boot. The lead overrode that and asked for it in
`0012`; it is there. `migration_0011_deposit_identity.rs` still pins `convalidated = false`
immediately after 0011 (history), `migration_all_invariants.rs` pins `= true` after the full
chain (present). Both matter; neither replaces the other.

### P9-8 — currency-blind sweep: not reachable, hardened anyway

`account_balances` and `unbalanced_txns` were currency-blind, so equal-and-opposite
USDC/UsdcCredit drift would cancel. But `migrations/0002_ledger_triggers.sql:1-24` installs a
constraint trigger grouping by `la.currency` that refuses any per-currency-unbalanced commit,
and `PgTx::ledger_apply` validates per currency in Rust. A red test would have to disable the
very trigger that makes the state unreachable. **Refuted as reachable; confirmed as a
defense-in-depth gap** — the sweep was strictly weaker than the trigger it backstops. The lead
ruled the locked Phase-6 authority makes per-currency identities a contract question, so:
`account_balances` now carries `currency`, and `unbalanced_txns` groups by
`(e.txn_id, a.currency)` to match the trigger. `TxnSumRow` has no currency field (core's type),
so one offending pair is one row — core may want to reword `invariant_sweep.rs`'s
`"{n} unbalanced transactions"` detail string.

`tests/invariant_currency_detector.rs` proves the hardening end to end. It plants drift with
`session_replication_role = replica` — the technique `w2_ops_contract.rs:999-1018` already
establishes — because the only way to exercise a backstop is to create the state the front line
prevents. Identity 1: a transaction `+7 usdc / -7 usdc_credit` whose currency-blind sum is
provably `0` must still be reported. Identity 3: `usdc` drifted `+5` internal and `usdc_credit`
drifted `-5` external, chosen so the **aggregate cancels exactly** (`-Σexternal == Σinternal`)
while both currencies are individually broken — two real breaks hiding each other. A third test
pins that the trigger still refuses that commit through the ordinary path, so the planting is
not evidence that live code can produce it.

MUTATION CHECK: reverting `unbalanced_txns` to `group by e.txn_id` and collapsing
`AccountBalanceRow.currency` to a single bucket gives
```
identity 1 must report a transaction unbalanced within a currency
assertion `left != right` failed: Usdc is drifted and must not be reported as mirroring
test result: FAILED. 1 passed; 2 failed
```
Mutation removed.

I also caught a boundary bug introduced by core's widening of `balance_micro` to `i128`: the
old `::bigint` cast would have **raised inside Postgres** on exactly the oversized aggregate
the type was widened for. The sum now crosses as exact `::text` and parses to `i128`, with
`StoreError::Invariant("account balance exceeds i128")` beyond that rather than wrapping.

### P9-5 / P9-6 — asked to refute, could not

**P9-5.** Two independently sorted class-2 runs are not one ascending run, and referral users
are not a subset of voters. Live Postgres proof, two connections mimicking two concurrent
resolutions:
```
ERROR:  deadlock detected
DETAIL:  Process 7466 waits for ExclusiveLock on advisory lock [2393447,2,2309499808,2];
         blocked by process 7467.
         Process 7467 waits for ExclusiveLock on advisory lock [2393447,2,2242110839,2];
         blocked by process 7466.
```
Core applied the union-and-sort-once fix. `tests/resolve_lock_order.rs` pins **both**
directions: two runs deadlock (asserting the message names the detector), and one merged
ascending run does not. The positive control is not decoration — without it the suite would
only prove Postgres can deadlock, not that ordering fixes it. Its first draft **livelocked**,
because a barrier placed after a merged run can never be reached by the second transaction;
the barrier is now confined to the two-run case and the reason is documented in the source.

**P9-6.** `resolve_tx()` is READ COMMITTED; `referral_relevant_users` skips referees with no
bind, and `grant_referrals_on_paid` re-reads later in the same transaction and can see a bind
committed in between — granting for a referrer whose lock was never taken. Bounded by
`config_flag("feature_referrals")`, seeded `false`.

**Holder enumeration (lead's escalation): split verdict.** "Holders may not be voters" is
**refuted** — `save_position` (`trade_tx.rs:471`) is the only production writer, `PlaceTrade`
enforces the D4 vote gate in-tx against the same `votes` table `voter_ids` reads, and neither
`delete from votes` nor `delete from positions` appears anywhere. So holders ⊆ voters by
construction. But "the prelock may still miss a payout recipient" is **confirmed** by a
different mechanism: `voter_ids` is read *before* `market_for_update`, so its snapshot can be
stale relative to the post-lock `holdings` read. The required re-enumeration/retry step does
not exist. Ledger correctness is safe (`ledger_apply` sorts and row-locks accounts); what races
is the policy read — a payout is a cash ingress running `auto_collect` for a user whose class-2
lock a concurrent withdrawal holds.

### Sub-claim I raised and then killed

I suspected `reps_for_update`'s `... from unnest($1) as user_id order by user_id` evaluated the
lock function *below* the Sort, making the `ORDER BY` a no-op. **Wrong.** `EXPLAIN (VERBOSE)`
gives `Result (Output: probe(u)) -> Sort -> Function Scan`, and a side-effecting probe on input
`[30,10,20]` recorded evaluation order `10,20,30`. Recorded so nobody re-opens it.

### Others checked and dropped

- `rails/watcher.rs:87` `let _ = …await?` — the `?` propagates; only the `Ok` value is
  discarded. The lead independently withdrew this candidate.
- `withdrawals.request_fingerprint` has no unique index — not a double-withdrawal race:
  uniqueness lives in `request_fingerprints` and the path takes `serialize_key` first.
- `deposit_machine_from_row` decoding NULL identity to `""` — the decoder rejects incomplete
  identity and both machine queries exclude `quarantined_legacy`.
- Advisory class 4 shared between D25 pause fences and `lock_cap` — distinct key strings, no
  collision; naming hygiene only.
- `OutboxRelay::pump_once` — at-least-once with a documented safe duplicate; ordering holds
  because `main.rs` spawns exactly one relay.
- `openapi.json` — eight representative Phase-7 paths, all absent. No doc drift.
- P9-12: I could not construct a failing meaningful replacement for the tautology myself (all
  28 `Capability` variants are in the matrix; `required_capability` has no duplicate rows), so
  I left it until the lead's `scripts/test_test_quality.py` gave a real red.

---

## 7. Residual risks

- **Phase-7 routers are not composed.** 18 admin routes are `phase7_admin_stubs` returning a
  **bare 503 with no `ApiError` envelope** (post-auth, so no capability leak).
  `compliance_admin::admin_router` covers 12 of them, `compliance_admin::public_router` covers
  `POST /self_exclusions` and `/users/{id}/deposit_limit` (+ `/sandbox/kyc/complete` under the
  staging arm), `phone::router` covers `/phone/{challenge,verify}`, `kyc_webhook::router`
  covers `/webhooks/kyc`. Six are genuinely unimplemented at the HTTP layer:
  `/admin/credits/grant/*`, `/admin/bonus_reserve/topup/*`, `/admin/markets/{id}/fee_override/*`.
  **Consequence:** with the webhook unmounted there is no production path by which a KYC event
  reaches the system, so `users.kyc_tier` moves only by seed or manual SQL — and deposit and
  withdrawal admission gate on that tier. Every KYC fix above is therefore latent until the
  coordinator's integration pass, which is also when it is hardest to notice.
- **`Telemetry` and `ChainBalance` are never composed**; `application::ops::chain_reconcile`
  has **zero callers**, so D35's reconciliation detector is entirely uncalled — same root cause
  as P9-1, now fixed only for the invariant detector.
- **`main.rs:612-616` carries a stale comment** claiming "SharedAlerter + NoopTelemetry always
  compose". `NoopTelemetry` never composes. I rewrote the block I edited; the older stanza
  above it should go when someone next touches that region.
- **No structured logging.** Error handling is `eprintln!` throughout (14 sites in `main.rs`);
  no `tracing`, no correlation ids. `docs/spec.md:382` promises OpenTelemetry end to end.
- **`alert_outbox` retention.** 0012 adds the pending-delivery index, but resolved episodes are
  kept forever by design and nothing prunes them.
- **Three scratch-database suites use fixed names** (`opinions_suite_alert`,
  `opinions_suite_compliance`, `opinions_suite_withdraw`) and `drop database … with (force)`
  them at first use. Two workers running the same suite against one Postgres instance destroy
  each other's run. The per-test suites use UUID-suffixed names and are safe.
- **`vote_metadata` vs `withdraw::client_ip` diverge**: the vote path falls back to the trusted
  proxy's own IP when the whole `X-Forwarded-For` chain is trusted; the withdraw path returns
  `None` (fail-closed, as D33 requires). Defensible, but it will skew
  `subnet_share_max_ppm` toward false positives and deserves an explicit decision rather than
  two functions that quietly differ.
- **`reps_for_update` error text is now inaccurate.** After core's union fix it is called with
  voters ∪ referral users but still says `"voter without reputation row"`. Production is safe
  (the only production user-creation path inserts the reputation row in the same statement) but
  `notifier.rs`'s test fixtures create rep-less users — the scratch DB holds 4141 users and
  4093 reputation rows. Any future creation path that forgets the rep row now breaks
  *resolution*, not just reputation.
- **Adding a new Pg suite requires adding it to `SUITES`** in `pg_suites_cannot_skip.rs`.
  Deliberate — an unlisted suite is an ungated suite — but it is a manual step. All nine of my
  suites are listed.
- **`DepositAmlIo` velocity rules are not covered.** `deposit_aml_contract.rs` exercises the
  structuring band and its floor, the replay path and the binding conflict; the 24h deposit and
  withdrawal velocity thresholds ($5000 seeded) are evaluated by the same shared
  `evaluate_aml` but are not asserted from the deposit side. The withdrawal suite covers that
  algebra; the deposit-side coverage is band-only, and I would rather say so than imply the
  whole policy is pinned.
