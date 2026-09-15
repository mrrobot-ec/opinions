# Phase 6 plan — review round 3 resolution map

Revision 4 dispositions for codex-p6r3 (6 NEW BLOCKERs + PARTIAL carryovers) and grok-p6r3 (2 BLOCKER-level items + 2 MAJOR + residuals). All ACCEPTED except one explicitly accepted residual.

## codex-p6r3

| Finding | Disposition |
|---|---|
| NEW-1 (config_changes PK / hot history) | → D24: `config_generations(generation pk)` + `config_changes(pk(generation,key), index(key,generation))`, one row per changed key per patch; retention watermark with conservative `StaleConfig` below it; archived history for revert. |
| NEW-2 (stamped version unreachable; min_fee silent drift) | → D25a: `expected_config_version` in the public PlaceTrade contract (DTO/OpenAPI/pending/fingerprint); `dto/market.rs` + `routes/market.rs` W1-owned; `min_fee_bps` AND `fee_discount_bp_by_tier` classified market/user-relevant (effective-fee inputs always stale a confirmed quote). |
| NEW-3 (replay precedence) | → D25: pinned sequence — key hit ⇒ fingerprint compare ⇒ original receipt with NO pause/config checks; mismatch ⇒ 409 IdempotencyConflict; miss ⇒ locks → fences → authoritative reads → writes; four named contracts fake+Pg. |
| NEW-4 (publication command authority) | → D26: `publication_commands` schema (states, lease, attempts, result link, idempotency), 202 + status URL response, publisher tick consumes commands first under existing lease discipline, crash-after-authorize recovery contract; matrix expanded (create_draft/review_draft/publisher/publish_draft → W2; routes/social.rs → W2; routes/market.rs → W1; resolve_market.rs wholly 6.0a). |
| NEW-5 (receivable facts insufficient) | → D30: receivable authority row linked to unwind + origin reversal tx, append-only `receivable_movements` with actor/audit/cash-tx/idempotency, outstanding derived, per-reversal-tx reconciliation (identity 7 computable), write-off appends a compensating realization fact; collection/write-off files W2-owned. |
| NEW-6 (phantom withdrawal surface) | → D30: no HTTP withdrawal in Phase 6; guard ships as `WithdrawalEligibility` application role + fake/Pg contracts + read-only admin endpoint; e2e (b2) re-scoped to eligibility + deposit auto-collection. |
| r1#3 PARTIAL (catalog attribute gaps) | → D24: every key now carries bounds/class/role/max-delta (min_fee, rep thresholds, min-pot, seed budget, hold, sweep delay, cadence, flags, faucet caps, discount vector, remedial caps). |
| r1#6/#7 PARTIAL (Voided-unwind payout/dust/LP) | → D30: Voided unwind explicitly reverses the committed neutral-payout tx + dust legs with LP-PnL compensating realizations. |
| r1#13/NEW-6 carryover (matrix gaps) | → §2: all named omissions assigned; resolve_market.rs single-owner (6.0a); "one owner per file" restored. |

## grok-p6r3

| Finding | Disposition |
|---|---|
| N9/NEW-2 (discount vector absent — wash assert vacuous) | → D24 catalog + §3: `fee_discount_bp_by_tier = [0,0,10,20,30]` (published), finance+2P, monotone, ≥ min_fee floor; wash assert now has a 90bp counterfactual. |
| NEW-1 (roster isolation at 2k) | → D28 + §3: fat-pot voter set = ring k≥31 + ≤15 honest at EVERY scale; 2k load + 1k-voter settlement only on the lifecycle book; unique device/XFF per non-ring agent; e2e asserts Flag on the fat pot AND at-most-one-signal Pass on the lifecycle book. |
| NEW-4 (720s book not stampable; untimed 422) | → §3: five markets via explicit per-market stamps (global FLASH env only for the convoy book); young 422 attempted at closes−60, control young vote 200 at t=0. |
| NEW-5 (lien freezes collectible cash) | → D30: deposit auto-collection `min(cash, outstanding)` in the same tx; eligibility blocked only if outstanding remains; e2e deposit-$200-on-$50 → clear + house +$50; zero-cash debtor still blocked. |
| NEW-3 (auto-expiry dump window) | ACCEPTED AS RESIDUAL (it is D22 restored); optional hardening adopted: PlaceTrade/Preview 423 during a Live voting pause. |
| Residuals (remedial cap magnitudes, pad OI $50, convoy 2xx fraction, sweep_delay pin, ops.md in exit) | → D24 (per-market $500 / daily $2,000), §3 ($50 OI floor; ≥50% 2xx fraction; sweep_delay=180 named; ops.md barrier-gated in §3.7). |

## Round-3 verdict summary
codex: 8 RESOLVED / 5 PARTIAL / 2 UNRESOLVED → all carryovers now landed. grok: 12/12 r2 findings RESOLVED, 2 blockers + 2 majors new → all landed; NEW-3 accepted residual with hardening.
