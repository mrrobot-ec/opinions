BLOCKED

## Round-3 finding verification

### Round-1 carryovers tracked in round 3

1. **RESOLVED — r1#1, config outbox ordering.** D24 retains the feasible generation-row serialization proof and independent missed-wake reconciliation proof.

2. **RESOLVED — r1#2, kill-switch linearization.** D25 retains the three fences, post-row-lock acquisition, one-way pause-writer graph, and the now-pinned idempotent-hit precedence.

3. **PARTIAL — r1#3, complete config catalog.** Revision 4 fixes `min_fee_bps` and adds the discount vector, but the claim that every key has bounds/class/role/max-delta is still false in the text. `rep tier thresholds`, `rep_score_min_pot_micro`, `payout_hold_threshold_micro`, cadence, feature flags, and faucet caps still have no stated max-delta; `sweep_delay_secs` repeats its absolute `[60,3600]` bounds rather than specifying a delta; and `daily_seed_budget_micro` states a delta but no absolute bound. The new discount-vector constraint is also internally inconsistent (NEW-1).

4. **RESOLVED — r1#4, atomic audit of existing admin mutations.** D26 now gives `publish_now` a durable command boundary, consumer, response/status contract, and recovery proof; the existing draft, market, and moderation mutation files are assigned.

5. **RESOLVED — r1#5, RBAC and faucet containment.** The fail-closed capability layer and two-factor staging mount remain explicit.

6. **RESOLVED — r1#6, unwind lifecycle/non-negativity.** D30 keeps receivables outside the cash ledger and now explicitly reverses a Voided market's neutral payout and dust.

7. **RESOLVED — r1#7, unwind authority/lineage/derived state.** The reversal authority and entry lineage remain one-transaction and unique; D30 adds the missing neutral-payout, dust, and LP-PnL repairs plus collection/write-off movement lineage.

8. **RESOLVED — r1#8, ledger-exact invariants.** The authority row, origin reversal transaction, append-only movements, linked cash transactions, and per-reversal reconciliation make identity 7 computable.

9. **RESOLVED — r1#10, replay determinism.** D28's manifest/opportunity/decision contracts are unchanged and buildable.

10. **RESOLVED — r1#11, deterministic mid-payout crash.** D29 retains the application port, one call site, production Noop, armed adapter, readiness protocol, and owners.

11. **UNRESOLVED — r1#13, exhaustive compiling ownership.** The Rust DTO/route files are now owned, but D25a also requires the converse pending action to retain `config_version`, send it as `expected_config_version`, and replace a stale pending with a new preview/new lexical confirmation. No owner is assigned to the required `services/converse/src/converse/{graph.py,core_client.py,api_models.py}` or their tests. Today `graph.py` stores only shares/fee/average price in the pending preview and `core_client.py` has no execute-side version argument. Thus the claimed exhaustive wave cannot implement exit criterion 3. The matrix also names “the deposit use case” without assigning its actual `credit_deposit.rs` path and assigns 6.0a placeholders in `fakes/*` while D30 separately says W2 owns the receivable fakes; those ownership transfers need to be made explicit.

### Round-2 NEW findings tracked in round 3

1. **RESOLVED — r2 NEW-1, fence convoy drain.** No regression.

2. **RESOLVED — r2 NEW-2, saga-aware audit.** The publication-command producer, first-in-tick consumer, leasing, 202/status response, and crash recovery are specified.

3. **RESOLVED — r2 NEW-3, receivables versus cash identities.** D27/D30 now describe a separate, append-only, reconcilable facts subledger.

4. **RESOLVED — r2 NEW-4, deterministic artifact contract.** No regression.

5. **RESOLVED — r2 NEW-5, crash-point seam.** No regression.

6. **UNRESOLVED — r2 NEW-6, exhaustive ownership matrix.** Same concrete converse and deposit/fake ownership gaps as r1#13 above.

7. **RESOLVED — r2 NEW-7, durable proposal authority.** No regression.

8. **RESOLVED — r2 NEW-8, fee preview policy and change history.** D24 now supplies atomic per-key history and D25a consistently preserves the live pool fee while selectively staling actual effective-economics changes. The invalid new discount validation is a separate revision-4 regression (NEW-1).

### Round-3 NEW findings

1. **RESOLVED — NEW-1, atomic config patch/history retention.** D24 has the generation header, `(generation,key)` rows, `(key,generation)` index, one row per changed key, watermark behavior, and archived revert history.

2. **PARTIAL — NEW-2, reachable preview generation/effective-fee drift.** The Rust public contract, DTO/route ownership, fingerprint, and relevant fee inputs are present. The required converse carrier and stale-pending behavior remain unowned, so one public execution path still cannot supply the mandatory field; see r1#13.

3. **RESOLVED — NEW-3, replay precedence.** Hit/fingerprint/original-receipt precedence and the four fake+Pg contracts are explicit.

4. **RESOLVED — NEW-4, publication command authority.** Schema, lease/retry/idempotency/result state, consumer, response contract, recovery test, and existing mutation owners are now present.

5. **RESOLVED — NEW-5, receivable authority and reconciliation.** The original data-model defect is closed by the authority row, linked reversal transaction, append-only movements, cash links, derived outstanding, per-reversal reconciliation, and compensating realization. The authorization of the newly executable write-off operation is a distinct new regression (NEW-2 below).

6. **RESOLVED — NEW-6, phantom withdrawal surface.** Phase 6 now exposes only `WithdrawalEligibility` plus fake/Pg contracts and the read-only admin endpoint; the e2e is correctly re-scoped to eligibility and deposit auto-collection.

## NEW findings

### NEW-1 — BLOCKER — D24's discount-vector validator rejects its own published seed

**Scenario:** D24 requires `fee_discount_bp_by_tier = [0,0,10,20,30]` and says each vector entry is “never < min_fee_bps.” The smoke pins `min_fee_bps = 10`. The tier-0 and tier-1 entries are therefore invalid (`0 < 10`). Because `SetConfig` validates the whole prospective snapshot, a literal implementation either rejects the published snapshot/the first unrelated patch, including the fee proposal exercised by exit criterion 3, or silently ignores an explicit catalog invariant. The existing fee authority also confirms the dimensional error: it floors the *effective fee* `max(base - discount[tier], min_fee)`, not the discount amount.

**Fix:** Delete the entry-versus-floor constraint. Require the computed effective fee for every tier to be at least `min_fee_bps` (the existing `effective_fee` formula already does this), and give the discount entries their own coherent bounds/max-delta. Keep `[0,0,10,20,30]` valid so the 100bp-not-90bp proof is executable.

### NEW-2 — BLOCKER — D26/D30 make receivable write-off a single-principal economic transfer

**Scenario:** D30 adds an executable write-off route that removes outstanding debt and appends a compensating realization, but specifies no proposer/confirmer authority, distinct principals, delay, or cap. D26's exhaustive list of dual-control endpoints omits receivable write-off. A single credential with the eventual route capability can therefore write off a $50 receivable; the user's next $200 deposit remains $200 instead of auto-collecting $50 to house, and the same action changes leaderboard economics. An audit records the transfer but does not prevent it. This is economically equivalent to a remedial credit up to the forgiven amount while bypassing remedial credit's dual control and caps.

**Fix:** Either remove write-off from Phase 6 or give it the same durable dual-control grade as unwind/remedial credit: immutable proposal, finance/superadmin distinct tokens, delay, reason, two atomic audits, idempotent confirmation, explicit caps, and a movement plus realization linked to the confirmed command. Add fake+Pg and RBAC e2e proofs that one principal cannot forgive the receivable.
