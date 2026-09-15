# P9 — independent adversarial refutation of the in-flight findings

Read-only audit. No VCS command of any kind was run. No Cargo, rustc, just, npm, pnpm,
uv, pip, migration, or build was run. No file outside this one was written. All claims
below come from reading the current tree, diffing it against the explicit `var/backup`
copies, and reading the authority docs and prior dispositions.

Snapshot taken **2026-08-15 20:30–20:36**. Other lanes are editing concurrently; every
conclusion that depends on a shared file names the file's mtime at the moment I read it,
and the three load-bearing ones were re-read immediately before this was written.

`var/p9-python-web-report.md` **did not exist** at any point during this audit
(`ls var/p9-*.md` at 20:33 → core, db-runtime, smoke-fix, smoke-refutation only). The
candidate-5 verdicts are therefore derived from the code and the `var/backup/web`
originals, not from that lane's red/green evidence.

---

## Verdict table

| # | Candidate | Verdict | Reachability | Prior disposition | Red test honest? | Fix complete? |
|---|---|---|---|---|---|---|
| 1a | Unchecked `i64` receivable total (deposit path) | **SUSTAINED** as panic-safety; **REJECTED as a live defect** | Needs Σ open receivables > `i64::MAX` (~$9.2T) | none | yes (real overflow panic) | yes |
| 1b | AML velocity `i64` overflow | **SUSTAINED** as panic-safety; **REJECTED as a live defect** | same order of magnitude | none | yes | yes for the decision; **evidence value is falsified** |
| 2a | Invariant-sweep `i64` aggregation → `i128` | **SUSTAINED, but the repair is not range-complete** | effectively nil | core report §3 | test constructs the state **below** the adapter | partial — see the row boundary |
| 2b | Invariant-sweep currency blindness | **SUSTAINED as hardening**; the earlier "not reachable" ruling still stands | unreachable through the write path (DB trigger) | **P9-8 rejected as reachable** | n/a (no red test possible) | yes, semantics-preserving today; **one new blind spot** |
| 3a | Resolution class-2 lock inversion | **SUSTAINED** | live; two markets resolving concurrently | **P9-5 CONFIRMED** with a real 40P01 | yes (live Postgres deadlock) | yes; **one regression risk retained** |
| 3b | Referral bind/enumeration TOCTOU | **SUSTAINED — and NOT closed in production** | latent behind `feature_referrals=false` | **P9-6 CONFIRMED** | **positive control is fake-only** | **NO — fake and Pg adapter diverge** |
| 3c | Pre-market participant snapshot race | **SUSTAINED as a residual** (new) | narrow but real | not previously dispositioned | no test exists | **NO — the accepted re-enum/retry step is absent** |
| 3d | Locked payout oldest-first collection | **SUSTAINED**; zip alignment hypothesis **REJECTED** | n/a | codex-p7r3 B5 (accepted) | **ordering half has no positive control** | placement/alignment correct |
| 4 | simswarm records HTTP success as WS delivery | **SUSTAINED, and worse than reported**; **the repair does not work** | live in every gated run | not previously dispositioned | **positive control is fabricated at the wire boundary** | **NO — parses a format the server never emits** |
| 5a | Web withdrawal retry idempotency | **PARTIALLY REFUTED** — no money hole existed | n/a | none | n/a | correct, but it is not the safety property |
| 5b | Exact fixed-point amount parsing | **SUSTAINED** | live | none | n/a | yes |
| 5c | Refused-receipt presentation | **SUSTAINED** (DTO mismatch), **refusal branch REFUTED as reachable** | live for the shape; dead for the branch | none | n/a | yes, via `moneyRequest`, not via the copy |

---

## 1. Receivable total and AML velocity overflow

### 1a — `ops::receivable_collection::auto_collect`

Diff vs `var/backup/current/crates/application/src/ops/receivable_collection.rs`: the
unchecked `sum::<i64>()` is now `try_fold` + `checked_add` → `AppError::Overflow`,
returned before `account`, `ledger_apply`, or any movement insert. The error precedes
every write, so there is no partial-collection semantics change.

**Sustained** as a panic/wrap defect: the red output in `var/p9-core-report.md:34` is a
genuine `accum.rs:206 attempt to add with overflow`, not a contrived assertion, and the
withdrawal-path collector already used `checked_add` — the asymmetry is the real
justification.

**Refuted as a live defect.** Reaching it needs one user's open receivables to sum past
`i64::MAX` ≈ 9.2 × 10¹² USD. `receivables.opened_micro` is `bigint`, so you would need
thousands of near-maximal rows for a single user. Ship it as consistency hardening; do
not describe it as an exploitable path.

**No residual.** The signature change (`&mut (dyn DepositTx)` → generic
`T: LedgerWriter + ReceivableCollectionIo + ?Sized`) is what lets `resolve_market` reuse
it (§3d) and does not alter behaviour.

### 1b — `money::aml::evaluate_aml`

**Sustained** on the same panic-safety grounds, and the "fail closed" argument holds for
the *decision*: `saturating_add` pins the total at `i64::MAX`, which is ≥ any
`deposit_velocity_micro_24h` / `withdraw_velocity_micro_24h`, so the flag still fires
(`aml.rs:160-165`).

**Residual the lead should know about — the durable evidence is falsified.** The saturated
totals are not confined to the comparison; they are returned in `AmlEvaluation` and written
straight into the persisted flag payload (`aml.rs:207-211`:
`"deposit_velocity": evaluation.deposit_velocity, "withdraw_velocity": evaluation.withdraw_velocity`).
A saturated evaluation therefore records `9223372036854775807` as the observed 24-hour
volume — a number that is not the true total, on a compliance record a human will later
read. Two different policies are now in use for the same defect class in the same lane:
`checked_add` → typed `Overflow` (1a) versus `saturating_add` → silent clamp (1b). If the
clamp is kept, the evidence JSON should carry a `saturated: true` marker.

---

## 2. Invariant sweep

### 2a — `i128` aggregation, and the numeric→bigint row boundary

The repair is real: `external_mirrors_internal` and `MoneyIdentityFacts` aggregate in
`i128`, and the mirror identity uses `try_fold`/`checked_add` → `AppError::Overflow`.

**The row boundary makes it incomplete, exactly as suspected.**
`crates/adapters/src/pg/invariant_read_tx.rs` computes
`coalesce(sum(e.amount_micro), 0)::bigint as balance_micro` per `(account, currency)`, and
`unbalanced_txns` computes `sum(e.amount_micro)::bigint`. Postgres `sum(bigint)` yields
`numeric`; the `::bigint` cast raises **SQLSTATE 22003 `bigint out of range`** the moment a
single account balance or a single `(txn, currency)` imbalance leaves `i64`. That error
becomes `StoreError::Backend` and the sweep fails as a query error — it can never *report*
a ledger that overflows at the row level.

Consequences to state plainly:

1. The `i128` change only covers "many individually in-range rows whose total leaves
   `i64`". That requires **two or more accounts each near `i64::MAX`**, i.e. two ~$9.2T
   authorities. Not a live path.
2. The red test
   `integrity::invariant_sweep::tests::aggregate_money_identities_handle_totals_larger_than_i64`
   builds `MoneyIdentityFacts::from_balances([i64::MAX, i64::MAX])` **directly**, below the
   adapter that would have refused to produce it. It is a valid unit test of the arithmetic
   and an invalid demonstration of reachability. `var/p9-core-report.md:86-88` says
   "a valid multi-row aggregate can exceed that range" — true in principle, but the sentence
   should not be read as evidence of an attainable production state.
3. If total-range safety is actually wanted, the cast has to go too (return `numeric` as a
   string/`i128`, or `sum(...)::numeric`). That is an adapter change nobody has proposed.

**Verdict: keep the change, restate the claim.** It is defensive hardening against a debug
panic, not a defect closure.

### 2b — currency blindness

**The earlier ruling was right and still is.** `var/p9-db-runtime-report.md:458-481`
rejected this as reachable because `migrations/0002_ledger_triggers.sql` installs
`ledger_entries_balanced`, which groups by `la.currency` and raises if any currency sums
non-zero, and `PgTx::ledger_apply` validates per currency as well. I did not find a way to
commit the offending state either. The fix has landed anyway as defence in depth; that is a
legitimate call, but the finding should be recorded as **hardening, prior disposition
unchanged**, not as a defect the earlier review missed.

Three adversarial checks on the applied change:

- **`unbalanced_txns` regrouped to `(txn_id, currency)`** — strictly stronger, and the
  duplicate-`txn_id` consequence is benign: the identity is `unbalanced.is_empty()`
  (`invariant_sweep.rs:54-59`) and `.len()` only feeds the human-readable detail string.
  ✔ no semantic change.
- **`MoneyIdentityFacts::from_balances` now filters `currency == Usdc`** — I tried to find a
  legitimate non-USDC `Withheld` / `DepositSuspense` / `BonusReserve` account and could not.
  Every construction site is `Currency::Usdc`: `credit_tx.rs:443`, `credit_tx.rs:1036`,
  `credit_deposit.rs:174,306,507,1171`, `fakes/ops.rs:1884,1943`. ✔ semantics-preserving
  today. **Residual:** the filter *drops* such a row silently rather than failing, so a
  future UsdcCredit suspense/withheld account would become invisible to identities 8–10
  instead of loud.
- **A new blind spot the fix introduces.** `invariant_sweep.rs:77` iterates a hardcoded
  `[Currency::Usdc, Currency::UsdcCredit]`. `domain::ledger::Currency` has exactly those two
  variants (`ledger.rs:32-35`) and there is **no `Currency::ALL` and no exhaustive match**,
  so adding a third variant silently excludes it from identity 3 — where the previous
  currency-blind aggregate would at least have included it. Cheap fix: drive the loop from an
  exhaustive `match` or a `Currency::ALL` const so the compiler catches the next variant.

---

## 3. Resolution, referrals, and the payout collection hook

### 3a — class-2 lock inversion: sustained, fix correct

`crates/application/src/resolve_market.rs` (mtime 20:30:51) now builds one union
(`lock_users = voters ∪ referral users`), sorts and dedups it once, and takes **one**
`reps_for_update` call at :171-184. The old `for user in &referral_users { tx.lock_user(*user) }`
loop is gone (confirmed against `var/backup/crates/application/src/resolve_market.rs`).
`grep -n "voter_ids\|market_for_update"` shows a single class-2 acquisition site before the
market lock.

I tried three ways to break it and failed:

- **Filter correctness.** `locked_reps` is narrowed back to voters with
  `voter_ids.binary_search_by_key(&row.user.0, |user| user.0)`. `voter_ids` is sorted and
  deduped at :160-161 and never mutated afterwards (`lock_users` is a clone), so the binary
  search is valid. ✔
- **Client/server order agreement.** Rust sorts by `Uuid` (`Ord` = big-endian byte compare);
  `reps_for_update` orders by `user_id` in SQL, and Postgres `uuid` also compares the 16
  bytes. They agree — and in any case the SQL `order by` is what determines acquisition,
  which `var/p9-db-runtime-report.md:483-493` already verified with `EXPLAIN (VERBOSE)`.
- **Hash collisions in `pg_advisory_xact_lock(2, hashtext(user_id::text))`.** Not a deadlock
  source: both transactions acquire in ascending `user_id`, and the key is a deterministic
  function of `user_id`, so the induced key sequences cannot invert; a collision merely makes
  one lock cover two users, and advisory locks are re-entrant within a transaction.

**Regression risk retained (independently confirmed, already raised by the DB lane).**
`reps_for_update` still returns `StoreError::Invariant("voter without reputation row")` when
`rows.len() != ids.len()` (`resolve_tx.rs:371-373`), and the id set now includes non-voting
referrers. A user without a `reputation` row now breaks **resolution**, not just reputation.
The message is also wrong for the widened set. Production is currently safe only because
`deposit_tx.rs:147-150` inserts the rep row in the same statement.

### 3b — bind/enumeration TOCTOU: **NOT closed in production**

This is the finding I most expected to refute and could not; it inverted instead.

`crates/application/src/resolve_market.rs:163-167` now says:

> "Include unbound qualifying referees so a concurrent bind cannot appear after enumeration
> without sharing their lock."

**That claim is false against the current Pg adapter.**
`crates/adapters/src/pg/resolve_tx.rs:26-29` (re-read at **20:35:54**, file mtime
**05:20:47** — untouched this session) still reads:

```rust
for referee in referees {
    let Some(bind) = CreditIo::referral_bind_for_referee(self, referee).await? else {
        continue;                       // unbound referee is SKIPPED — no lock taken
    };
```

The **fake** does the opposite. `crates/application/src/fakes/market.rs:595-603` (mtime
**20:09:47** — edited this session):

```rust
for referee in referees {
    // The referee must be locked even before a bind exists. BindReferral
    // takes this same lock, closing the enumeration/bind TOCTOU.
    users.push(referee);                // unbound referee IS locked
    let Some(bind) = ... else { continue };
```

So the fake locks unbound referees and Postgres does not. Any application-layer test that
demonstrates the TOCTOU is closed passes against the fake and **does not hold in
production** — precisely the fake/contract drift class. The P9-6 exposure
(`var/p9-db-runtime-report.md:319-330`: `resolve_tx()` is READ COMMITTED, and
`grant_referrals_on_paid` re-reads the referee list later in the same transaction) is
therefore still open.

Caveat I am obliged to state: `resolve_tx.rs` belongs to the adapter lane, which recorded
P9-6 as "CONFIRMED, fix owned by the core worker". This may be work in flight rather than a
final state. **Recommended action: the lead confirms the adapter half landed before the
barrier, and does not accept a green application test as evidence that it did.** Impact
stays latent while `feature_referrals` is seeded `false` (`referrals.rs:540`).

### 3c — pre-market participant snapshot race: sustained residual, no test

`voter_ids` is read at `resolve_market.rs:159`, **before** `market_for_update` at :185.
`holdings` is read at :282, **after**. There is no re-enumeration and no retry, although the
accepted Phase 7 algorithm (codex-p7r3 B5, recorded in `docs/reviews/p7r3-resolution.md`)
specifies "enumeration → sorted user locks → market lock → **re-enum/retry** → payout +
oldest-first collection in-tx".

Window, stated precisely: resolution enumerates voters at T0 while the market is still Live;
a concurrent `PlaceTrade` commits a vote+trade at T1 (it holds the market lock, which
resolution has not yet taken); the market closes at T1.5; resolution acquires the market
lock at T2 and sees `Closed`, so it proceeds. The T1 trader is a payout recipient who was
**not** in the class-2 lock run.

The core lane's retraction (`var/p9-core-report.md:122`, "valid user payout recipients are a
subset of voters") is correct about the *final* state and does not cover a *stale*
enumeration. That subset property is now load-bearing for the new collection hook, and
nothing asserts it.

Before the hook, an unlocked recipient only meant missing class-2 serialisation on a payout
whose ledger rows are locked anyway. **The hook raises the stakes:** `auto_collect` now reads
`open_receivables_for_user` and writes `ReceivableMovement` rows for that user under a lock
nobody holds, and the movement idempotency key is path-scoped
(`recv-collect:payout:{market}:{user}:{receivable}` versus the withdraw path's
`recv-collect:withdraw-fp:...`), so two concurrent collectors on the same receivable are
**not** deduplicated by key. Over-collection past the outstanding balance is the failure mode.

This is a static reachability argument. I executed nothing, so treat it as a residual to
close (add the re-enum/retry, or assert `payout_users ⊆ lock_users` and fail loudly), not as
a demonstrated race.

### 3d — payout oldest-first collection

**Hypothesis I raised and killed.** `holdings.iter().zip(&settlement.payouts)`
(`resolve_market.rs:299-305`) looked like a positional-alignment bug that could collect from
the wrong user. It is sound: `settle_market` allocates
`Vec::with_capacity(holdings.len())` and pushes exactly one `(account, payout)` per holding
in input order (`crates/domain/src/resolution.rs:121-134`), and `settlement_input` is built
from `holdings` in order. ✔ **Rejected.** Fragility note only: there is no length assertion,
so a future `settle_market` that filtered zero payouts would silently misattribute
collections; keying payouts by `AccountId` would remove the hazard.

Placement is right: after the payout `ledger_apply`, after the crash point, inside the same
transaction, over a sorted and deduped `payout_users`.

**The ordering half of the algorithm has no positive control.** Oldest-first exists only in
Postgres — `crates/adapters/src/pg/unwind_tx.rs:190-191`, `order by r.created_at, r.id`. The
fake sorts by `rows.sort_by_key(|row| row.receivable.id)` — a random UUID
(`crates/application/src/fakes/ops.rs:349`). No application-layer test can therefore
distinguish oldest-first from arbitrary order, and deleting `order by r.created_at` from the
adapter would be caught by nothing in the suite. Second instance of the same fake/production
gap as §3b.

**Product semantics:** payouts now sweep open receivables at settlement. That is user-visible
and is the accepted codex-p7r3 B5 item, so it is a specified change, not a regression — worth
one line in `docs/copy/ops.md` all the same.

---

## 4. simswarm WS delivery — sustained, and **the repair does not work**

### The old behaviour was worse than reported

`var/backup/crates/simswarm/src/engine/runner.rs` fabricated the series in **two**
independent places:

1. `ActorWorker::run` — `if matches!(operation, "trade-confirm" | "vote") && 2xx { latency.record("ws-delivery", 200, elapsed) }`, recording the **HTTP round-trip** as a WS delivery sample.
2. `Runner::record_ws_delivery` — `http_send_marks.pop()`, i.e. **LIFO**, correlating a
   delivery to the most recent send rather than the matching one. Even granting the premise,
   the correlation was wrong.

### The new collector is honest in shape and wired

`WsDeliveryTracker::observe` requires `type == "trade"`, an `outbox_seq`, and a parseable
`created_at`, and records one sample per `outbox_seq`. It is wired to a real socket:
`crates/simswarm/src/main.rs:204` inside `collect_ws_latency`, which subscribes, waits for
each `snapshot`, then reads frames.

### It cannot parse a single real frame

`observe` uses `OffsetDateTime::parse(created_at, &Rfc3339)` (`runner.rs`, the
`let Ok(created_at) = … else { return }` guard).

`ServerFrame` derives `Serialize` (`crates/adapters/src/http/ws.rs:109`) with **no**
`#[serde(with = "time::serde::rfc3339")]` on any field — `grep -n "serde(" ws.rs` returns
only the two container attributes — and `crates/adapters/Cargo.toml:16` enables
`serde-human-readable`. The `time` crate's default human-readable form is
`2026-08-13 09:43:53.608904 +00:00:00`: a space instead of `T`, and a three-part offset.
`Rfc3339` rejects it.

Two independent confirmations that this is the live wire shape:

- `scripts/e2e_swarm_smoke.sh:447` — `AGED_AT=$(date -u -v-80d '+%Y-%m-%d %H:%M:%S.0 +00:00:00')`,
  under the in-file comment "`time`'s serde-human-readable wire shape (the DTO's actual contract)".
- `web/lib/__tests__/countdown.test.ts:24` — the browser client parses
  `parseTimeMs("2026-08-13 09:43:53.608904 +00:00:00")`.

Frames are emitted through that derive: `serde_json::to_string(frame)` at `ws.rs:420`.

**Therefore every real `trade` frame falls out at the parse guard and the `ws-delivery`
series is empty in a live run.**

### The positive control is fabricated at the wire boundary

`websocket_frames_are_the_only_ws_latency_evidence` (`runner.rs:815-849`) feeds
hand-written `"1970-01-01T00:00:00Z"` and `"1970-01-01T00:00:00.010Z"` — RFC3339 strings the
server never emits. The test verifies the tracker's arithmetic and dedup, not its ability to
read a frame. The companion negative test
(`http_success_is_not_a_websocket_delivery_sample`) is genuine and does fail the old
behaviour; the gap is that nothing pins the **format contract** between `ServerFrame` and the
tracker.

### The failure is loud, which is the one mercy

`expected_ws_delivery = if spike == 0 { 0 } else { 1 }` (`trace.rs:94`), and smoke's
`close_spike_ticks` is non-zero (`domain/schedule.rs:26` default 120; `:81` asserts
`smoke.close_tick - smoke.close_spike_ticks == 60`). With zero observed samples,
`check_count(0, 1)` returns `ThinSeries` (`trace.rs:670-678`), so the gated run goes **red**
rather than passing on an empty series. The repair breaks the gate; it does not weaken it.

**Minimum fix:** either annotate `ServerFrame::Trade.created_at` with
`#[serde(with = "time::serde::rfc3339")]` (a wire-format change other clients must absorb —
`web/lib/__tests__/countdown.test.ts` proves at least one parses the current shape), or parse
the actual human-readable format in `observe`. Then add a test that serialises a real
`ServerFrame::Trade` and feeds the resulting JSON to `observe`, so the two sides cannot drift
again.

### Two further residuals

- **Cross-clock measurement.** `delivered_at` is the swarm client's `OffsetDateTime::now_utc()`;
  `created_at` is the server's timestamp. The sample is client-clock minus server-clock, not an
  elapsed interval. Same-host e2e makes this ~0, but negative skew makes
  `u64::try_from((delivered_at - created_at).whole_milliseconds())` fail and the sample is
  **silently dropped**, which looks identical to the parse failure above.
- **The measured quantity changed.** The old fabricated sample was an HTTP round trip; the new
  one is commit → frame receipt, including outbox relay pickup and fan-out.
  `SLO_WS_DELIVERY_P95_MS = 100` was calibrated against the old quantity. Re-baseline it
  deliberately rather than discovering it as a red gate.
- **`paid_at` (genuine strengthening, small flakiness residual).** The old code set
  `closed_at = now` on first seeing `paid`, manufacturing a ~0 ms close-to-paid sample; the new
  code records `paid_at` only if a `closed` observation was actually seen. Correct. But if the
  1 ms poller never catches `closed`, no sample is produced, and `Profile::Full` requires
  `expected_close_to_paid = 1` (`trace.rs:89-93`).

---

## 5. Web withdrawal surfaces

No `var/p9-python-web-report.md` existed during this audit, so there is no red/green evidence
to assess; the following is from the code and `var/backup/web`.

### 5a — retry idempotency: **partially refuted**

`web/lib/money.ts` now keeps one `activeWithdrawalAttempt` keyed by a fingerprint of
`[userId, amount_micro, dest, confirm_dest]` and reuses its `idempotency_key` on a retry.

What I can confirm is correct: the fingerprint includes every field the server's
`intent_fingerprint(user, amount, dest)` uses, so changing the amount mints a **new** key and
cannot provoke a spurious `IdempotencyConflict`; and the assignment happens before the
`await`, so a double-click reuses one key (the button is also `disabled={busy}`).

**But the framing overstates it: there was no money hole.** `RequestWithdraw`
(`crates/application/src/money/withdraw_request.rs:38-41`) looks up
`intent_fingerprint(user, amount, dest)` **before** anything else and returns the stored
receipt with `replayed = true`. The old client, with no key at all, could not double-hold
either. The client change removes a spurious-conflict class and makes the receipt legible; it
does not close a duplicate-withdrawal path. Present it that way.

**Residual:** `activeWithdrawalAttempt` is never cleared on success, so a deliberate second
identical withdrawal reuses the key and replays the first receipt. That happens to match the
server (the durable fingerprint refuses an identical repeat forever), so the UI is honest —
but the copy renders "was already accepted" without explaining that an identical repeat is
impossible by construction.

### 5b — exact fixed-point parsing: **sustained**

Old: `Math.round(Number(amount) * 1_000_000)`. Concrete counterexamples it accepted:
`"1e3"` → 1 000 000 000 µ ($1000) from a three-character input; `"5.0000005"` → 5 000 001 µ,
silently rounding a 7th decimal **up**; `".5"` and `"5."` → accepted; `"abc"` → `NaN`, and
`Math.round(NaN)` is `NaN`, which went on the wire. New `parseUsdMicros` uses
`/^(0|[1-9]\d*)(?:\.(\d{1,6}))?$/` with a `BigInt` path, rejects ≤ 0, and bounds at
`Number.MAX_SAFE_INTEGER`. Exact, and the page blocks submission on `null`.

Deliberate, user-visible tightening: forms the old field accepted (`.5`, `5.`, `1e3`,
leading zeros, 7+ decimals) now show an error instead of silently changing the amount. That is
the right trade for money; note it as a product change rather than a pure bugfix.

### 5c — refused-receipt presentation: sustained shape, **refuted branch**

The real defect is a DTO mismatch and it is sustained. `var/backup/web/lib/money.ts` declared
`WithdrawReceipt { id; status; review_state?; amount_micro }`, but the server's
`WithdrawalReceiptDto` (`crates/adapters/src/http/dto/withdraw.rs:19-31`) has **no** `status`
and **no** `review_state`. The old page therefore rendered
`Request <id> is undefined.` The new interface matches the Rust DTO field for field
(`id, user_id, dest, amount_micro, combo, hold_tx_id, replayed, refused, refuse_code, refuse_message`). ✔

**Refuted:** `withdrawReceiptCopy`'s refusal branch is unreachable through the HTTP client.
`withdrawal_status` (`crates/adapters/src/http/routes/withdraw.rs:188-201`) returns `200`
**only** when `!receipt.refused`; every refusal maps to 403/423/503/402/429/422, and
`moneyRequest` throws `ApiClientError` on `!res.ok` before any copy runs. A receipt with
`refused: true` cannot arrive on a 2xx.

The change that actually fixes the user-visible refusal is the other one in the same diff:
`moneyRequest` now reads `refuse_code` / `refuse_message` off the **error** body (the refusal
body *is* the receipt DTO) and surfaces them through `ApiClientError`, which the page renders
in its error paragraph. Credit that; describe the refusal copy as defensive, currently
dead code.

---

## Recommended actions, in priority order

1. **simswarm `created_at` format contract (§4).** The WS SLO measurement is currently
   dead and will fail the gated smoke run with `ThinSeries`. Fix the parse (or the
   serialisation) and add a test that round-trips a real serialised `ServerFrame::Trade`
   through `WsDeliveryTracker::observe`. Re-baseline `SLO_WS_DELIVERY_P95_MS` deliberately.
2. **Referral TOCTOU fake/production divergence (§3b).** `fakes/market.rs` locks unbound
   referees; `pg/resolve_tx.rs` (mtime 05:20:47) does not. Confirm the adapter half is
   landing; do not accept a green application test as evidence.
3. **Pre-market snapshot race (§3c).** Add the accepted re-enum/retry after the market lock,
   or assert `payout_users ⊆ lock_users` and fail loudly. The new payout collection hook makes
   this materially worse than it was.
4. **Oldest-first has no positive control (§3d).** Order the fake by `created_at` so an
   application test can pin the algorithm the plan names.
5. **`reps_for_update` widened-set regression (§3a).** A referrer without a `reputation` row
   now breaks resolution; the error message still says "voter".
6. **AML saturated evidence (§1b).** Either use `checked_add` for consistency with 1a, or mark
   the saturated total in the persisted evidence.
7. **`Currency` iteration blind spot (§2b).** Drive identity 3 from an exhaustive match or a
   `Currency::ALL` const.
8. **Restate, do not re-scope, §1a / §1b / §2a.** All three are debug-panic hardening at
   totals around $9.2 trillion. Keep them; stop describing them as reachable defects, and note
   that the `::bigint` row cast means §2a is not range-complete regardless.
