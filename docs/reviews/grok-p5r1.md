VERDICT: fix-first

Reviewed `docs/plans/phase5-content-engine.md` against spec §§1, 4.1, 6 (PnL cards), 8 (video pipeline), `docs/copy/scoring.md` (integrity framing), `docs/design-refs/notes.md` (Coming Soon rail), and current `web/` (no admin curator surface yet; LiveUpcomingRail exists as layout shell). Phase 5’s *direction* is right: draft → curator → slotted auto-publish through SeedMarket, deterministic template renderer + share cards, LLM behind ports with only-escalate moderation, module-split for parallel build. Product-mechanics gaps still leave the **cadence superiority claim under-specified as a human-fed queue**, and **public SVG share cards without an injection/CSP contract** — amend before the 5.1–5.2 wave.

## FINDINGS

[B1] **Cadence “machine” still has a human-shaped fuel line — not stated loudly enough for Spec §1.**  
Spec §1’s loudest superiority is **hourly flash + daily flagship**. Plan delivers slotting + `PublishDueDrafts` sweep (good), but draft *creation* is `POST /admin/drafts` from a topic seed (template or LLM). Nothing auto-refills the queue when empty; `max_pending_drafts` only caps backlog, not emptiness. Empty queue ⇒ empty slots ⇒ Coming Soon rail goes dark exactly when we claim “hourly.” Flash without a continuous topic/draft supply is a **curator treadmill**, not a machine — and the plan never names the ops SLA (e.g. “N approved flash drafts must sit ahead of the cadence” or “unfilled slot publishes a branded empty-state / skip”).  
**FIX:** Global Constraints + Task 5.1: (1) explicit supply model — *who* creates flash drafts at what rate (curator batch, template topic pool of size K, LLM batch job); (2) empty-slot behavior (skip + metric + curator notif vs. hold last “Coming Soon”); (3) full-queue / collision when two approved drafts map to the same `next_slot` (serialize, bump, or reject); (4) e2e or unit fixture for “queue empty at flash boundary does not invent a market.” Do not market “hourly machine” until this is product-true.

[B2] **Share-card / poster SVG is a public content surface without injection or serve-posture rules.**  
Share cards carry **real PnL** and market question text (user/curator-controlled) into deterministic SVG, served at `GET /assets/{job_id}.svg` and via `GET /users/{id}/share_card/{market_id}`. Unescaped `<`, `&`, quotes in question/handle break XML or enable XSS if the browser treats SVG as a document (inline, `image/svg+xml` navigation, or `<object>`). Plan pins brand palette and post-resolution privacy but **not** text escaping, Content-Type, `Content-Disposition`, CSP, or “serve only as `<img>` / never inline HTML.” Referral/handle fields on cards (spec §6) are also unstated.  
**FIX:** Task 5.2 contract: (1) all text runs XML-escape (and length-cap) at `RenderSpec` build; (2) response headers `Content-Type: image/svg+xml; charset=utf-8` + `Content-Disposition: inline` + long cache; (3) product rule — clients embed via `<img src>` only (no inline SVG HTML); (4) property tests: adversarial question `"</text><script>…"` produces non-executable SVG bytes; (5) pin share-card fields (handle? tier? referral code?).

[B3] **`publish_now` vs review integrity is underspecified for real-money markets.**  
Approve assigns `publish_at = next_slot`; `publish_now` “skip the slot — still via the sweep path semantics.” Missing: must status already be `approved`? Can `publish_now` publish a `pending` draft (bypassing review)? Do review `edits` apply only on approve, or can publish_now race unreviewed body text? A single bad question + LP seed moves real money (D1 rails).  
**FIX:** Pin state machine: `publish_now` allowed only from `approved` (or `pending` only with an explicit second confirmation that applies the same edit validation as approve); always run `DraftSpec::validate` on the final row under lock; concurrent `publish_now` + sweep share `draft:<id>:seed` idempotency (checkpoint 3) — add negative tests for pending→publish_now rejection.

[M1] **Per-tier market defaults (seed / windows / min_votes / fee) are not product-pinned.**  
Draft rows store all knobs, but plan never states launch defaults for `daily` vs `flash` (e.g. flash: shorter `open_secs`/`hidden_window_secs`, smaller `seed_micro`, lower `min_votes_to_resolve` — must still pass D21 floors). A curator typo (min_votes=1 on a flagship, or $1 seed) ships as-is after approve.  
**FIX:** `ContentConfig` or domain tables: default templates per tier; ReviewDraft merges edits over tier defaults; document floors that refuse approve (seed ≥ LP policy, min_votes ≥ integrity floor, hidden window ≤ open_secs).

[M2] **Slotting when queue is full / multiple drafts due — product UX gap.**  
`next_slot` is pure for one draft; two simultaneous approves can claim the same boundary. Sweep due-index doesn’t define order (created_at? tier priority daily over flash?). Full pending queue hits `max_pending_drafts` on create but not “too many approved waiting.”  
**FIX:** Define total order for due drafts; on slot collision bump to next free slot; surface schedule rail conflicts in 5.3; metric for “slots with zero approved drafts.”

[M3] **LLM draft fallback can silently mask broken config unless source is first-class in curator UI.**  
`source ∈ {template,llm}` is on the row — good. Create path “unavailable engine → clean error, callers may fall back with `fallback: bool`” can produce a **template** draft after the curator asked for LLM, with no required reason field.  
**FIX:** Always set `source` to the *actual* generator used; if fallback, set `source=template` and payload/meta `fallback_from=llm` (or force curator-visible banner in 5.3); never silently label template as llm. E2E already wants keyless fallback — assert UI/API exposes it.

[M4] **Flash = poster-only is scope-honest for §8 economics only if we stop implying video parity.**  
Plan: hero → `market_video` job, flash → poster only; Remotion/genAI deferred. Spec §1/§8 sell automation that *matches reels at 10× count*. SVG posters are a fine Phase 5 artifact — **if** product copy and curator docs say flash is card-first, video hot-swap is flagship-path / later vendor.  
**FIX:** One sentence in Global Constraints + `docs/copy/curation.md`: flash ships poster-only this phase; home rail still shows cadence; no false “video every hour” claim in UI.

[M5] **Parallel wave frays on frozen `error.rs`/`model.rs` and unowned Cargo deps.**  
Matrix freezes shared files; new content/video errors and `DraftId` newtypes likely live in frozen modules. Coordinator escalation is correct but **slow path under four parallel agents** — realistic jam. SVG/`resvg`/font deps: who edits `Cargo.toml`?  
**FIX:** Task 5.0 pre-declares: all content/video error variants, newtype IDs, outbox event string constants, and optional deps (svg stack) in frozen modules *before* the wave; Cargo.toml ownership = 5.0 or coordinator-only; wave workers open `ask` only for unplanned freezes — document max turnaround expectation.

[M6] **E2E proves one curated market loop, not the cadence weapon.**  
`e2e_content.sh` path (draft→edit→approve→sweep→render→attach→vote/trade→resolve→share + LLM fallback) is strong for the **machine path of a single market**. It does **not** prove hourly multi-publish, empty-queue behavior, slot collisions, publish_now races, or two-worker job claims.  
**FIX:** Add green markers for (a) two approved drafts → two distinct `publish_at` / markets, (b) empty queue at due time → zero SeedMarket, (c) publish_now idempotent vs sweep, (d) optional second job-worker claim test if multi-process is in scope — or explicitly defer cadence-load e2e with honesty in 5.5 text.

[M7] **Only-escalate LLM moderation is the right call; second-pass ordering needs a product note.**  
Deterministic screen first, LLM can only tighten (visible→shadow, never unshadow) — correct adversarial posture (model cannot launder spam). Residual: latency/timeout on LLM path must not block comment write (async escalate after insert vs sync reject) — **unstated**.  
**FIX:** Pin: post path stays deterministic-sync; LLM escalate is async outbox/job that may shadow after visible insert (author may briefly see public then shadow) **or** sync with timeout→no escalate. Choose one; test it.

[m1] **Hot-swap snapshot rehydration.**  
Attach emits WS frame; client connecting between publish and attach must see poster from snapshot. Plan checkpoint 7 asks this — Task 5.2 should require market snapshot/DTO asset ref fields (poster URL + optional video URL).  
**FIX:** Snapshot + REST detail carry `poster_asset` / `video_asset` nullable; attach updates both.

[m2] **LP / seed concentration from auto-publish.**  
Unlimited approved drafts + large default seeds can drain house LP faster than D23 kill-switch alone.  
**FIX:** Cap concurrent live flash markets or daily seed budget in ContentConfig; refuse approve when kill-switch already tripped (cross-link SeedMarket force rules).

## CHECKPOINTS (plan’s 8)

1. **Split safety:** Behavior-identical split + pre/post 100%×3 is the right bar. Frozen list is good for ports/fakes/routes/dto/mod; **misses `Cargo.toml`, possibly `lib.rs` module decls, and migration ordering if 5.1 adds 0007b while 5.0 owns 0007** — state 0007 sole owner = 5.0, 0007b data-only = 5.1 only. **Sound if 5.0 proof is mandatory before wave.**

2. **Ownership matrix:** Hidden shares are **error/model types and Cargo deps** (M5). Pre-declaration in 5.0 is required, not optional. Content vs video both need `Store` factory methods — pre-stubbed NotImplemented is correct.

3. **Publication idempotency:** `draft:<id>:seed|golive` through existing SeedMarket/AdvanceMarket is the right shape. Must also cover **publish_now ∥ sweep** and **double publish_now** (B3). Status transitions `approved→published` under draft row lock.

4. **Job worker:** Claim needs status CAS or `FOR UPDATE SKIP LOCKED` on `queued` rows; crash-replay of `rendering` must re-enter safely (timeout reclaim). Unique active job per (market, kind) — pin partial unique index. Two-worker test required for honest multi-process (or single-worker documented).

5. **Renderer determinism:** Spec must forbid HashMap iteration for z-order; use Vec runs; fixed fonts under repo path; no wall-clock in SVG; fixed decimal formatting for PnL. Byte-equality test is the right gate — list these constraints in `render_spec.rs` docs.

6. **LLM adapters:** Env-gate at **construction** (stated) is correct. Fixture realism without PII: hand-written JSON only. Only-escalate rule: **right product call** (M7 timing residual). Injection posture: body-as-data + fixture is testable **if** the system prompt is a versioned constant in-repo under test assertion.

7. **Hot-swap:** Attach frame + snapshot asset refs (m1). Placeholder at GoLive is correct per §4.1. Client test in 5.3 for frame without full reload — good.

8. **No-VCS parallel discipline:** Believable **only with** 5.0 pre-declarations (M5) and coordinator `ask` for freezes. Cargo `target/` contention is acceptable (one machine, serialized rustc). Four agents editing “only their tree” fails if anyone “just quickly” patches `routes/mod.rs` — CI should fail on freezes outside 5.0/5.5 (optional path grep in 5.5).

## WHAT CHECKS OUT

- Architecture: domain pure slotting/validation/render specs; application use cases; ports for DraftEngine/Renderer/Screen — matches standing hexagonal discipline.
- Curator approval as the real-money gate is the right control (needs B3 teeth on publish_now).
- Deterministic template renderer + env-gated genAI/LLM is the correct Phase 5 economics vs §8 full pipeline.
- Share cards post-resolution only continues Phase 4 vote-side privacy discipline for structured surfaces.
- Only-escalate moderation is the correct adversarial default for an untrusted model.
- E2E single-market loop is a solid *path* proof once cadence residuals are either tested or deferred honestly (M6).
- Deferral list (vendors, TTS/HLS, trend scrape, Remotion) is mostly scope-honest with M4 copy discipline.

## BUILD READ

Do not start the **5.1–5.4 wave** until plan text amends **B1–B3** (cadence supply/empty-slot, SVG serve+escape, publish_now state machine). Land **M1/M3/M5** (tier defaults, fallback visibility, 5.0 pre-declared errors/deps) in the same amend so parallel workers do not invent conflicting types. Task **5.0** can proceed on the pure split + migration + pre-wiring **including** pre-declared content/video errors and asset fields. Task **5.3** parallel is fine once admin draft DTOs are frozen post-amend.

---

FINAL VERDICT: fix-first
