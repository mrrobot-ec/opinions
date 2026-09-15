# P9 — adversarial refutation #2: KYC webhook atomicity and incident alert lifecycle

Read-only audit. **No VCS command of any kind was run.** No Cargo, no build directory, no
mutation. The only file written is this one. Snapshot taken 2026-08-15, ~20:40–20:52.

I tried to disprove both findings. **I could not disprove either.** Both are confirmed at
the code, schema and test level. Reachability differs sharply between them and is stated
per finding, because both sit behind surfaces that are not currently composed.

---

## Verdict table

| # | Claim | Verdict | Reachable today? | Prior disposition | Class |
|---|---|---|---|---|---|
| 1 | Inbox acceptance and the KYC effect commit in two transactions; a crash between them makes every retry a permanent `Replay` | **CONFIRMED — not disproved** | **No** — `kyc_webhook::router` is never merged | **codex-p7r1:73-75 named this exact risk; the accepted fix was never implemented** | spec-vs-implementation drift, real bug |
| 2a | `raise` does `find_open` then `insert` with no unique open constraint → concurrent duplicate incidents and duplicate pages | **CONFIRMED** | **No** — nothing calls `raise` | plan §D35 requires "dedup within an open incident" | real bug (latent), fake masks it |
| 2b | `raise` marks delivery successful before the alerter call, so a failed first page is never redelivered | **CONFIRMED** | **No** — no `raise` caller, and the composed alerter cannot fail | plan §D35 requires "at-least-once delivery" | real bug (latent); **the suite encodes it as intended behaviour** |
| 2c | `deliver_pending` synthesizes `UNIX_EPOCH` | **CONFIRMED** | Not today (queue is empty) | none | data-correctness / hardening, not liveness |
| 2d | *(found while testing 2c)* `deliver_pending` aborts the whole pump on the first page error, starving the queue behind it | **CONFIRMED** | Not today | none | hardening, head-of-line blocking |

---

## Finding 1 — KYC webhook crash safety

### The mechanism, traced

| Step | File:line | Transaction |
|---|---|---|
| handler verifies HMAC, resolves user | `crates/adapters/src/http/routes/kyc_webhook.rs:81-86` | `resolve_user_from_our_records` opens and commits its own read tx (`inbox.rs:75-79`) |
| `ingest` → `accept_inbox` | `crates/adapters/src/inbox.rs:122` → `crates/application/src/money/admin.rs:496-517` | **tx 1**: `admin_tx` → `inbox_get` → `inbox_insert` → **`tx.commit()` at `admin.rs:505`** |
| handler applies the effect **only if `Accepted`** | `kyc_webhook.rs:101-120` | — |
| `apply_inboxed_kyc` | `crates/application/src/money/kyc.rs:128-163` | **tx 2**: `store.compliance_tx()` at `:138` … `tx.commit()` at `:161` |

The two are genuinely distinct Postgres transactions on distinct pooled connections:

```rust
// crates/adapters/src/pg/compliance_tx.rs:55-59
async fn compliance_tx(&self) -> Result<Box<dyn ComplianceTx + '_>, StoreError> {
    Ok(Box::new(PgComplianceTx { tx: self.pool.begin().await.map_err(db_error)? }))
}
// crates/adapters/src/pg/compliance_tx.rs:64-68
async fn admin_tx(&self) -> Result<Box<dyn ComplianceAdminTx + '_>, StoreError> {
    Ok(Box::new(PgComplianceTx { tx: self.pool.begin().await.map_err(db_error)? }))
}
```

The replay decision has **no notion of "applied"**:

```rust
// crates/application/src/money/admin.rs:163-169
pub fn inbox_algebra(existing: Option<&InboxRecord>, incoming: &InboxRecord) -> InboxOutcome {
    match existing {
        None => InboxOutcome::Accepted,
        Some(prior) if prior.payload_hash == incoming.payload_hash => InboxOutcome::Replay,
        Some(_) => InboxOutcome::Conflict,
    }
}
```

So: tx 1 commits → process dies, connection drops, or `compliance_tx()` errors → the
provider retries the identical delivery → `Accepted` is impossible forever → the handler
skips `apply_inboxed_kyc` and answers **200 `{"outcome":"replay"}`**. The tier is never
applied and the failure is converted into a false success. There is no error left anywhere
to find.

### Disproof routes I tried, and why each failed

| Route | Result |
|---|---|
| A reconciler / sweeper over unapplied inbox rows | **None exists.** The inbox lives in `compliance_decisions` with `subject_type='inbox'` (`compliance_tx.rs:964-984`). `rg "subject_type = 'inbox'"` over all of `crates` returns **exactly one hit** — the read inside `inbox_get`, `compliance_tx.rs:937`. Nothing scans it. |
| A DB trigger repairing or coupling the two writes | **None.** `rg "create trigger\|create constraint trigger" migrations/*.sql` yields four triggers, all in `0002_ledger_triggers.sql` and `0003_accounts_identity.sql`, all on `ledger_*` tables. Nothing on `compliance_decisions` or `kyc_events`. |
| An outbox that carries the effect | **No.** The inbox row *is* a `compliance_decisions` row; nothing consumes it. |
| The two "transactions" secretly sharing one | **No** — both are `self.pool.begin()`, above. |
| Another production caller that would re-apply | **No.** `apply_inboxed_kyc` has exactly two callers: `kyc_webhook.rs:108` and `sandbox_complete_full` (`kyc.rs:176`), the staging two-factor sandbox route. A manual operator workaround is not a replay path. |
| Retrying the webhook repairs it | **No** — that is the defect: the retry is precisely what returns `Replay`. |
| The effect is idempotent, so "apply on Replay too" is already safe | **No.** `persist_kyc_event` (`kyc.rs:97-104`) is `insert_kyc_event` + `set_kyc_tier`; `kyc_events` has `id uuid primary key default gen_random_uuid()` and **no** unique key over `(user_id, provider_ref, to_tier)` (`migrations/0011_money.sql:237-245`). Re-applying would duplicate event rows. This matters for the fix design. |

### This is not a relitigation — it is an accepted requirement that never shipped

`docs/reviews/codex-p7r1.md:73` states the risk in the same words as the finding:

> "If an adapter commits the webhook inbox row and then crashes before the effect, later
> duplicates can be discarded forever; effect-first can double-apply."

`docs/reviews/codex-p7r1.md:75` is the accepted resolution:

> "Define an authenticated durable inbox with provider/event key, canonical payload hash,
> signature/timestamp verification result, **processing status/lease, and effect id**. …
> **Commit a directly coupled effect in the same DB transaction where possible; otherwise
> use a leased inbox command whose terminal effect is independently idempotent.**"

`docs/reviews/p7r1-resolution.md:13` and `:24` **ACCEPTED** it; `docs/reviews/codex-p7r2.md:20`
marked M2 **RESOLVED** — at the level of plan text.

What shipped has **neither** half: the persisted record carries only
`{event_id, payload_hash, body, user_id}` (`compliance_tx.rs:975-980`) — no processing
status, no lease, no effect id — and the effect is committed in a second transaction. The
codebase already articulates the correct principle elsewhere, in a comment on the very file
that holds the inbox:

> `compliance_tx.rs:81-83` — "written once so a money-effect transaction can CAS a proposal
> and commit its economic effect atomically **instead of opening a second transaction**."

### Reachability

**Latent, not live.** `kyc_webhook::router` is never merged into the served router:
`rg "kyc_webhook" crates` outside the file itself returns only
`crates/adapters/src/http/routes/mod.rs:47: pub mod kyc_webhook;`. This matches the
independently recorded P9-2 caveat and P9-9. The defect becomes live the moment the
coordinator's integration pass mounts the router — which is exactly when it is hardest to
notice, because the symptom is a 200 response.

### Smallest atomic fix

**Option A (preferred — matches the accepted "same DB transaction where possible").**
Both writes already target tables reachable from one `ComplianceTx`: the inbox row is a
`compliance_decisions` insert, and the effect is `kyc_events` + `compliance_decisions`.
Move the algebra inside the effect transaction:

```
compliance_tx()
  inbox_get(provider, event_id)          -- same tx
  match algebra:
    Accepted -> inbox_insert(record)      -- same tx
                persist_kyc_event(...)    -- same tx
                insert_decision(...)      -- same tx
    Replay   -> no writes
    Conflict -> no writes, typed error
commit()                                  -- one commit
```

The concurrency guard already exists and needs no migration: `inbox_insert` binds the
deterministic `inbox_subject_id(provider, event_id)` (`compliance_tx.rs:166-175`) as the
**primary key** `id` of `compliance_decisions`, and the insert maps a unique violation to
`StoreError::Conflict("inbox event")` via `conflict_or` (`compliance_tx.rs:73-79`). Two
concurrent deliveries therefore serialize on the PK: the loser re-reads and returns `Replay`.
This requires widening the trait boundary so the inbox methods (today on
`ComplianceAdminTx`) are callable from the effect transaction — a trait-level change, no
schema change.

**Option B (only if the traits cannot be merged).** Add `applied_at` / `effect_id` to the
inbox payload; treat "prior exists, same hash, **not applied**" as *apply-again*; and add
`unique (user_id, provider_ref, to_tier, at)` — or an explicit idempotency key — to
`kyc_events` so re-apply is genuinely idempotent, since it is not today. Strictly larger
than Option A and it does need a migration.

### Required red-first tests

1. **Crash/error injection (unit, application layer).** A `ComplianceStore` double whose
   `admin_tx()` succeeds but whose *next* `compliance_tx()` returns
   `StoreError::Unavailable`. Deliver the event once (expect `Err`), then deliver the
   identical event again. Assert the user's `kyc_tier` equals the requested tier.
   **Red today:** the second delivery returns `Replay` and the tier is unchanged.
2. **Concurrency (Pg contract).** Two simultaneous identical deliveries. Assert exactly one
   `kyc_events` row for the user **and** the tier applied — the current PK collision proves
   only that the *inbox* row is unique, never that the effect ran.
3. **Rollback (Pg contract, post-fix).** Force a failure after `inbox_insert` but before
   commit inside the single transaction; assert **no** `compliance_decisions` row with
   `subject_type='inbox'` exists, so the retry is `Accepted`. This is the test that pins the
   atomicity rather than the outcome.
4. **Positive control.** A clean delivery writes both the inbox row and the `kyc_events`
   row, and a genuine duplicate (same key, same hash, effect already applied) writes
   neither a second event nor a second tier change.

---

## Finding 2 — incident alert atomicity and lifecycle

### 2a — concurrent `raise` creates duplicate open incidents and duplicate pages: **CONFIRMED**

```rust
// crates/application/src/ops/alerts.rs:152-171
if let Some(existing) = self.store.find_open(&key).await? { return Ok(existing); }
let incident = Incident { … };
self.store.insert(incident.clone()).await?;
self.alerter.page(severity, &key.encoded(), body).await?;
```

Disproof routes, all dead:

- **Transaction scope.** There is none. `PgAlertStore::find_open` (`alert_tx.rs:109-113`)
  and `insert` (`alert_tx.rs:133`) each `execute`/`fetch_optional` on **`&self.pool`** —
  two separate pooled connections, each autocommit. No `begin()`, no `for update`, no
  advisory lock anywhere in the file.
- **Isolation level.** Cannot help: raising the isolation level of two independent
  single-statement autocommits on different connections changes nothing.
- **Unique constraint / index.** None. `alert_outbox` (`migrations/0011_money.sql:354-372`)
  declares only `id uuid primary key`, and `rg "alert_outbox" migrations/*.sql` returns
  **exactly one line** — the `create table` — so there is no index on `incident_key` at all.
- **Upsert.** `insert` is a plain `insert into … values (…)` with no `on conflict`
  (`alert_tx.rs:118-123`).
- **Serialization by a single caller.** Would be a reachability argument, not a disproof —
  and see below.

**Why no test catches it:** `MemoryAlertStore::insert` is
`guard.insert(incident.key.encoded(), incident)` over a `BTreeMap` keyed by the encoded key
(`alerts.rs:105-112`), so a duplicate raise **overwrites** and leaves exactly one incident.
The fake and the Pg adapter disagree on precisely the property under test.

**Spec:** `docs/plans/phase7-money-compliance.md:42` makes dedup normative — "durable alert
outbox; incident key = detector+subject+episode; **dedup within an open incident**". This is
a defect against an accepted requirement, not discretionary hardening.

**Minimal fix — and it must be a *partial* index.** A plain
`unique (incident_key)` would be **wrong**: `alert_contract.rs:243-252` asserts
`select count(*) from alert_outbox == 2` after resolve + recurrence, because episodes are
intentionally multi-row. The correct constraint is

```sql
create unique index alert_outbox_one_open_per_key
    on alert_outbox (incident_key)
    where status in ('open','acked');
```

then have `insert` map the unique violation to "someone else raised it": re-run `find_open`
and return that incident **without paging**. `find_open`'s `order by created_at desc limit 1`
(`alert_tx.rs:107`) becomes redundant but stays harmless.

**Red-first test:** a Pg concurrency test firing two `raise` calls for one key concurrently
(`tokio::join!`), asserting `count(*) where incident_key = $1` is 1 and the recording
alerter saw exactly 1 page. It must be a Pg test — the memory store cannot express the bug.

### 2b — delivery marked successful before the alerter call: **CONFIRMED**, and the suite blesses it

```rust
// crates/application/src/ops/alerts.rs:166-172
delivery_attempts: 1,                                        // :166
last_paged_at: Some(now),                                    // :167
…
self.store.insert(incident.clone()).await?;                  // :171 persisted as "paged"
self.alerter.page(severity, &key.encoded(), body).await?;    // :172 may fail AFTER
```

The retry queue is defined as "never observed a page":

- `alert_tx.rs:170` — `where status = 'open' and last_paged_at is null`
- `alerts.rs:127-129` — the same predicate in the memory store
- `migrations/0011_money.sql:361-363` — the column comment makes it normative: "*Distinct
  from updated_at: `pending_delivery` is `status='open' AND last_paged_at IS NULL`, so the
  pager can never confuse 'row touched' with 'operator actually paged'.*"

So a first-page failure is **permanently** excluded from redelivery. Re-calling `raise` does
not repair it either — `alerts.rs:153-155` returns the existing incident before reaching the
alerter. This violates `phase7-money-compliance.md:42` "**at-least-once delivery**".

**The strongest evidence is in the tests, which encode the defect as intended behaviour.**

`crates/adapters/tests/alert_contract.rs:226`, immediately after a real `raise` against real
Postgres:

```rust
// It was paged at raise, so nothing is pending.
assert_eq!(manager.deliver_pending().await.unwrap(), 0);
```

And both "at-least-once" tests hand-force a row shape that `raise` can never produce —
`alert_contract.rs:172-178`:

```rust
// An incident enqueued without a page (crash between insert and page) is
// the whole point of the durable outbox.
let mut row = incident(undelivered.clone(), now());
row.delivery_attempts = 0;
row.last_paged_at = None;
store.insert(row).await.unwrap();
```

The comment **names the exact crash this finding describes**, then constructs the row by
hand because the only producer of incidents cannot create it. `alerts.rs:336-350` does the
same in the unit suite. Both are fabricated positive controls: the durable outbox's central
guarantee is verified only against rows no production path emits.

Compounding it: **no `Alerter` implementation anywhere returns `Err`.** `RecordingAlerter::page`
always returns `Ok(())` (`alerts.rs:262-269`), and production composes `SharedAlerter`, a thin
wrapper over it (`crates/adapters/src/money_ports.rs:33-38`). A page failure is not currently
expressible in the test suite at all.

**Minimal fix:** insert with `delivery_attempts: 0, last_paged_at: None`; page; then `save`
with `delivery_attempts: 1, last_paged_at: Some(now)`. A crash or error between insert and
page then leaves the row pending — exactly what `alert_contract.rs:172` already claims to
test. Accepted cost: a page that lands but whose `save` fails will be re-paged by a later
pump. That is at-least-once, which is the specified semantics, and it makes `raise` and
`deliver_pending` consistent (today `raise` is at-most-once and `deliver_pending` is
at-least-once — opposite orderings in the same module).

**Red-first test:** add a `FailingAlerter` double returning `Err(StoreError::Unavailable(..))`.
`raise` → expect `Err`; then assert `store.pending_delivery().len() == 1` (**red today: 0**),
and that a following `deliver_pending()` pages it exactly once. Mirror it in the Pg contract
suite so the column semantics are pinned end to end.

### 2c — `UNIX_EPOCH` synthesis: **CONFIRMED**, data-correctness only

```rust
// crates/application/src/ops/alerts.rs:224
incident.last_paged_at = incident.last_paged_at.or(Some(OffsetDateTime::UNIX_EPOCH));
```

`pending_delivery` returns **only** rows with `last_paged_at IS NULL`, so the `.or(...)` is
not a fallback — it is the **only** branch ever taken. Every redelivered incident is durably
stamped `1970-01-01T00:00:00Z`.

- **Liveness is fine:** the value is non-NULL, so the row does leave the queue and
  at-least-once terminates. This is *not* a lost-page bug.
- **Correctness is not:** the column whose stated purpose is to distinguish "row touched"
  from "operator actually paged" (`0011_money.sql:361-363`) now records a page that
  demonstrably did not happen in 1970. Any future age-based re-page or SLA policy reads every
  redelivered incident as infinitely stale.
- **Root cause is structural:** `IncidentManager` has no clock. `raise`, `ack` and `resolve`
  all take `now: OffsetDateTime` (`alerts.rs:151, 180, 203`); only `deliver_pending`
  (`alerts.rs:216`) does not.

**Minimal fix:** `pub async fn deliver_pending(&self, now: OffsetDateTime)` and assign
`Some(now)`. One production call site to update: `crates/main/src/main.rs:629`.
Classification: **hardening**, not a confirmed liveness bug — but a one-line fix.

### 2d — head-of-line blocking in the pump (found while testing 2c)

```rust
// crates/application/src/ops/alerts.rs:219-222
for mut incident in pending {
    self.alerter.page(…).await?;   // `?` aborts the whole pump
```

A single incident whose page fails aborts the entire tick via `?`, before any later incident
is attempted. `pending_delivery` orders by `created_at, id` (`alert_tx.rs:171`), so the same
poison incident sits at the head of the queue on every subsequent tick and starves everything
behind it indefinitely. The production loop only logs the error and sleeps a minute
(`main.rs:628-631`). **Hardening:** accumulate per-incident errors, continue the loop, and
report a count. Not part of the original claim; recorded so it is not lost.

### Reachability for all of finding 2

**Nothing in production calls `raise`.** `rg "IncidentManager|\.raise\(|deliver_pending"`
over `crates` returns exactly one production site — `main.rs:620-632` — which constructs an
`IncidentManager` and pumps **only** `deliver_pending()` every minute. Every `raise` caller
is a test — `crates/adapters/tests/alert_contract.rs:214,221,239` and the in-file unit tests
at `alerts.rs:313,317,327` (inside `#[cfg(test)] mod tests`, which begins at `alerts.rs:294`).
`PgAlertStore` is the only writer to
`alert_outbox` (the smoke script now only *reads* it, `scripts/e2e_swarm_smoke.sh:1168`), so
the table is empty in production and `pending_delivery` returns nothing.

That is the same gap recorded as **P9-1** ("nothing raises a D35 incident") and **P9-10**.
It means 2a/2b/2c/2d are all **latent**: correct as code defects, unreachable until a
detector is wired. Fix them **before** wiring a detector, not after — 2b in particular is a
silent lost-page and 2a a duplicate page, and both are far harder to attribute once real
incidents are flowing.

---

## Summary for the lead

- **Finding 1 is a confirmed bug and an accepted-requirement regression.** codex-p7r1:75
  required same-transaction coupling *or* a lease plus an idempotent terminal effect;
  neither shipped, and the failure presents as a 200 `"replay"`. Fix before the router is
  mounted. Option A needs no migration.
- **Finding 2a and 2b are confirmed bugs against the normative D35 contract**
  (`phase7-money-compliance.md:42`), each with a one-line-to-one-index fix. 2b is the more
  serious of the two: it silently defeats the durable outbox, and both existing
  "at-least-once" tests prove the guarantee only against hand-built rows that `raise` cannot
  emit, while `alert_contract.rs:226` asserts the defective behaviour outright.
- **2c and 2d are hardening**, both cheap, neither a liveness bug.
- **Two fake/production divergences surfaced**, and they are why review missed these:
  `MemoryAlertStore::insert` overwrites by key so the duplicate-raise race cannot be
  observed in unit tests, and no `Alerter` implementation can fail so the page-failure path
  is untestable as written. Any fix should land its red test in the **Pg** contract suite,
  plus a failing-alerter double in the unit suite.
