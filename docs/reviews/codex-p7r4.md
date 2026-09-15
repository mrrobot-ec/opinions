# Phase 7 plan — round 4 delta verification

## Verdict: FIX-FIRST

Revision 3.2 closes **18 of 19** round-3 findings; R3-M8 remains partial only because the required `region_allowset` migration seed is still not an actual value. I found one additional blocker introduced by the AML delta. The plan is otherwise buildable, and the minimal remaining delta is the two exact fixes below.

## Round-3 finding verification

| R3 finding | Status | Revision-3.2 evidence |
|---|---|---|
| **B1 — Paid unwind contradiction** | **CLOSED** | D30 is again never-from-Paid, conversion can occur only after Paid, and the exit test now separates Paid conversion from Voided unwind (`phase7-money-compliance.md:26,105,123`). |
| **B2 — reserve-at-qualification insolvency** | **CLOSED** | Coverage now reserves every remaining cash promise at lot issuance; qualification is reporting only (`:26`). |
| **B3 — invalid catalog seeds** | **CLOSED** | The revised inequality accepts the numeric withdrawal seeds and an exact-seed validator test is required (`:51–61,81`). |
| **B4 — unrepresentable command matrix** | **CLOSED** | Only D26 roles are used; generic `money_command_proposals`, non-zero expiry windows, single-principal exceptions, concrete cap keys, and middleware ownership are specified (`:85–105,109`). |
| **B5 — payout lien/lock race** | **CLOSED** | `ResolveTx` gains `UserLockGuard`; the stable sorted-user payout algorithm and in-transaction collection are explicit, with named race owners (`:21,109,114`). |
| **B6 — legacy deposit migration** | **CLOSED** | `admitted_legacy`, the identity carve-out, old `txn_id` mapping, re-derive/quarantine rules, nullable unmatched users, and upgrade cases are pinned (`:25,109`). |
| **M1 — transition table after migration freeze** | **CLOSED** | The combination table is a mandatory 7.0a artifact authored before 0011; both CHECKs and W1 documentation derive from it (`:14,109–110`). |
| **M2 — undefined non-landing quorum** | **CLOSED** | The predicate is 2-of-3 independent archival RPCs with finalized-height/history rules, persisted evidence, and fail-to-unknown behavior (`:19`). |
| **M3 — unpinned rail identity** | **CLOSED** | Startup config now pins/fingerprints cluster, endpoints, mint, decimals, treasury account, commitment, and finalized-root depth semantics (`:19`). |
| **M4 — broadcast treated as wallet debit** | **CLOSED** | Reconciliation subtracts only finalized-on-chain/unsettled outbound; broadcast/unknown is exposure, not drift (`:41`). |
| **M5 — loose allocation algebra** | **CLOSED** | Split uniqueness, terminal-child XOR, move-not-add finalization, exact-source reversal, user-first lock order, and races are stated (`:26`). |
| **M6 — ownership gaps/collisions** | **CLOSED** | Middleware, watcher, phone adapter/routes, explicit unfrozen transfers, and payout race owners are assigned (`:109–114`). Remaining path shorthand does not block the named integration surfaces. |
| **M7 — fee override cannot inherit again** | **CLOSED** | `inherit|override(bps)` makes unset/revert representable and defines the first-write delta baseline (`:44–45,79`). |
| **M8 — incomplete catalog** | **PARTIAL** | Referral, AML, approval/top-up cap, and allowset keys now exist (`:69–79`), but `region_allowset` still has the non-value seed “counsel launch set” while 0011 must seed and validate the exact snapshot. See R4-B2. |
| **M9 — deposit hold dead-end** | **CLOSED** | Machine reevaluation and dual-controlled manual-admission edges are explicit and raced against refund (`:25,99`). |
| **M10 — phone verification uniqueness/ownership** | **CLOSED** | Verified-number uniqueness, rotation, challenge/attempt rules, adapter/routes, durable bind, and the two-account race are named (`:27,111`). |
| **m1 — zero/multiplicative delta** | **CLOSED** | Zero is a disable sentinel; re-enable uses the seed baseline and disabled-key inequalities suspend narrowly (`:81`). |
| **m2 — one sample minimum for two profiles** | **CLOSED** | Smoke and full runs now derive separate exact expected series from their manifests (`:40`). |
| **m3 — overwritten normative references** | **CLOSED** | Revision 3.2 restates identities, compliance algebra, responsible-gaming rules, alert lifecycle, and fee override inline (`:3,20,29–45`). |

## Genuine remaining blockers

### R4-B1 — The AML predicate and its mandatory control test cannot both pass

**Evidence.** D34 says a structuring leg is any amount in `(0, aml_structuring_threshold_micro)`, with `N=4` and a $500 seed, then requires four $25 deposits **not** to flag while four $499 withdrawals do (`docs/plans/phase7-money-compliance.md:35,70–71`). Twenty-five dollars is strictly inside `(0,$500)`, so the written predicate counts all four.

**Build failure.** A literal detector flags the $25 control and fails the required e2e. Excluding the control requires inventing an unstated lower edge for the “threshold band,” so W2 and the simulator cannot implement the same rule from the plan.

**Minimal resolution.** Add an exact lower bound, for example `aml_structuring_floor_micro`, enforce `0 < floor < threshold`, and count amounts in `[floor, threshold)`; seed it below $499 but above $25 and add it to the catalog/snapshot. Alternatively define a fixed near-threshold ratio with exact rounding. Keep the three stated tests against that executable interval.

### R4-B2 — The exact seed snapshot still contains a legal placeholder, not JSON data

**Evidence.** `region_allowset` is a non-empty JSON array but its Seed cell is “counsel launch set” (`phase7-money-compliance.md:74`). 0011 must install catalog seeds and prove the **exact** seed snapshot validates (`:81,109`), while counsel-owned launch content is explicitly an external gate rather than a build input (`:3`).

**Build failure.** A deterministic SQL migration cannot insert “counsel launch set” as a valid non-empty state-code array. Guessing states makes a legal/product decision; omitting the row contradicts the stated seeded snapshot unless absence is explicitly the seed. This blocks 0011 and its required test independently of whether the runtime validator itself is sound.

**Minimal resolution.** Make the seed explicitly `absent (deny-all)`—consistent with D33's missing-policy rule—and have staging setup apply a pinned non-production allowset before money-path tests; production remains gated on the counsel-supplied proposal. Alternatively pin a concrete sandbox-only array and a startup rule forbidding that seed in production. Update the exact-seed test accordingly.

Once these two predicates are made executable, my verdict becomes **APPROVED** without another architecture change.
