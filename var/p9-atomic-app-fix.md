# P9 — application-layer fix: atomic KYC inbox application + durable incident delivery

Scope: the application half of the two defects independently confirmed in
`var/p9-atomic-refutation-2.md`. **No VCS command of any kind was run.** Only the
six owned files were edited. No adapter, main, migration, doc, script, or other
file was touched. No gate was weakened: no coverage exclusion, no `#[allow]`
added to silence clippy (the one clippy finding was fixed properly), no test
removed, ignored, or loosened.

Backups taken with plain `cp` before the first edit, sha256-verified identical at
the time of copy:

```
var/backup/crates/application/src/ops/alerts.rs
var/backup/crates/application/src/money/admin.rs
var/backup/crates/application/src/money/kyc.rs
var/backup/crates/application/src/money/mod.rs
var/backup/crates/application/src/fakes/compliance.rs
```

Build dir: `CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-atomic` (removed at
hand-off). `DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_core`.
No other target directory was created and no other worker's target was touched.

---

## 1. Final gate results

```
cargo test -p application            580 passed; 0 failed; 0 ignored
cargo clippy -p application --all-targets -- -D warnings    clean (exit 0)
cargo fmt --all -- --check           exit 0, zero diffs tree-wide
```

Focused suites: `ops::alerts` 14 / `money::kyc` 8 / `money::admin` 5 /
`fakes::compliance` 5 — all passed, 0 failed. Raw captures in
`var/p9-atomic-evidence/`.

---

## 2. Defect 1 — KYC inbox acceptance and effect were two commits

### Red (observed before the fix)

`accept_and_apply_inboxed_kyc` was first written as the route that ships today —
acceptance committed in one `compliance_tx`, the effect applied in a second — and
tested against a double that really is a transaction (staged writes, visible only
on `commit`). The shared `FakeComplianceStore` cannot express this: its `commit`
is `Ok(())` and its writes are immediate, which is exactly why the defect survived
review.

```
$ cargo test -p application money::kyc
test money::kyc::tests::a_failed_kyc_effect_leaves_no_acceptance_so_the_retry_still_applies ... FAILED

thread '...' panicked at crates/application/src/money/kyc.rs:478:9:
assertion `left == right` failed: an acceptance must not survive a failed effect:
the stranded row is what makes every retry a Replay
  left: 1
 right: 0

test result: FAILED. 7 passed; 1 failed; 0 ignored; 0 measured; 565 filtered out
```

### The fix

`crates/application/src/money/kyc.rs` — new
`accept_and_apply_inboxed_kyc(store, record, to_tier, provider_ref, valid_until,
policy_version, now) -> Result<InboxOutcome, AppError>`. One `compliance_tx`
carries: the inbox algebra read, the user lock, the inbox insert, the KYC event,
the tier, and the compliance decision, then one `commit`. Any failure rolls the
acceptance back with the effect, so the provider's retry is a fresh `Accepted`
rather than a `Replay` over an effect that never happened. This is the
"commit a directly coupled effect in the same DB transaction" half of
`docs/reviews/codex-p7r1.md:75`, accepted at `docs/reviews/p7r1-resolution.md:13,24`
and never implemented.

Ordering inside the transaction is `lock_user` → `inbox_insert` → KYC writes, so
the class-2 user lock is still taken first.

**Trait change — a MOVE, not an addition.** `inbox_get`/`inbox_insert` moved from
`ComplianceAdminTx` (`money/admin.rs`) down into its supertrait `ComplianceTx`
(`money/mod.rs`). Declaring them on both is `E0034` ambiguity because
`ComplianceAdminTx: ComplianceTx`. Every existing `ComplianceAdminTx` caller —
including `accept_inbox` — keeps compiling through the supertrait, unchanged.

**Preserved, deliberately:** `accept_inbox` and `apply_inboxed_kyc` are untouched
and still exported, and `sandbox_complete_full` still delegates to
`apply_inboxed_kyc`. Its round-trip test (`persist_and_sandbox_complete_round_trip`)
passes unmodified.

### Green

```
$ cargo test -p application money::kyc
test money::kyc::tests::a_failed_kyc_effect_leaves_no_acceptance_so_the_retry_still_applies ... ok
test money::kyc::tests::a_replayed_delivery_applies_nothing_twice ... ok
test money::kyc::tests::a_same_key_different_hash_delivery_is_a_typed_conflict_that_applies_nothing ... ok
test money::kyc::tests::an_unresolved_user_is_refused_before_any_write ... ok
test money::kyc::tests::persist_and_sandbox_complete_round_trip ... ok
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 569 filtered out
```

Coverage of the required behaviours: rollback + retry (failure injected after the
inbox insert, before commit); positive delivery (inbox + event + tier + decision
all committed once); `Replay` on same key/same hash with **no** second KYC event;
typed `ProposalConflict` on same key/different hash with nothing applied; and an
unresolved user refused before any write.

---

## 3. Defect 2 — incident lifecycle

### Red (observed before the fix)

Five behavioural claims, run as assertion failures rather than compile errors by
landing the API surface first (`insert_if_absent`, `deliver_pending(now)`) with
the old behaviour still in place:

```
$ cargo test -p application ops::alerts
failures:
    ops::alerts::tests::a_concurrent_raise_opens_exactly_one_incident_and_pages_once
    ops::alerts::tests::a_failed_first_page_leaves_the_incident_pending_for_redelivery
    ops::alerts::tests::a_poison_incident_does_not_starve_the_incidents_behind_it
    ops::alerts::tests::an_acked_but_never_paged_incident_stays_pending
    ops::alerts::tests::redelivery_stamps_the_supplied_time_and_never_the_unix_epoch

test result: FAILED. 5 passed; 5 failed; 0 ignored; 0 measured; 559 filtered out
```

Representative panics:

```
a_failed_first_page_leaves_the_incident_pending_for_redelivery
  assertion failed: an incident whose first page failed must stay in the
  at-least-once queue        left: 0   right: 1

redelivery_stamps_the_supplied_time_and_never_the_unix_epoch
  assertion failed: the pump must stamp the real time it paged, not 1970
  left: Some(1970-01-01 0:00:00.0 +00:00:00)   right: Some(2023-11-14 22:18:20.0 +00:00:00)

an_acked_but_never_paged_incident_stays_pending
  assertion failed: an operator ack is not a page; the incident is still
  undelivered                left: 0   right: 1
```

### The fixes, in `crates/application/src/ops/alerts.rs`

1. **`raise` persists pending first.** The incident is inserted with
   `delivery_attempts: 0, last_paged_at: None`; only after the page succeeds does
   it `save` `attempts = 1, last_paged_at = Some(now)`. A pager failure or crash
   between insert and page now leaves the incident in `pending_delivery` — the
   reason the durable outbox exists. Previously it was stamped as paged before the
   page was attempted, so a failed first page was excluded from redelivery forever
   and re-calling `raise` returned the existing incident without ever paging.
2. **Atomic open-or-get.** New `AlertStore::insert_if_absent`, used by `raise` in
   place of `find_open` + `insert`. Two concurrent raises could both read "absent"
   and open two incidents with two pages. `MemoryAlertStore` implements it under
   one lock; the adapter gets the same guarantee from a partial unique index.
3. **`deliver_pending(now)` takes a real clock.** `pending_delivery` only returns
   rows whose `last_paged_at` is NULL, so the old `.or(Some(UNIX_EPOCH))` was not a
   fallback — it was the only branch, and it durably recorded every redelivery as
   having paged in 1970.
4. **Non-starvation without silence.** A failing incident no longer aborts the
   sweep (the queue is oldest-first, so one poison incident starved everything
   behind it on every tick). The whole batch is attempted; the **first** pager or
   save error is kept and returned after the loop, so a broken pager can never read
   as a quiet healthy sweep. An all-success batch still returns `Ok(count)`.
   *(Applied per lead review; proven red-first — the poison test asserting `Err`
   failed against the earlier continue-and-`Ok` version at `alerts.rs:544`.)*
5. **Acked is still pending.** `MemoryAlertStore::pending_delivery` now matches
   `Open | Acked` with `last_paged_at.is_none()`. An operator acknowledging an
   incident is not the pager delivering it; only `Resolved` retires an undelivered
   incident.
6. **`sync_invariant_report` (new, for the DB lane's no-production-raise gap).**
   Per identity: detector `invariant_breach`, subject = identity name, episode =
   the exported `INVARIANT_EPISODE` (`"active"`). A failing identity raises and
   dedups with the identity's detail as the body; a passing identity resolves an
   existing open/acked incident via the new `resolve_if_open`; a passing identity
   with nothing open is a no-op. Every identity is attempted and the first error is
   returned. This keeps `main` wiring-only.

Recurrence after resolve is preserved: `insert_if_absent` only treats `Open`/`Acked`
as blocking, so a resolved episode does not suppress a new one.

### Green

```
$ cargo test -p application ops::alerts
test ops::alerts::tests::a_failed_first_page_leaves_the_incident_pending_for_redelivery ... ok
test ops::alerts::tests::a_successful_first_page_records_one_attempt_at_the_supplied_time ... ok
test ops::alerts::tests::redelivery_stamps_the_supplied_time_and_never_the_unix_epoch ... ok
test ops::alerts::tests::a_poison_incident_does_not_starve_the_incidents_behind_it ... ok
test ops::alerts::tests::an_acked_but_never_paged_incident_stays_pending ... ok
test ops::alerts::tests::a_resolved_incident_is_never_redelivered ... ok
test ops::alerts::tests::a_concurrent_raise_opens_exactly_one_incident_and_pages_once ... ok
test ops::alerts::tests::insert_if_absent_reports_the_existing_open_incident_and_reopens_after_resolve ... ok
test ops::alerts::tests::an_ongoing_invariant_breach_pages_once_and_then_dedups ... ok
test ops::alerts::tests::a_recovered_identity_resolves_and_a_recurrence_pages_a_new_incident ... ok
test ops::alerts::tests::a_continued_pass_with_nothing_open_is_a_no_op ... ok
test ops::alerts::tests::a_failing_identity_whose_page_fails_stays_pending_and_the_error_surfaces ... ok
test ops::alerts::tests::open_incident_is_deduped_until_resolved_then_repages ... ok
test ops::alerts::tests::pending_delivery_is_at_least_once_and_missing_ack_is_typed ... ok
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 563 filtered out
```

`sync_invariant_report` was also driven red-first from a no-op stub: 3 of its 4
tests failed (`11 passed; 3 failed`) before the implementation landed; the
`a_continued_pass_with_nothing_open_is_a_no_op` test passed against the stub by
construction, which is correct — it asserts a no-op.

Both pre-existing tests still pass untouched.

### A note on why no existing test caught any of this

`MemoryAlertStore::insert` overwrites by key, so a duplicate raise could not be
observed in unit tests; and no `Alerter` implementation in the tree could return
`Err`, so a page failure was inexpressible. The two "at-least-once" tests
hand-forced `delivery_attempts = 0, last_paged_at = None` — a row shape `raise`
could never emit. The new `PartlyFailingAlerter` and `RacyStore` doubles close both
gaps.

---

## 3b. Defect 1b — the inbox key was never serialized (found by the DB lane after the first green)

Making acceptance and effect atomic was necessary but not sufficient. Two Pg reds
from the adapter lane showed the remaining race:

- **Red A** — 15 runs, 12 pass / 3 fail: the losing duplicate delivery surfaced a raw
  `Store(Conflict("inbox event"))` instead of the D33 `Replay`.
- **Red B** — same key with a different payload hash must produce the typed
  `ProposalConflict` plus the page, not a raw `StoreError`.

Two of my own hypotheses were wrong and are recorded so they are not retried:

1. *Move `lock_user` before `inbox_get`.* Wrong. The user is resolved from the
   payload's `provider_ref`, and a conflicting payload can name a **different**
   `provider_ref` and therefore a different user — so a class-2 user lock does not
   order two deliveries that share an inbox key.
2. *Catch the unique violation and re-read.* Wrong. After SQLSTATE 23505 the
   Postgres transaction is aborted; any further statement fails with 25P02 unless a
   `SAVEPOINT` was taken first.

**The fix is the codebase's own existing rule.** `ports/market.rs:13-20` already
documents `IdempotencyGuard::serialize_key` as "`pg_advisory_xact_lock(1, hashtext(key))`
— MUST be the first call in every write tx (codex B3): it serializes duplicate
requests so the read-or-create that follows is race-free." `ComplianceTx` was the
one write path in the tree without it. Added narrow
`ComplianceTx::serialize_inbox(provider, event_id)`, called as the **first**
statement in both `accept_and_apply_inboxed_kyc` and legacy `accept_inbox`, before
`inbox_get`, with the user lock after.

### Fake/Pg role-contract parity — the fake must be able to block

A no-op fake would re-hide exactly this race: the application suite was green at
577 tests while Postgres raced. `FakeComplianceTx::serialize_inbox` therefore takes
a real owned per-key guard, mirroring `LockMap` in `fakes/state.rs` and the
`IdempotencyGuard` impl at `fakes/market.rs:1-12`. `KeyLocks` is a local private
copy in `fakes/compliance.rs` — `LockMap` is private to `state.rs` and widening it
would be a cross-owner refactor. Guards are held in
`inbox_guards: HashMap<String, OwnedMutexGuard<()>>` and released when the
transaction commits **or drops**, which is what `pg_advisory_xact_lock` does.

Three retained parity tests, shaped after the precedent at `fakes/ops_config.rs:572`:

```
test fakes::compliance::tests::serialize_inbox_blocks_a_second_tx_on_the_same_key_until_the_first_ends ... ok
test fakes::compliance::tests::a_dropped_transaction_releases_its_inbox_key ... ok
test fakes::compliance::tests::serialize_inbox_is_reentrant_within_one_transaction ... ok
```

They assert a second transaction times out at 50 ms on the held key, proceeds
within 200 ms once the first commits (and separately, once the first is *dropped*,
so rollback releases too), that a different key proceeds immediately, and that a
transaction does not deadlock against its own key. Per the coordinator these are
**parity evidence**, not a red-first artifact: the real Pg race is what proves the
production bug, so no no-op implementation was manufactured just to make them fail.

**Closed by the adapter lane.** `PgComplianceTx::serialize_inbox` landed as the
class-1 `pg_advisory_xact_lock` over the same `kyc-inbox:{provider}:{event_id}` key,
and the DB lane has confirmed **both** Pg races green — the duplicate-delivery race
(previously 12 pass / 3 fail with raw `Store(Conflict("inbox event"))`) and the
different-hash / different-user race, which now yields the typed `ProposalConflict`
plus the page. The retained harness is the deterministic table/advisory barrier;
the 24-pair probabilistic approximation was rejected and replaced.

## 4. Files changed (all owned)

| File | Change |
|---|---|
| `crates/application/src/ops/alerts.rs` | `insert_if_absent` on `AlertStore` + `MemoryAlertStore`; `raise` pending-first; `deliver_pending(now)` with accumulate-and-return; `pending_delivery` includes `Acked`; `sync_invariant_report` + `resolve_if_open`; `INVARIANT_EPISODE`; 12 new tests + 2 doubles |
| `crates/application/src/money/mod.rs` | `inbox_get`/`inbox_insert` + `serialize_inbox` added to `ComplianceTx` |
| `crates/application/src/money/admin.rs` | the two inbox methods removed from `ComplianceAdminTx` (now inherited); `accept_inbox` serializes the inbox key first |
| `crates/application/src/money/kyc.rs` | `accept_and_apply_inboxed_kyc` (serialize → algebra → lock → writes → one commit); 4 new tests + a transactional store double |
| `crates/application/src/fakes/compliance.rs` | inbox methods moved into the fake's `ComplianceTx` impl; private `KeyLocks` + owned per-key guards; 3 lock-contract parity tests |
| `var/p9-atomic-app-fix.md` | this report |

`var/p9-atomic-evidence/` holds the raw red/green captures.

---

## 5. Cross-owner contract (sent to the lead as escalations #1 and #2)

The adapter/runtime lane must land these for `crates/adapters` and `crates/main` to
compile; `crates/application` compiles, tests, and lints standalone today.

1. **`PgComplianceTx`** — move the existing `inbox_get`/`inbox_insert` bodies from
   the `ComplianceAdminTx` impl into the `ComplianceTx` impl. Verbatim move, no SQL
   change, no migration. **Also implement `serialize_inbox`** as the class-1
   advisory lock, reusing the body shape at `adapters/src/pg/withdraw_tx.rs:116-123`
   (`select pg_advisory_xact_lock(1, hashtext($1))`) over a namespaced key; the fake
   uses `kyc-inbox:{provider}:{event_id}`. It must be **class 1** (serialization),
   not class 2 (user locks).
2. **`kyc_webhook.rs`** — replace `ingest(...)` + `if outcome == Accepted { apply_inboxed_kyc(...) }`
   with the single `accept_and_apply_inboxed_kyc(...)` call, keeping the
   page-on-conflict behaviour by mapping `AppError::ProposalConflict("inbox payload hash conflict")`.
3. **`PgAlertStore::insert_if_absent`** — backed by
   `create unique index alert_outbox_one_open_per_key on alert_outbox (incident_key) where status in ('open','acked');`.
   It must be **partial**: `adapters/tests/alert_contract.rs:243-252` requires two
   rows per key after resolve + recurrence, so a plain unique on `incident_key`
   would break it.
4. **`PgAlertStore::pending_delivery`** — widen to
   `where status in ('open','acked') and last_paged_at is null`.
5. **`deliver_pending(now)`** — update `crates/main/src/main.rs:629` to pass a real
   clock and to treat the returned `Err` as a reported failure rather than a fatal
   loop exit; update `adapters/tests/alert_contract.rs:227`.
6. **`sync_invariant_report`** — wire the continuous invariant loop to call it
   instead of only logging violations, which closes the known no-production-`raise`
   gap without putting policy in `main`.

Note for the adapter test suite: `raise` now inserts pending and only marks the
page after it succeeds. The success-path assertions in
`alert_contract.rs:214-226` still hold; any assertion that a raised incident is
never pending after a **failed** page is inverted by design.

**Status at hand-off:** items 1, 2 and the `serialize_inbox` half of item 1 have
landed in the adapter lane and both Pg KYC races are confirmed green with a
deterministic barrier harness. Items 3–6 (the alerts partial unique index,
`pending_delivery` widening, `deliver_pending(now)` call sites, and wiring
`sync_invariant_report`) remain with the adapter/runtime lane.

---

## 6. Final gate results at hand-off

```
cargo fmt --all -- --check                                   exit 0 (zero diffs, whole tree)
cargo clippy -p application --all-targets -- -D warnings     exit 0
cargo test -p application                                    580 passed; 0 failed; 0 ignored
  fakes::compliance   5 passed    money::kyc     8 passed
  money::admin        5 passed    ops::alerts   14 passed
```

`CARGO_TARGET_DIR=/Users/mrrobot/opinions-build/p9-atomic` was the only target
directory used and was removed at hand-off. No other worker's target was touched.
