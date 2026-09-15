# P9 — adversarial refutation of the Phase 7 smoke-script theatre claim

**Candidate claim under attack:** `scripts/e2e_swarm_smoke.sh` Phase 7 section
(lines 785–1000) is verification theatre and can report green without
exercising required money/compliance behavior.

**Verdict: CONFIRMED, and stronger than stated.** I attacked it from four
directions (predicate space, route composition, the last recorded green run's
own artifacts, and a null-implementation replay) and could not refute it. Ten of
the thirteen named legs are green against an *absent* route, a *hardcoded 503*,
or a tautological self-INSERT. Three legs survived refutation and are real; they
are named and excluded below.

One finding goes past the claim: as the tree stands today the section can no
longer reach green at all — leg 8's own fixture violates
`deposits_observation_identity`, so the run aborts at line 851. The last green
artifact set proves it used to get past that line, i.e. script and migration
have drifted.

Constraints honored: no VCS command of any kind, no Cargo, no build directory,
no production/test/doc/migration/config edit. Owned writes only:
`var/p9-smoke-refutation.md`, `var/p9-smoke-refutation-proof.sh`,
`var/p9-smoke-refutation-proof.out`, `var/p9-smoke-refutation-nullimpl.sqlite`,
`var/p9-smoke-refutation-check.sqlite`.

---

## 1. Verification run

```
$ bash -n scripts/e2e_swarm_smoke.sh
syntax OK                                  # exit 0, no output

$ bash var/p9-smoke-refutation-proof.sh    # full output: var/p9-smoke-refutation-proof.out
```

The harness runs three independent proofs, no network, no Postgres, no Cargo.

### PROOF A — accepted status space per leg

Each function is a verbatim transcription of the leg's literal acceptance test.
`A` = "the leg treats the attack as failed and the run stays green".

```
leg                                 200  400  403  404  409  422  423  500  503
------------------------------------------------------------
1 wash-after-grant preview            A    A    A    A    A    A    A    A    A
5 omitted config version              .    A    A    A    A    A    A    A    A
6 fee override propose                .    A    A    A    A    A    A    A    A
7 unsigned KYC webhook                .    A    A    A    A    A    A    A    A
9 dust warm-up + dump                 A    A    A    A    A    A    A    A    A
10a Hit user -> mule dest             .    A    A    A    A    A    A    A    A
10b self-excluded -> new dest         .    A    A    A    A    A    A    A    A
11 settle-then-retry daily cap        .    A    A    A    A    A    A    A    A
13b unwind racer vs Paid              .    A    A    A    A    A    A    A    A
14 admit/refund source lock           .    A    A    A    A    A    A    A    A
```

Every leg accepts `404` (route absent) and `503` (hardcoded stub). Legs 1 and 9
accept **everything including 200**: leg 1's status check is `|| echo` (line
793–794) and leg 9 pipes `-w '%{http_code}'` into `>/dev/null` and never
compares it (lines 867–872).

### PROOF A2 — statuses actually observed in the last "PHASE 7 W4 E2E GREEN" run

Source: `artifacts/phase7-w4-p7w4-1786686052-95850/` (the run
`artifacts/phase7-w4-report.md` cites as green).

```
6 fee override        503 (phase7_admin_stubs)                    LEG PASSED
7 unsigned webhook    404 (kyc_webhook::router never merged)      LEG PASSED
9 dust withdraw       404 (empty body artifact)                   LEG PASSED
10a Hit dest          404 (empty body artifact)                   LEG PASSED
10b self-excluded     404 (empty body artifact)                   LEG PASSED
11 daily retry        404 (empty body artifact)                   LEG PASSED
13b unwind racer      409 (IllegalTransition, market Paid)        LEG PASSED
14 refund propose     404 (empty body artifact)                   LEG PASSED
```

`p7-conc-withdraw.status` literally contains `404`, and every other
`/withdrawals` artifact (`p7-dust-1000000.json`, `p7-dust-90000000.json`,
`p7-hit-withdraw.json`, `p7-self-excl.json`, `p7-daily-retry.json`) and
`p7-unsigned-webhook.json`, `p7-fee-override.json`, `p7-refund.json` are
**zero-byte** — the empty body of a 404/503. Six money/compliance legs reported
"attack failed" against a route that did not exist.

### PROOF B — null-implementation replay

The exact SQL of the SQL-only legs, run against an EMPTY schema in sqlite3 with
**zero production code**: no watcher, no referral engine, no AML scanner, no
reconciliation detector, no withdraw machine, no credit converter.

```
GREEN  leg 1  wash-after-grant: converted lots            got=0 want=0
GREEN  legs 2-4  unverified-phone referral grant sum      got=0 want=0
GREEN  legs 2-4  A<->B referral grants                    got=0 want<=1
GREEN  leg 8  no-KYC observation unadmitted/unbound       got=1 want=1
GREEN  leg 9  dust dest auto-approved rows                got=0 want=0
GREEN  leg 10  freshest sanctions verdict                 got=hit want=hit
GREEN  leg 12  four $25 deposits open AML flags           got=0 want=0
GREEN  leg 13  no residual page before injection          got=0 want=0
GREEN  leg 13  injected residual pages                    got=1 want=1
GREEN  leg 13  recurrence dedups inside open incident     got=1 want=1
GREEN  leg 13  in-flight exposure not paged as drift      got=0 want=0

Null-implementation replay: 11 leg assertions GREEN, 0 RED.
```

11/11 assertions pass in a world where none of Phase 7 was ever written.

### PROOF C — `deposits_observation_identity` vs the script's own fixtures

```
line 846 partial observation rows inserted: 0 (script expects rejection => 0)
line 851 'complete' observation rows inserted: 0 (script assumes 1)
line 851 error: CHECK constraint failed: deposits_observation_identity
```

---

## 2. Structural facts (source evidence)

**Three Phase 7 routers exist but are never merged into the served router.**

| Router | Defined | Mounted |
|---|---|---|
| `kyc_webhook::router` | `crates/adapters/src/http/routes/kyc_webhook.rs:31` | **no** |
| `phone::router` | `crates/adapters/src/http/routes/phone.rs:48` | **no** |
| `compliance_admin::admin_router` / `public_router` | `compliance_admin.rs:140,180` | **no** |

`rg -n "kyc_webhook|compliance_admin|routes::phone|phone::router" crates --type rust`
returns exactly two hits, both bare `pub mod` declarations
(`crates/adapters/src/http/routes/mod.rs:44` and `:47`; `pub mod phone;` sits at
`:50` and matches none of those patterns). Nothing references the routers.
`crates/adapters/src/http/routes/core.rs:252-278` is the only composition site
and merges `content`, `ops_config`, `ops_admin`, `phase7_admin_stubs`,
`withdraw::admin_router`, `deposit_admin`, `video`, `withdraw`.

This is a **known, tracked** gap — `docs/reviews/p7-coordinator-integration-todo.md:6-16`
item 1 is "Mount W2 routers … replacing the `phase7_admin_stubs` rows". What is
*not* tracked is that the smoke script already reports those legs as passing.

**`phase7_admin_stubs` is a hardcoded 503 table.**
`core.rs:184-186` defines `async fn phase7_unavailable() -> StatusCode { SERVICE_UNAVAILABLE }`
and `core.rs:188-232` binds it to 20 routes, including
`/admin/markets/{id}/fee_override/propose` and `/confirm`. Leg 6 targets exactly
that path and asserts `!= 200`.

**Every `/withdrawals` POST in the script omits a required DTO field.**
`crates/adapters/src/http/dto/withdraw.rs:9-17`:

```rust
pub struct WithdrawRequestDto {
    pub user_id: Uuid,
    pub amount_micro: i64,
    pub dest: String,
    /// Required second entry of the destination; both values must canonicalize identically.
    pub confirm_dest: String,
    pub idempotency_key: Option<String>,
}
```

`confirm_dest` is a bare `String` — no `Option`, no `#[serde(default)]`.
`rg -n "confirm_dest" scripts web services docs` returns **nothing**; the field
appears only in `dto/withdraw.rs` and `routes/withdraw.rs`. All five script
bodies (lines 870–871, 884, 892, 901, 944) are `{user_id, amount_micro, dest}`.
`Json(body)` is an extractor in `request_withdraw` (`routes/withdraw.rs:74-81`;
the `Json` argument is line 79, `require_demo` is line 81), so it runs **before**
the handler body and before `require_demo` — the request
dies as an axum `JsonDataError` (422) with no geo, sanctions, warmth, limit, or
self-exclusion code ever reached. The same rejection shape is visible in the
green run's `p7-omit-version.json`:
`Failed to deserialize the JSON body into the target type: missing field ...`.

**Consequence:** completing the coordinator's mounting TODO does **not** repair
legs 9/10/11/13b. They flip from 404 to 422 and keep passing.

---

## 3. Per-leg findings

Legend for "survives deletion": would the leg still report green if the
production implementation it names were deleted outright?

| # | Intended invariant (plan §3 exit criteria 4/6/10) | Executable action in the script | Accepted space | Survives deletion? | Evidence |
|---|---|---|---|---|---|
| 1 | wash-after-grant no-convert **with stated fee volume ≥ lot** — proves the Paid-finalization rule | line 791 raw `insert into credit_grant_lots`; line 793 a **preview** of $500; line 795 counts `converted_at is not null` | preview status ignored (`|| echo`) | **yes** | previews are read-only; no trade is placed, no market goes Paid for `GRANT_USER`; `p7-wash-preview.json` shows a 200 preview and nothing else. PROOF B row 1 |
| 2–4 | A↔B referral once; unverified-phone grant 0; third device zero | raw `insert into referral_codes` / two raw `insert into referral_binds` (`|| true`); sums `credit_grant_lots where source like 'referral%'` | any | **yes** | no HTTP call, no device dimension anywhere in the leg; `BINDS` (line 802) is captured and only echoed at line 1000, never asserted. PROOF B rows 2–3 |
| 5 | omitted `expected_config_version` → 422 | real `POST /trades` without the field | 400, 422, or anything ≠ 200 | **no** | `dto/market.rs:227` `pub expected_config_version: i64` (required); artifact `p7-omit-version.json` carries the serde message. **Real — see §4** |
| 6 | fee override with old-generation preview → **409** | `POST /admin/markets/{id}/fee_override/propose` | ≠ 200 | **yes** | route is `phase7_unavailable` (`core.rs:225-231`); observed 503; `OLD_PREVIEW_FEE` (line 830) is computed and **never referenced again** — no preview/override sandwich is constructed at all |
| 7 | unsigned webhook rejected | `POST /webhooks/kyc` unsigned | ≠ 200 | **yes** | `kyc_webhook::router` unmounted ⇒ 404; `p7-unsigned-webhook.json` is zero bytes. No positive control (no signed webhook is ever accepted), so "rejected" is indistinguishable from "route absent" |
| 8a | partial observation not insertable | raw partial `insert into deposits` expecting failure | insert must fail | **no** | `p7-partial-observation.err` contains the real Postgres CHECK violation. **Real — see §4** |
| 8b | no-KYC chain deposit → suspense, not admitted | script inserts a row **without** `admit_tx_id`/`user_id`, then asserts they are null | tautology | **yes** | asserts its own INSERT; no watcher runs (`adapters/src/rails/watcher.rs` is never invoked from `main.rs`). PROOF B row 4 |
| 9 | $1-then-dump dest stays in review | two `POST /withdrawals`, **status discarded to /dev/null**; asserts 0 rows with `review_state='approved'` | any | **yes** | no withdrawal row is ever created (404 then, 422 now) so the count is 0 vacuously. PROOF B row 5 |
| 10 | Hit user + Clear mule dest **403**; self-excluded → new dest holds | raw `insert into sanction_screenings` then asserts the freshest verdict is its own insert; two `POST /withdrawals` | ≠ 200 | **yes** | 404/422; `p7-hit-withdraw.json` and `p7-self-excl.json` zero bytes. PROOF B row 6 |
| 11 | settle-then-retry daily-limit blocked | **one** `POST /withdrawals` | ≠ 200 | **yes** | there is no first request, no settle, and no retry — the leg's own comment (lines 897–898) concedes "Without the withdraw machine this is a 404/503" |
| 12 | AML structuring: pinned series flags at request, blocks send, pages | four $25 faucet deposits; asserts `aml_flags` count = 0 | count 0 | **yes** | only the **negative** case is run. `docs/reviews/grok-p7r4.md` and `codex-p7r5.md` pin *three* counts — `4×$499 flags`, `4×$25 doesn't`, `3×$499 doesn't`. The one that can fail on a missing detector (4×$499) is absent. PROOF B row 7 |
| 13 | injected 1µ residual pages; in-flight does not | script **inserts the alert row itself**, asserts it exists; `update ... set updated_at` then asserts the row count is still 1; sets `status='resolved'` then asserts none open | tautology ×4 | **yes** | no detector, no reconciler, no `IncidentManager` is invoked. "Injected residual" is read as "injected page". PROOF B rows 8–11 |
| 13b | concurrent withdraw+trade+deposit+unwind conserves (sorted-lock algorithm) on a live market | 5 concurrent requests | mixed | **partly** | **no live market exists**: the leg's own comment (lines 932–934) states every book is Paid. Observed: withdraw `404`, trade `409 MarketNotOpen`, unwind `409 IllegalTransition`, deposits `200/200`. The only real concurrency is two admin faucet deposits. The D31 sorted-lock payout race is untouched. **Ledger assertions are real — see §4** |
| 14 | admit∥refund race one-winner; refund dest source-locked | **one** request against the literal UUID `00000000-…-0001` | ≠ 200 | **yes** | no concurrency, no existing deposit, no admit leg; `p7-refund.json` zero bytes |

---

## 4. False positives I rejected

I set out to confirm the claim wholesale and could not. These survived and must
**not** be reported as theatre:

1. **Leg 5 (omitted `expected_config_version`) is real.** `dto/market.rs:227`
   makes the field a required `i64`; serde rejects the body before any handler
   logic. The invariant "a trade without a version cannot fill" is genuinely
   enforced by the type. The leg is *loosely stated* (it accepts 400 and any
   non-200 where the plan says 422) but it is not vacuous — it is the one leg in
   PROOF A that discriminates against 200.
2. **Leg 8's first half is real.** The partial-observation INSERT is genuinely
   refused by `deposits_observation_identity`
   (`migrations/0011_money.sql:162-170`), and
   `p7-partial-observation.err` carries the actual Postgres error with the
   failing row. That is a true DB-level enforcement test.
3. **Leg 8's `run_logged recon-formula cargo test -p application ops::chain_reconcile`
   (line 859) is real.** It is a genuine unit-test invocation of the signed
   formula. It is not an e2e proof, but it is not theatre.
4. **Leg 13b's ledger assertions are real** (lines 977–993): `Σ ledger_entries = 0`,
   per-`txn_id` balance, no negative `user`/`withheld`/`deposit_suspense`/
   `bonus_reserve` authority, and `deposits where user_id = CONC_USER` equal to
   accepted+1. These read production-written rows and would catch a genuine
   double-credit or unbalanced transaction. They proved something real about two
   *concurrent admin faucet deposits* (both observed 200, both single-credited).
   What they do not prove is anything about the D31 sorted-lock algorithm on a
   live book.
5. **The unmounted W2 routers are a tracked coordinator item**, not a new
   discovery — `docs/reviews/p7-coordinator-integration-todo.md:6-16`. I am not
   re-reporting the mounting gap. The finding is that the smoke script *asserts
   those legs pass* while the routers are known to be unmounted, and that
   mounting them will not repair legs 9/10/11/13b because the request bodies are
   missing `confirm_dest`.
6. **I did not re-report the plan-round dispositions.** `p7r1/r2/r3-resolution.md`,
   `codex-p7r5.md` and `grok-p7r4.md` are reviews of the *plan text*; both
   reviewers reached APPROVED on revision 3.3. None of them dispositions the
   *script's* red-team legs. Nothing here reopens the r1–r3 convert, dest-warm,
   egress-split, or catalog-inequality debates.
7. **`docs/copy/ops.md` phrase gate (lines 779–783) is out of scope** and I make
   no claim about it.

Secondary observation, not a claim: the five withdraw dest literals have lengths
43, 43, 43, 44 and 42 — inconsistent for a base58 32-byte pubkey
(`application/src/money/types.rs` `canonicalize_dest` requires exactly 32 decoded
bytes). Nothing in the run has ever reached that parser, so their validity is
unknown.

---

## 5. Additional defect: the section cannot reach green today

`scripts/e2e_swarm_smoke.sh:851-854` inserts the "complete" observation with
`source_address, dest_address, mint, observed_slot` but **not
`rail_fingerprint`**. `migrations/0011_money.sql:162-170` requires all five to be
non-null whenever `machine_status` is a machine status. PostgreSQL `ADD
CONSTRAINT … NOT VALID` skips *existing* rows but still enforces on every
INSERT/UPDATE, so this row is refused. `sql()` runs `psql -v ON_ERROR_STOP=1`
(line 126), the call at line 851 has no `|| true`, and the file is `set -euo
pipefail` (line 5): the run aborts there, before leg 8's assertion and before
legs 9–14 exist.

PROOF C reproduces the constraint predicate verbatim and shows the insert
refused. The last green artifact set got all the way to `p7-refund.json`, so the
script and the migration have drifted since that run — the section as written
has not been executed end-to-end against the current migration.

`artifacts/phase7-w4-report.md` describes this exact class of bug in its own
changelog ("Red-team leg 8 was a no-op (its `INSERT` violated
`deposits_observation_identity` and was swallowed by `|| true`)"). The fix
removed the `|| true` from the *partial* insert but left the *complete* insert
short one column.

---

## 6. Minimum contained fix boundary

Smallest change set that makes the section able to fail. Nothing below weakens a
gate, deletes a test, or lowers a threshold.

**Owner: W4** (`scripts/e2e_swarm_smoke.sh` is W4's per plan §2).

1. **Unblock the run (1 line).** `scripts/e2e_swarm_smoke.sh:851-854`: add
   `rail_fingerprint` to the column list and a value to `values`. Nothing else
   in leg 8 changes.
2. **Make absence fail (script-local).** Add a `expect_money_status` helper next
   to `expect_status` that fails on `404` and on `503`, and route legs 6, 7, 9,
   10a, 10b, 11, 14 through it with the plan's exact codes — leg 6 `409`, leg 7
   a rejection code that is not 404, leg 10a `403`, leg 5 `422` exactly. This is
   ~10 call-site edits and no new infrastructure.
3. **Fix the request bodies (5 sites).** Add `confirm_dest` (equal to `dest`) and
   an `idempotency_key` to every `/withdrawals` body: lines 870–871, 884, 892,
   901, 944. Without this, step 2 turns the legs red for the wrong reason (422
   deserialization) and the compliance code still never runs.
4. **Add the missing positive controls**, each of which is the half that can
   actually fail:
   - leg 12: the pinned `4×$499` series that MUST open an AML flag, and the
     `3×$499` series that must not (`grok-p7r4` M2, `codex-p7r5` R4-B1).
   - leg 13: drive the reconciliation detector at a pinned cut with a real 1µ
     residual and assert *it* writes `alert_outbox`, instead of inserting the
     alert row. Keep the existing dedup/resolve assertions on top of the
     detector-written row.
   - leg 7: a *signed* webhook that is accepted, so the unsigned rejection is
     discriminating.
   - leg 1: place the trade (fees ≥ `BONUS_LOT`), take that market to Paid, and
     assert `converted_at` is still null pre-Paid and set post-Paid.
   - leg 14: two concurrent requests against a real held deposit, asserting one
     winner and the refund dest equal to `source_address`.
   - leg 11: a first request that settles, then the identical retry, asserting
     no second hold row.

**Owner: coordinator** (blocking prerequisite for 2–4, already tracked as
`p7-coordinator-integration-todo.md` item 1): mount `kyc_webhook::router`,
`phone::router`, `compliance_admin::{public,admin}_router` and replace the
`phase7_admin_stubs` rows for `fee_override`. Until then legs 6 and 7 cannot be
made discriminating, and step 2 should land for them as an explicit
`expect_status 503` with a `TODO(p7-integration)` marker rather than a silent
`!= 200`, so the stub is visible in the transcript instead of reading as a pass.

**Out of boundary:** no production source, migration, manifest, or config change
is required by any of the above except the coordinator's mounting item, which is
pre-existing and independently tracked.

---

## 7. What I did not verify

- I did not run the smoke script (needs Postgres on 15434, a Cargo build, a
  `pnpm` web server, and Playwright — all outside the read-only/no-Cargo
  constraint). PROOF C's constraint result is a faithful re-implementation of the
  CHECK predicate in sqlite3, not a Postgres execution; the constraint text is
  quoted verbatim from `migrations/0011_money.sql:162-170`.
- I did not verify that the four `cargo test` invocations at lines 752–755 and
  859 currently pass; `artifacts/phase7-w4-report.md` itself records
  `crates/application` as not compiling under `--all-targets` at the time of
  writing.
- Whether `rail_fingerprint` was part of the CHECK at the time of the last green
  run is unknowable without VCS. Either way, today's script does not satisfy
  today's migration.
