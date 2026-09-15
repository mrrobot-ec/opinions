# grok-p7r4 — delta verification (economics / abuse)

**Plan:** `docs/plans/phase7-money-compliance.md` revision 3.2
**Maps:** `docs/reviews/p7r3-resolution.md` (grok r3 1B/3M/3m + codex r3 applied)
**Prior:** `docs/reviews/grok-p7r3.md`
**Bar:** only genuine build-blockers; reopen test on closed exploits

## Verdict

**APPROVED**

The four r3 deltas are in the text and the seeds now validate. Codex’s additions do not restore a self-serve mint, dest hatch, or fee sandwich. 7.0a / W1–W4 may dispatch on a matching second APPROVED.

No new B/M/m.

## r3 delta — closed as written

| r3 | r3.2 evidence | Status |
|---|---|---|
| **B1** seeds vs invariant | `min ≤ auto < dest_warm_floor` AND `auto ≤ dual ≤ max ≤ dest_daily ≤ daily ≤ hot_wallet`; seeds unchanged ($5 / $50 / $100 / $500 / $1k / $1k / $2k / $10k); 7.0a `validate_patch(seed) == Ok` | **CLOSED** — arithmetic holds; auto still cannot warm |
| **M1** shadow $10 tell | `shadow_*_cap_micro` seed `25e6` (= published tier-0); MUST track | **CLOSED** |
| **M2** structuring N undefined | `aml_structuring_threshold_micro` seed `$500`; count only `(0, threshold)` per user and per dest; e2e 4×$499 / 4×$25 / 3×$499 | **CLOSED** |
| **M3** Paid-unwind / vacuous e2e | D30 never-from-Paid restated; Paid-unwind sentence gone; §3.5 = (a) Paid-convert (b) Void-unwind no convert; Paid fix = separate ADR’d correction | **CLOSED** |
| r3 m1–m3 | lazy convert on next `lock_user`; Voided never finalizes; SOL opex = hot-wallet/min | **CLOSED** |

## Codex additions — reopen check

Hunted, not blocking.

**Reserve-at-grant.** Encumbering at issuance means abandoned lots soak `BonusReserve` until convert / stamped expiry / correction. That refuses *new grants*, not earned converts (convert still cannot 422). Soak is bounded by `bonus_mint_daily_cap_micro` ($500) + OTP/device/instrument binds — the mint cap is the DoS cap. Sweeps lots do not encumber USDC. Not a mint; not a build-blocker.

**Payout encumbrance fallback.** Cash “never exposed unlocked”; release **and** collect on the next `lock_user` (withdraw is one). AML/structuring still see the later D31 withdraw, not the payout. Does not reopen the lien dodge if that hook is on every `lock_user` (W1 withdraw included), not only `PlaceTrade`. Implementation constraint, not a new door: the bucket must not be `Withheld` (identity a) or leftover `Escrow` after Paid (identity 5). Primary path (sorted locks in the payout tx) needs no new class.

**`admitted_legacy`.** Backfill only; watcher still books `External → DepositSuspense` for new inflows. Identity-4 carve-out is honest (no invented suspense). New money cannot skip KYC by wearing this status.

**`compliance_hold → admission_pending`.** Fires on fresh **Clear** facts, not on Hit. Hit/banned still cannot send (egress table). Manual `hold → admitted` is finance+superadmin, same grade as the frozen-funds license — not self-serve. Machine admission still requires sanctions Clear. B2 dest hatch stays closed (refund dest remains `source_address`; Hit source still never auto-refunds).

**Manual deposit admission row.** Dual-control, amount-capped, replayed on deposit id. Does not by itself pick a dest. Hit funds that are manually admitted become User cash and later leave only through D31 (warmth + AML + freeze-if-Hit). Acceptable named override.

**`inherit \| override(bps)`.** Revert is representable; first Δ vs pool stamp; set→unset e2e; drift still required to treat both directions as preview-relevant. Sandwich stays 409.

## Build read

Revision 3.2 is buildable under the existing 7.0a / W1–W4 matrix. Do not reopen r1–r3 convert, dest-warm, egress-split, or catalog-inequality fights. Residual implementation notes (encumbrance class if the SLO fallback is actually taken; admission predicate includes `users.status`) belong in 7.0a skeletons, not another plan round.
