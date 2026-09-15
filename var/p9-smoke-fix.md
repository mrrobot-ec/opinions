# P9 — Phase 7 smoke verification-theatre fix

Fixes the defects confirmed in `var/p9-smoke-refutation.md`. Scope was
`scripts/e2e_swarm_smoke.sh` plus a new `scripts/test_e2e_swarm_smoke.py`.
No crate, migration, web, service, other script, manifest, or docs file was
touched. No VCS command was run. No Cargo was run and no build directory was
created.

Pre-edit backup (mandatory, verified identical):

```
$ cp scripts/e2e_swarm_smoke.sh var/backup/scripts/e2e_swarm_smoke.sh
$ shasum -a 256 scripts/e2e_swarm_smoke.sh var/backup/scripts/e2e_swarm_smoke.sh
ab410cdd5fc2e7803ce11ab677675222a783ebc825d27cb59998a25a7efe6e9b  scripts/e2e_swarm_smoke.sh
ab410cdd5fc2e7803ce11ab677675222a783ebc825d27cb59998a25a7efe6e9b  var/backup/scripts/e2e_swarm_smoke.sh
```

---

## 1. Red first

`scripts/test_e2e_swarm_smoke.py` was written and run against the **unmodified**
script before any edit. It stays reproducible: the test reads `SMOKE_SCRIPT`, so
the pre-fix copy can be re-checked at any time.

```
$ SMOKE_SCRIPT=var/backup/scripts/e2e_swarm_smoke.sh \
    python3 -m unittest scripts/test_e2e_swarm_smoke.py
Ran 39 tests in 0.020s
FAILED (failures=33)
```

The 33 red assertions (full list in `var/p9-smoke-fix-red.txt`):

| Group | Red tests |
|---|---|
| `MoneyStatusHelper` | `test_helper_exists`, `test_unmounted_route_fails`, `test_stub_route_fails`, `test_wrong_status_fails`, `test_expected_status_passes`, `test_unmounted_is_rejected_even_when_expected_is_404` |
| `BroadPredicates` | `test_no_not_200_predicates`, `test_no_discarded_http_status` |
| `WithdrawalRequests` | `test_withdraw_helper_sends_confirm_dest`, `test_withdraw_helper_sends_idempotency_key`, `test_single_withdraw_helper_is_used`, `test_dest_literals_canonicalize_to_32_bytes`, `test_dest_literals_are_validated_in_script` |
| `ExactStatuses` | `test_omitted_config_version_is_exactly_422`, `test_unsigned_webhook_asserts_authentication_rejection`, `test_signed_webhook_positive_control_exists`, `test_webhook_body_is_well_formed` |
| `NoVacuousSql` | `test_no_self_inserted_alert_is_asserted`, `test_no_self_asserted_sanction_verdict`, `test_no_raw_referral_bind_inserts`, `test_no_observation_self_assertion` |
| `DepositObservationFixture` | `test_complete_observation_supplies_rail_fingerprint` |
| `PositiveControls` | `test_wash_after_grant_places_a_trade`, `test_wash_after_grant_asserts_both_sides_of_paid`, `test_wash_after_grant_proves_fee_volume`, `test_aml_has_in_band_positive_series`, `test_aml_positive_series_expects_an_open_flag`, `test_settle_then_retry_issues_two_requests`, `test_concurrency_runs_on_a_live_market`, `test_admit_refund_race_is_not_a_nil_uuid_probe` |
| `PrerequisiteReporting` | `test_prereq_missing_helper_exists`, `test_missing_prerequisites_fail_the_run`, `test_green_line_is_gated` |

Six tests were green from the start and stayed green: `bash -n`, and the four
`PreservedControls` tests plus `test_reconciliation_alert_is_detector_written`,
which pin the genuine controls the refutation said to keep.

The helper tests are behavioural, not textual: `run_helper()` extracts the shell
function from the script and executes it in a real `bash` with representative
status codes, so renaming the helper while loosening the predicate still fails.

---

## 2. Exact edits to `scripts/e2e_swarm_smoke.sh`

1000 → 1289 lines. Five new helpers, then the Phase 7 section rewritten leg by leg.

### 2.1 New helpers (inserted after `public_post`)

- **`expect_money_status <expected> <label> <actual> [body]`** — the single
  pass/fail decision for every money leg. `404` fails as "route is not mounted",
  `503` fails as "route is an unimplemented stub", `404`/`503` are also refused
  as an *expected* value, and anything other than the exact expected status
  fails. 27 call sites. Measured behaviour:

  ```
  actual=200  -> leg FAIL      actual=409  -> leg FAIL
  actual=403  -> leg PASS      actual=422  -> leg FAIL
  actual=404  -> leg FAIL      actual=503  -> leg FAIL
  (expect_money_status 403 probe <actual>)
  ```

- **`prereq_missing <text>` + `PREREQ_FAILURES` + the gate before the GREEN
  line** — an absent cross-owner surface is printed immediately and fails the
  run at the end. Deferred rather than immediate so every other leg still
  reports; the run is red either way and the GREEN banner is unreachable.
- **`route_is_live <label> <status>`** — returns non-zero for 404/503 after
  recording the prerequisite, so a stubbed route skips its assertions instead of
  satisfying them. 2 call sites (fee override, KYC webhook).
- **`assert_dest_canonical <dest> <label>`** — decodes base58 in `node` and
  requires exactly 32 bytes, matching `canonicalize_dest`
  (`application/src/money/types.rs`).
- **`withdraw_request <user> <amount> <dest> <key> <output>`** — the only way the
  script posts to `/withdrawals`. Always sends `confirm_dest` and a replay-stable
  `idempotency_key`, and canonicalises the dest first. 10 call sites; zero raw
  `"$BASE/withdrawals"` curls remain in the section.

### 2.2 Destination literals

`WithdrawRequestDto.confirm_dest` is a required `String`, so every previous
`/withdrawals` body died as an axum 422 before geo, sanctions, warmth, limits or
self-exclusion ran. Two of the five old literals were not canonical either:

| Old literal | Decodes to |
|---|---|
| `Dust111…` (43) | 32 bytes — valid |
| `Mule111…` (43) | **invalid base58** — `l` is not in the alphabet |
| `NewDest111…` (43) | 32 bytes — valid |
| `Dest1111…` (44) | 32 bytes — valid |
| `Conc111…` (42) | **31 bytes** — not a pubkey |

All destinations are now deterministic 32-byte values (sha256 of a stable label,
first byte forced non-zero so leading-zero handling cannot shorten the decode),
validated at runtime by `assert_dest_canonical` and independently re-decoded by
`test_dest_literals_canonicalize_to_32_bytes`.

### 2.3 Leg-by-leg

| Leg | Before | After |
|---|---|---|
| — | (no live book) | `P7_MARKET=$(create_market p7-live 420 60 …)` + two pad votes and a pad trade so it clears `min_votes_to_resolve` and stays above `OI_FLOOR_MICRO` |
| 1 wash-after-grant | preview only, status ignored via `\|\| echo`; asserted `converted_at is not null` count = 0 | votes, previews, asserts quoted fee ≥ lot, **places** the trade, asserts `credit_fee_allocations` rows exist (positive control), asserts no convert pre-Paid, and after `wait_until p7-live paid` triggers `lock_user` with a withdrawal and asserts `converted_at is not null` |
| 2–4 referrals | inserted `referral_binds`/`referral_codes` itself, then counted grants nothing could create | raw inserts removed; records the missing bind surface; asserts `referral_binds` is **empty** (a non-empty table means a fixture is standing in for the behaviour) and keeps the unverified-phone grant invariant, which `grant_referral_on_paid_in_tx` can now actually reach via the p7-live payout |
| 5 omitted version | `!= 422 && != 400` then `!= 200` | `expect_money_status 422`, plus a `grep` that the body names the missing field |
| 6 fee override | `!= 200` against a 503 stub; `OLD_PREVIEW_FEE` computed and never used | full D36 sandwich behind `route_is_live`: pre-override preview → propose 200 → same-principal confirm 403 → second-principal confirm 200 → fill at the stale version is 409 `StaleConfig` |
| 7 KYC webhook | `{"event":"kyc.complete"}` (no `event_id`, so 422 `InvalidWebhook` — the auth gate was never reached), asserted `!= 200` | well-formed body → unsigned is exactly **403** `AdminForbidden`; **signed positive control** (`x-kyc-signature` = lowercase HMAC-SHA256 hex of the exact body) is exactly 200 and must have advanced `users.kyc_tier` |
| 8 no-KYC observation | complete INSERT omitted `rail_fingerprint` → aborted the run; then asserted the null columns it had itself omitted | `rail_fingerprint` supplied; partial-rejection control kept **and tightened** to require the violation name `deposits_observation_identity` in the `.err`; self-assertion replaced by driving the admission machine — `admit/propose` on an unheld observation is exactly **409** and must leave `admit_tx_id` null |
| 9 dust warming | two statuses piped to `/dev/null`; counted approved rows that never existed | two real `withdraw_request`s at `$5` and `$90` (the old `$1` is below `withdraw_min_micro` and would refuse on `amount` before warmth ran), each exactly 200; asserts nothing approved **and** that both holds carry `dest_not_warm` |
| 10 Hit / self-exclusion | inserted a `sanction_screenings` Hit and asserted the verdict it had just inserted | removed entirely — `SandboxCompliance::plant_hit` has no route and `RequestWithdraw` re-screens through the provider on every request, so the inserted row was read by nothing. Two named prerequisites recorded |
| 11 settle-then-retry | one request, at an amount above `withdraw_max_micro`, `!= 200` | three legs to distinct dests exhaust the **user** daily cap: 200, 200, then exactly **429** with `refuse_code=limit`; the identical retry is 429 with `replayed=true` and mints **no** second hold; the same key at a different amount is exactly **409** `IdempotencyConflict` |
| 12 AML | four faucet **deposits** (nothing evaluates deposits) and a single "no flag" assertion | both pinned series on the **withdraw** path, which is the only one that calls `evaluate_withdraw_aml_candidate`: 4 × `$499` in `[floor, threshold)` must open a `rule='structuring'` flag (positive control), 4 × `$25` below the floor must not |
| 13 reconciliation | inserted its own `alert_outbox` row and asserted that row existed; `update … set updated_at` then asserted the count was still 1 | all self-inserts removed; asserts no `reconciliation_residual` incident is open on a clean run, and records the missing detector |
| 13b concurrency | raced a **Paid** book; withdraw 404, trade 409 `MarketNotOpen`, unwind 409, all accepted as `!= 200` | races the **live** p7-live book at its current generation after asserting `market_state_is p7-live live`: concurrent trade exactly 200, concurrent withdrawal exactly 200, two faucet deposits, and the unwind racer against the Paid lifecycle book exactly 409. All conservation assertions preserved verbatim |
| 14 admit vs refund | one request against the literal UUID `00000000-…-0001`, `!= 200` | real deposit id; `refund/propose` and `admit/propose` on a non-held deposit are each exactly **409**, and a curator token is exactly **403** (the money-command matrix runs before the state check). The genuine one-winner race is blocked on the watcher prerequisite |

### 2.4 Preserved controls (refutation §4)

Kept verbatim and pinned by `PreservedControls`:

1. the exact missing-`expected_config_version` rejection,
2. the partial-observation `deposits_observation_identity` refusal (now also
   asserting *which* constraint fired),
3. `Σ ledger_entries = 0`, per-`txn_id` balance, no negative
   `user`/`withheld`/`deposit_suspense`/`bonus_reserve` authority, and the
   concurrent faucet single-credit count,
4. `run_logged recon-formula cargo test -p application ops::chain_reconcile`.

---

## 3. Green

```
$ bash -n scripts/e2e_swarm_smoke.sh
(exit 0, no output)

$ python3 -m unittest scripts/test_e2e_swarm_smoke.py
Ran 39 tests in 0.048s

OK
```

The full smoke run and Cargo were **not** executed, per the task constraints.

---

## 4. Unresolved cross-owner prerequisites

These are recorded by `prereq_missing` inside the script. Until they land, the
run prints `PHASE 7 RED-TEAM INCOMPLETE`, lists them, and **fails** — the GREEN
banner is unreachable. That is deliberate: the previous script reported green
with these same surfaces absent.

| # | Prerequisite | Owner | Blocks |
|---|---|---|---|
| 1 | Mount `kyc_webhook::router` (and settle the webhook-secret env name; the script reads `KYC_WEBHOOK_SECRET`, defaulting to `p7-kyc-webhook-secret`, and seeds the `kyc_provider_ref:<ref>` config mapping) | coordinator, `p7-coordinator-integration-todo.md` item 1 | leg 7 unsigned-403 and the signed positive control |
| 2 | Replace the `phase7_admin_stubs` rows for `/admin/markets/{id}/fee_override/{propose,confirm}` with the real D36 handlers | coordinator / W4 (`ops/fee_override.rs` exists) | leg 6 sandwich → 409 `StaleConfig`. The script's confirm body is `{proposal_id, reason}`; reconcile with the real DTO when it lands |
| 3 | A mounted referral **bind** surface for `application::money::referrals::{BindReferral, BindReferralCode}` — `rg` finds no caller outside tests | W2/W3 | legs 2–4: A↔B-once, unverified-phone, third-device |
| 4 | Credit grant **issuance** (`/admin/credits/grant/{propose,confirm}` are 503 stubs) | coordinator / W3 | reserve-at-grant encumbrance; the lot in leg 1 is a seeded fixture today |
| 5 | Construct the inbound chain watcher (`adapters/src/rails/watcher.rs`) in `main.rs` | coordinator / W3 | a genuine `compliance_hold` deposit with suspense backing, hence the real admit∥refund one-winner race and the no-KYC suspense path |
| 6 | A staging-armed way to plant a sanctions Hit (`SandboxCompliance::plant_hit` is in-process only, and `RequestWithdraw` re-screens through the provider every request) | W2 / coordinator | leg 10 Hit-user → mule dest, Hit-source no auto-refund |
| 7 | Mount the self-exclusion surface (`start_self_exclusion`; `compliance_admin::public_router` is not merged) | W2 / coordinator | leg 10 self-excluded → new dest holds |
| 8 | A caller for `ops::chain_reconcile::residual_incident` — no scheduled cut runs in `main.rs`; only the alert *delivery* pump is spawned | W4 / coordinator | leg 13 "injected 1µ residual pages, in-flight does not" |
| 9 | Deposit-side AML evaluation — `credit_deposit.rs` contains no `aml` reference, so exit criterion 6's "deposits AND withdrawals" is half-covered | W3 | the deposit half of the structuring series (the withdraw half is implemented and now tested both ways) |

### Two contract questions for the lead

1. **Plan text vs implementation on a sanctions Hit.** Exit criterion 4 says
   "Hit user + Clear mule dest **403**". W1's implementation never refuses on
   sanctions: `admission_refusal` returns 403 only for `banned`/`kyc`/
   `geo_missing_ip`, while a Hit becomes the risk reason `sanctions_not_clear`
   on an accepted, review-required hold. There is no `sanctions` refuse code, so
   403 is currently unreachable at request time. I did not encode either reading —
   the leg is blocked on prerequisite 6 anyway — but the plan and W1 need
   reconciling before that leg is written.
2. **`self-excluded to new dest holds`** matches the implementation (accept +
   `self_exclusion_new_dest` reason), not a refusal; worth confirming when
   prerequisite 7 lands so the leg asserts the hold rather than a status.

### Runtime notes for the first full run

- `p7-live` is created with a 420 s open window because the section runs a
  `cargo test` (`recon-formula`) between the market's creation and the
  concurrency leg; the leg asserts `market_state_is p7-live live` before racing
  and fails loudly rather than silently degrading if the window is missed.
- The AML in-band series is `4 × $499 = $1996`, deliberately under
  `withdraw_daily_limit_micro` ($2000) and `aml_withdraw_velocity_micro_24h`
  ($5000), across four distinct dests so `dest_daily_limit_micro` ($1000) does
  not refuse a leg before AML evaluation. `evaluate_aml` counts the candidate
  itself, so the fourth leg reaches `n = 4`.
- `SandboxCompliance` is armed by the script's existing `OPINIONS_ENV=staging` +
  `STAGING_FAUCET=1`, so geo/sanctions return verdicts rather than
  `StoreError::Unavailable`; the withdraw legs therefore reach the real gates and
  the expected statuses above are the armed-sandbox contract.
