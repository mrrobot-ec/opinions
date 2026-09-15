# Phase 7 review round 3 — coordinator resolutions

Inputs: `grok-p7r3.md` (FIX-FIRST, minimal delta: 1B/3M/3m), `codex-p7r3.md` (FIX-FIRST: 6B/10M/3m). Both deltas applied in place — the plan is now **revision 3.2**. Convergent items resolved once.

## Grok r3 (all four delta items applied)

| Finding | Disposition |
|---|---|
| B1 seeds violate own invariant | ACCEPTED — inequality now `min ≤ auto < dest_warm_floor` AND `auto ≤ dual ≤ max ≤ dest_daily ≤ daily ≤ hot_wallet`; seeds kept; 7.0a ships the seed-snapshot `validate_patch` Ok test. (Also closes codex B3.) |
| M1 shadow seed self-tell | ACCEPTED — shadow caps seeded to the published tier-0 numbers, noted MUST track them. |
| M2 structuring N undefined | ACCEPTED — `aml_structuring_threshold_micro` (seed $500, finance+2P); N counts only legs in (0, threshold) per user AND per dest; three pinned e2e counts (4×$499 flags; 4×$25 doesn't; 3×$499 doesn't). |
| M3 Paid-unwind vs D30 | ACCEPTED — D30 stands verbatim (never from Paid); Paid-unwind sentence deleted; §3.5 split into happy (Paid-convert) and void (unwind, no convert, not +G) legs; the only Paid compensation is a separately authorized correction command (also codex B1). |
| m1 convert in resolve tx | ACCEPTED — finalize allocations in resolve; convert fires lazily on the user's next `lock_user` tx. |
| m2 voided fees forfeited | ACCEPTED — stated: Voided never finalizes, no compensation outside D30 unwind; W3 must not finalize on Voided. |
| m3 SOL grief budget | ACCEPTED — bounded by hot-wallet cap / per-tx min; named in ops.md as accepted opex. |

## Codex r3

| Finding | Disposition |
|---|---|
| B1 Paid unwind impossible | ACCEPTED — as grok M3 row above; command matrix carries the never-from-Paid + correction-command sentence. |
| B2 reserve-at-qualification insolvency | ACCEPTED — reserve-at-grant: coverage = Σ remaining cash promise of every unconverted real_money lot; per-lot encumbrance atomic with issuance; release only on convert/stamped expiry/correction; qualified liability is reporting only; zero-reserve race tests. |
| B3 seed snapshot invalid | ACCEPTED — closed by the grok B1 inequality (validates against the seeds; dest_daily ≤ daily kept per grok's economic argument, satisfying codex's branch concern since the chain now holds). |
| B4 matrix unrepresentable | ACCEPTED — no fifth role: compliance duties map to finance (propose) + superadmin (confirm); generic `money_command_proposals` authority added to 0011 (payload hash, distinct tokens, `confirm_not_before`, `expires_at` = +15min window — never zero-width, caps, reason, replay key); single-principal exceptions = one atomic audit, no proposal; Phase 6 row filled by reference to D30's exact protocol; `withdraw_approve_daily_cap_micro` + `bonus_reserve_topup_daily_cap_micro` catalog rows added; unban row added; manual deposit admission row added; middleware.rs capability rows are 7.0a-owned. |
| B5 payout unprotected | ACCEPTED — `ResolveTx` += `UserLockGuard`; payout-recipient algorithm published (enumeration → sorted user locks → market lock → re-enum/retry → payout + oldest-first collection in-tx); SLO fallback = per-user encumbrance released on next `lock_user`; race matrix named with owners. |
| B6 deposit backfill impossible | ACCEPTED — grandfathering rule: `credited` → `admitted_legacy` (identity-4 carve-out, `txn_id → admit_tx_id`); `seen|confirmed` re-derived or quarantined; nullable `user_id` for unmatched inflows; upgrade tests for all three legacy states. |
| M1 transition table post-freeze | ACCEPTED — the full combination table is a 7.0a deliverable authored BEFORE 0011; migration CHECKs and W1's ops.md section derive from the same table. |
| M2 quorum placeholder | ACCEPTED — executable predicate: 2-of-3 independent archival RPCs, finalized-height reads > last_valid, signature-history absence (pruned ⇒ unknown), disagreement/timeout ⇒ unknown, evidence persisted on the attempt. |
| M3 unpinned rail identity | ACCEPTED — startup-validated immutable rail config (genesis hash, RPC set, mint pubkey, decimals 6, treasury token account, commitment, depth interpretation) fingerprinted into observations/payments; deposit depth and withdrawal finality unified as finalized-root measurement; wrong-cluster/mint/decimals tests. |
| M4 broadcast ≠ wallet debit | ACCEPTED — reconciliation subtracts only finalized-on-chain-not-yet-ledger-settled outbound at one pinned finalized cut; broadcast/unknown reported as exposure, never drift; observation slot/status persisted per term; four-state numeric tests. |
| M5 allocation algebra loose | ACCEPTED — unique (trade, lot, split_seq); ≤1 terminal child per source (finalized XOR reversed, unique on source); finalized moves provisional→finalized, never adds; lock order user → sorted lots → reserve; four named races Pg-tested. |
| M6 ownership gaps/collisions | ACCEPTED — middleware.rs capability rows to 7.0a; inbound watcher (`rails/watcher.rs`) to W3; PhoneVerification sandbox adapter + phone routes to W2; explicit 7.0a→W3 transfers for `ops/config.rs`, `pg/store.rs`, `fakes/state.rs` (not frozen); per-file fake/dto/test rows; payout race-test owners named. |
| M7 override unrevertable | ACCEPTED — typed enum `inherit|override(bps)` seed `inherit`; revert = write inherit; first-write Δ baseline = pool stamp; set→unset→replay tests. |
| M8 catalog omissions | ACCEPTED — rows added: `region_allowset` (content, not just version), `feature_referrals` (two-phase + prospective-snapshot precondition), `withdraw_approve_daily_cap_micro`, `bonus_reserve_topup_daily_cap_micro`; structuring threshold was grok M2; per-user/per-dest AML share thresholds stated. |
| M9 compliance_hold dead-end | ACCEPTED — reevaluation edges: hold → admission_pending on fresh facts; manual hold → admitted only via the command matrix with both records; history preserved; release-vs-refund race tested. |
| M10 phone uniqueness | ACCEPTED — unique verified normalized-number HMAC (key-version for rotation), one active challenge, atomic attempts, named adapter/routes/fakes, two-account race proves single grant eligibility. |
| m1 zero vs multiplicative Δ | ACCEPTED — zero = disable sentinel; re-enable baseline = seed; cross-key inequality suspended only for the disabled key. |
| m2 shared N_MIN constants | ACCEPTED — per-profile manifest-derived expected-series contracts, `observed ≥ expected` per profile; convoy assertion separated from thinness. |
| m3 "as rev 2" references | ACCEPTED — D31 identities (a)–(e), D33, D34 self-exclusion/limits, D35 alerter, and D36 restated inline; the plan is self-contained. |

**Net:** revision 3.2 is a specification pass — no architecture changes. Round 4 asks both reviewers for a delta verification and an APPROVED/FIX-FIRST verdict; the build wave dispatches on double-APPROVED.
