# Phase 5 plan — Round 2 VERIFY (Grok)

Verified rewritten `docs/plans/phase5-content-engine.md` + `docs/reviews/p5r1-resolution.md` against `docs/reviews/grok-p5r1.md` (and codex product-facing fixes). Findings-only; no implementation.

## Own findings (map)

| ID | Status | One line |
|---|---|---|
| **B1** cadence honesty | **VERIFIED** | Curator-fed supply stated loudly; unfilled slots **lapse** (no invented markets); `SlotUnfilled` + dashboard fill-rate; e2e empty-boundary case — product-true as an honest automation *of* a human queue, not a fake autonomous fountain. |
| **B2** SVG posture | **VERIFIED** | Escape-at-build on every text run; CSP `default-src 'none'` + nosniff + inline disposition; img-only product rule in 5.3; adversarial property test; fields pinned (handle/question/side/payout/return%; no referral this phase). |
| **B3** publish_now teeth | **VERIFIED** | Legal **only from `approved`** (pending → 409); edits only via review; final-row `DraftSpec` + floors under lock at approve **and** publish; one `PublishDraft` authority for sweep ∥ publish_now; saga crash stages + races tested. |
| **M1** tier defaults/floors | **VERIFIED** | `ContentConfig.tier_defaults` + refuse-approve floors (seed, min_votes ≥ D21, hidden < open); re-validated at publish. |
| **M2** slot reservation/order | **VERIFIED** | Integer slot math; `unique(tier, publish_at)` with bump-to-next-free / `NoSlotFree`; due order `(publish_at, created_at, id)`; schedule-rail conflicts in 5.3. |
| **M3** fallback visibility | **VERIFIED** | `source` = actual generator; `fallback_from` column + dashboard banner; e2e asserts visibility. |
| **M4** poster-first flash | **VERIFIED** | Constraints + `curation.md` + deferred list; no “video every hour” copy. |
| **M5** parallel/errors/deps | **VERIFIED** | 5.0 owns Cargo/lib/mod/errors/models/events/frames/0007/manifest; wave isolation + frozen sha256 at 5.5 — residual ask-latency noted under Codex product notes. |
| **M6** e2e cadence | **VERIFIED** | Two drafts → two slots/markets; empty boundary → zero markets + SlotUnfilled; publish_now ∥ sweep; two claimers; fallback visible — proves cadence *policy*, not only one happy path. |
| **M7** only-escalate timing | **VERIFIED** | Async third consumer decided; app-enforced visible→shadow only; brief-public-then-shadow **accepted and stated** (product-honest). |
| **m1** hot-swap snapshot | **VERIFIED** | `poster_asset_url`/`video_asset_url` on markets + snapshot/DTO + versioned `asset` frame; placeholder pre-attach. |
| **m2** seed budget / kill-switch | **VERIFIED** | Approve refused when kill-switch tripped + `daily_seed_budget_micro` cap. |

### B1 product judgment — is it product-true now?

Yes, as a **truthful** cadence product: the machine owns slotting, reservation, lapse, notification, and fill-rate observability; the human owns fuel and approval (real-money gate). Spec §1 “hourly flash” becomes “hourly *when the queue is fed*” — which matches ops reality and no longer overclaims autonomy. Residual ops practice (batch ahead of the rail) is implied by fill-rate, not a ship-blocker.

## Codex fixes as product matters

| Topic | Status | One line |
|---|---|---|
| Brief-public-then-shadow (async escalate) | **VERIFIED** | Right tradeoff: comment path stays fast/deterministic; model cannot launder spam; brief visibility is the cost and is named — acceptable for Phase 5 with only-tighten join in application code. |
| Share-card content addressing (right user) | **VERIFIED** | Cache key `(user, market, realization digest, spec version)` prevents cross-user PnL card reuse and wrong-user cache hits; post-resolution gate remains. |
| Wave ask-latency under manifest regime | **VERIFIED** as mitigated residual | Pre-declared contracts + 5.0-owned freezes make unplanned `ask`s rare; when they happen, stop-and-coordinator-remanifest is correct under no-VCS — still a schedule risk if 5.0 under-declares, not a product-logic hole. |

## NEW issues from the rewrite (cap 5)

[N1] **`SlotUnfilled` trigger needs a boundary clock, not only “empty due-set.”**  
Sweep with zero due drafts every tick could spam false `SlotUnfilled` or never fire if “flash boundary” is not defined as a discrete expected slot timestamp. Plan says “once per slot — dedupe by slot timestamp key” — good, but 5.1 must pin *how* the expected slot key is generated (e.g. on each flash boundary tick even when no draft is due).  
**FIX:** One sentence: publisher loop advances a per-tier “expected slot watermark”; missing approved claim at watermark → single `SlotUnfilled(slot_ts)`.

[N2] **`unique (tier, publish_at)` lifetime across terminal statuses.**  
If uniqueness is unconditional, a `published` (or `expired` after having been approved) row permanently occupies that slot key — fine for published; confirm rejected-never-slotted rows have `publish_at null`, and that re-approve after reject cannot collide with ancient published history (usually desired).  
**FIX:** Document uniqueness applies for all non-null `publish_at` (historical reservation) **or** use a partial unique on `status in ('approved','published')` — pick one and test reject/expire paths.

[N3] **Escalate latency SLA still unnamed (soft).**  
Brief-public-then-shadow is accepted; no max delay before shadow (seconds vs minutes) for product/support copy.  
**FIX:** Optional config `moderation_escalate_sla_secs` for metrics/alerts only — not a ship-blocker.

[N4] **Coming Soon rail UX when slots lapse.**  
Fill-rate is admin-facing; end users may see an empty rail during curator lag — coherent with honesty, but 5.3 should pin copy (“No upcoming markets” vs. recycled stale cards).  
**FIX:** One line in curation.md: never invent placeholders that look bookable.

[N5] **Share-card realization digest invalidation.**  
If a realization were ever corrected (ops void/rebuild — rare), cache could serve stale PnL until digest changes.  
**FIX:** Document digest inputs include realization id/version; acceptable for Phase 5.

## Checkpoints (R2 list) — skim

1. **Pure-move proof** — **sound** (inventory + num/den + symbols + cmp).  
2. **Matrix + manifest** — **sound** (complete 5.0 ownership; sha256 barrier).  
3. **Publication saga** — **sound** (stages, replay-id, one authority, SlotUnfilled with N1 pin).  
4. **Job leasing** — **sound** (short claim, CAS, reclaim, partial unique).  
5. **Renderer/serve** — **sound** (escape, CSP, traversal-proof, content-addressed cards).  
6. **LLM transport + only-tighten** — **sound**.  
7. **Hot-swap rehydration** — **sound**.  
8. **Wave isolation** — **sound** under stated dirs/DBs; ask path residual only.

## Residual product judgment

All R1 blocking product findings are **closed without regression**. The rewrite is more product-honest about cadence and more security-complete about public SVGs than R1. NEW items are implementation-precision (SlotUnfilled watermark, unique-index lifetime), not reopen of B1–B3. Parallel-wave ask latency is an ops/schedule residual, not a market-integrity hole, given 5.0 pre-declaration.

---

FINAL VERDICT: sound-to-build
