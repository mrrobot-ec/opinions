# Opinions — Phase 5 (Content Engine) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. **This revision integrates the full P5R1 fix set (codex BLOCKED + grok fix-first; see docs/reviews/p5r1-resolution.md).**

**Goal:** The market supply chain becomes a machine with an honestly-named human fuel line: curator-fed drafts (template engine + env-gated LLM engine) → review/edit/approve with per-tier defaults and floors → **slot-reserved scheduled publication** through a crash-recoverable saga → **leased video/poster jobs** rendering deterministic branded artifacts with atomic persistence and hot-swap → on-demand share cards (content-addressed, injection-safe) → async LLM moderation escalation behind the proven cursor pattern.

**Architecture:** unchanged — plus the parallel-wave mechanism, now hardened: Task 5.0 freezes **all cross-worker contracts as compiling, behavior-tested code**, completes the ownership matrix, and records a **frozen-file hash manifest** (the no-VCS substitute for a freeze-violation CI check); the wave runs with **per-worker build isolation** and scoped gates; the authoritative workspace gates run only at the 5.5 barrier.

## Global Constraints (Phase 5 deltas)

Everything standing binds (no VCS ever; 100% ×3 with empty allowlist at the barriers; TDD; contract suites fake+Pg; global lock order; typed fail-fast configs; artifacts twice+`cmp`).

- **No external network in any test or gate.** LLM/genAI adapters take a **mandatory injected transport**; the only real-HTTP constructor lives in one audited module (`crates/adapters/src/llm/transport.rs`); tests use a deny/assert transport. The deps-check grep covers HTTP-client construction outside that module (not just two hostname spellings — codex M5).
- **Cadence honesty (grok B1):** the machine automates slotting/publication/rendering; **draft supply is curator-fed** (topic-pool batch creation is one click, but a human approves every market — that is the real-money gate and stays). **An unfilled slot lapses: no market is ever invented without approval.** Slot-fill rate is a dashboard metric and an unfilled slot emits `SlotUnfilled` → admin notification. Product copy never claims "video every hour": **flash tier is poster-first this phase** (grok M4), stated in `docs/copy/curation.md`.
- Config: `ContentConfig { flash_cadence_secs (>0), daily_slots (>0, integer slot arithmetic pinned in 5.1), draft_ttl_secs (>0), max_pending_drafts (≥1), max_slot_horizon_secs (>0), daily_seed_budget_micro (>0 — grok m2), video_max_attempts (≥1), lease_secs (>0), backoff_base_secs (>0), render_dir (validated at startup), tier_defaults: per-tier {open_secs, hidden_window_secs, seed_micro, fee_bps, min_votes_to_resolve} with FLOORS that refuse approve (seed ≥ floor, min_votes ≥ D21 floor, hidden_window < open — grok M1) }` — fail-fast validated.
- **File-ownership matrix (complete per codex B1/grok M5):** Task 5.0 exclusively owns and pre-writes: `Cargo.toml`/`Cargo.lock` (all wave deps added up front), every `lib.rs`/`mod.rs`, `migrations/0007_content.sql` (ALL schema incl. the job uniqueness index — no 0007b exists), `crates/adapters/src/pg/{mod,store,rows}.rs`, `fakes/{mod,state}.rs`, `error.rs`/`model.rs` (all new variants + newtypes pre-declared: `DraftId`, `JobId`, `StoreError::Unavailable(&'static str)`, content/video `AppError` variants), outbox event name constants, WS `asset` frame type, `MarketSnapshot`/DTO asset fields, `justfile`, renderer fixture dirs. During the wave these are **frozen** — and the frozen set explicitly includes `crates/main/src/main.rs`, `crates/application/src/seed_market.rs` (5.0 touches both), and the manifest tooling itself (codex P5R2 N1). 5.0 emits `scripts/frozen_manifest.txt` (a **sorted path inventory**) + `scripts/frozen_manifest.sha256` (per-file digests over exactly that inventory); **5.5 first compares the path inventory (an added/removed target fails before any digest check), then the digests.** Coordinator-mediated changes are re-manifested by the coordinator only; a worker needing a frozen change stops and `ask`s (barriered, not concurrent).
- **Wave build isolation (codex M1):** each wave worker uses its own `CARGO_TARGET_DIR=target-w<N>` and its own database `opinions_w<N>` (created from migrations at task start); workers run **scoped** tests only (`cargo test -p <crate> <module>`); no worker runs workspace coverage/fmt/clippy during the wave — those are 5.5 barrier gates. (Wave workers still TDD; the barrier proves the union.)

**Worker protocol:** 5.0 solo → **5.1 ∥ 5.2 ∥ 5.3 ∥ 5.4** (four workers, disjoint by the matrix) → 5.5 solo barrier.

**Build status (exit-checked 2026-08-12):**

- [x] Task 5.0 — contract freeze + compiling skeleton
- [x] Task 5.1 — curation core
- [x] Task 5.2 — video/render engine
- [x] Task 5.3 — web surfaces
- [x] Task 5.4 — LLM adapters + moderation escalation
- [x] Task 5.5 — union barrier, deterministic artifacts, and content E2E

| Worker | Owns during the wave |
|---|---|
| 5.1 curation | `crates/domain/src/drafting.rs`, `crates/application/src/content/**`, `ports/content.rs`, `fakes/content.rs`, `contract/content.rs`, `pg/content_tx.rs`, `http/routes/content.rs`, `http/dto/content.rs` |
| 5.2 video/render | `crates/domain/src/render_spec.rs`, `crates/application/src/video/**`, `ports/video.rs`, `fakes/video.rs`, `contract/video.rs`, `pg/video_tx.rs`, `crates/adapters/src/render/**`, `http/routes/video.rs`, `http/dto/video.rs` |
| 5.3 web | `web/**`, `docs/copy/curation.md` |
| 5.4 LLM | `crates/adapters/src/llm/**`, `crates/application/src/moderation_escalate.rs` (pre-declared home) |

---

### Task 5.0: Contract freeze + skeleton (solo; the wave cannot start until this is green)

1. **Pure module split** of `ports.rs`/`fakes.rs`/`contract.rs`/`routes.rs`/`dto.rs` into per-area files with re-exporting mods. **Proof of pure move (codex M1):** before/after — identical test inventory (names+count), identical per-crate coverage **numerator and denominator**, and an identical sorted `pub fn|struct|enum|trait` symbol manifest (recorded to `scripts/split_manifest_{before,after}.txt`, compared with `cmp`). This checkpoint completes before ANY new code.
2. **Migration 0007** (sole schema owner): `market_drafts` as R1 draft PLUS `publish_stage text check in ('claimed','seeded','live','jobs_enqueued','published')`, `published_market_id` (pre-generated at claim), `expires_at timestamptz not null` (persisted at creation — config changes never move old expiries, codex M2), `fallback_from text` (grok M3), `unique (tier, publish_at)` (slot reservation — grok M2); `video_jobs` gains `available_at`, `claim_token uuid`, `lease_expires_at`, `attempts`, `error`, `updated_at`, kind check **without** share_card (share cards are not jobs — codex B5), partial unique `(market_id, kind) where status in ('queued','rendering','ready')` (codex B4), **and the durable issuance key `unique (draft_id, kind) where draft_id is not null` (codex P5R2 N2 — idempotent across ALL statuses: a job that attached or failed before the saga persisted `jobs_enqueued` can never be re-issued as a second canonical job; the crash test sits exactly on that gap)**; `markets` gains `poster_asset_url`/`video_asset_url` (codex B5/grok m1); **`moderation_jobs`** (codex P5R2 N5: `id, comment_id unique references comments, status queued|running|done|failed, available_at, claim_token, lease_expires_at, attempts, error, created_at, updated_at`); `outbox_cursors` seeded with `('moderation', 0)`; outbox event constants reserved incl. `SlotUnfilled`.
3. **Compiling skeleton, coverage-clean (codex B2):** final trait signatures for `DraftEngine`, `Renderer`, `ModerationPreflight`, `ContentTx`/`VideoTx` (+ Store factories); every placeholder returns the typed `StoreError::Unavailable("phase5:<area>")` and **every placeholder branch has a behavior-asserting test** (the 100% gate holds at the end of 5.0 — verified by running the full barrier gates once here); `main.rs` pre-wires the publisher sweep, job worker, and moderation-escalate consumer behind config flags with disabled/unavailable startup tests; `SeedMarket` gains the **replay-id fix** (returns the STORED market id from its idempotency record on replay, never echoing caller input — codex B3; regression test).
4. Record `scripts/frozen_manifest.sha256`. Gates: the FULL standing barrier set, green.

---

### Task 5.1: Curation core (wave)

- **Domain slotting, integer-pinned (codex M2):** day partitioned into `daily_slots` intervals of `floor(86400/slots)`s (remainder absorbed by the last slot); `next_slot` returns the **next strictly-after boundary** (a timestamp exactly on a boundary yields the following slot — pinned); flash slots = next `flash_cadence_secs` boundary strictly after now; total order for due drafts = `(publish_at, created_at, id)`.
- **Slot reservation:** approve assigns the next **unoccupied** slot within `max_slot_horizon_secs` via the `unique (tier, publish_at)` index (collision → bump to next free; none free in horizon → `ApproveError::NoSlotFree`, curator sees it). Approve also refuses when the LP kill-switch is tripped (reuses the Phase 3 check — grok m2) and when the day's published seed total would exceed `daily_seed_budget_micro`.
- **Review state machine (grok B3):** `publish_now` is legal **only from `approved`** (pending → 409 with copy; edits apply only through review; the final row re-validates `DraftSpec` + tier floors under the draft row lock at approve AND at publish). `CreateDraft` supports single + batch-from-topic-pool; `source` records the **actual** generator, `fallback_from='llm'` set when the engine was unavailable and the caller allowed fallback (never silently labeled — grok M3); backpressure admission under an advisory lock (count-then-insert race closed — codex M2).
- **Expiry authority (codex P5R2 N4):** exactly one draft-locked transition — `pending && now >= expires_at → expired` — implemented as a function used by BOTH the review path (an approve racing expiry loses to whichever takes the row lock first; the loser sees the new status and errors cleanly) and a bounded expiry sweep arm in the scheduler (`LIMIT`ed, ordered by `expires_at`). Race test: approve ∥ expiry sweep on the boundary row.
- **Budget reservation (codex P5R2 N4):** the daily seed budget is checked under a **day-scoped advisory lock** (class 3, key = UTC day) at APPROVE time, counting every reservation for that day — approved-and-slotted (unpublished) + in-flight sagas + published — not just published; two concurrent approvals cannot jointly exceed the cap (concurrency test).
- **Publication saga (codex B3):** `PublishDraft(draft_id)` — claim (row lock, status `approved`, stage `claimed`, `published_market_id` persisted if null) → `SeedMarket(market_id = published_market_id, key draft:<id>:seed)` → stage `seeded` → `AdvanceMarket(GoLive, key draft:<id>:golive)` → stage `live` → enqueue jobs (idempotent per the 0007 unique; flash: poster only; daily: poster + market_video) → stage `jobs_enqueued` → status `published` + `DraftPublished` event → stage `published`. **Every stage transition is its own committed step; resume re-enters at the persisted stage.** Sweep (`PublishDueDrafts`, ordered, batch-bounded) and `publish_now` both call this one authority. Empty due-set at a flash boundary → `SlotUnfilled` event (once per slot — dedupe by slot timestamp key) and nothing else (grok B1). Crash tests at every stage boundary; sweep∥sweep, sweep∥publish_now, double publish_now.
- HTTP + DTOs in owned files; tests fake+Pg in owned contract file; scoped gates + own DB/target dir.

---

### Task 5.2: Video/render engine (wave)

- **Job leasing (codex B4):** claim = short tx: `SELECT … WHERE status='queued' AND available_at <= now() ORDER BY updated_at LIMIT k FOR UPDATE SKIP LOCKED` → set `status='rendering', claim_token=uuid, lease_expires_at=now()+lease_secs, attempts=attempts+1` → COMMIT. Render happens **outside any transaction**. Complete = CAS `UPDATE … SET status='ready', asset path … WHERE id=$1 AND claim_token=$2 AND status='rendering'` (zero rows → a newer attempt owns it; drop silently). **Error completion is token-fenced the same way (codex P5R2 N3):** a known render failure CASes on `(id, claim_token, 'rendering')` → requeue with the pinned backoff and **cleared lease + token**, or → `failed` when `attempts >= video_max_attempts` (the `>=` boundary makes `max=1` mean exactly one attempt); expired-lease reclaim applies the identical `>=` boundary and clears lease/token. Backoff: `available_at = now() + backoff_base_secs * 2^min(attempts,8)` (saturating). Tests: two claimers, crash before/after render, stale completion dropped, expired reclaim, backoff boundaries, terminal failure, byte-identical retry.
- **Atomic artifacts (codex M4):** filename derived ONLY from the job UUID + kind; write temp file under the canonicalized `render_dir`, fsync, atomic rename; serving (`GET /assets/{job_id}.svg`) resolves the JOB by id and status (`ready|attached`) — never a path input; headers `Content-Type: image/svg+xml; charset=utf-8`, `X-Content-Type-Options: nosniff`, `Content-Security-Policy: default-src 'none'`, `Content-Disposition: inline`; immutable cache only for finalized artifacts. Traversal/symlink/partial-write tests.
- **Deterministic renderer (codex M3 + grok B2):** `RenderSpec` v1 — `Vec`-ordered runs (no maps anywhere), integer geometry, literal color strings, our own money formatter (fixed decimals), **our own XML escape applied to every text run at spec build** (adversarial `"</text><script>"` question renders as inert escaped bytes — property test), fixed char-count wrapping, no wall clock, attribute order = template literal order, `\n` newlines. **Byte identity is the contract; rasterized appearance may vary by viewer fonts (generic families referenced, nothing embedded) — stated.** Golden byte+digest tests across fresh renderer instances.
- **Hot-swap (codex B5):** `AttachReady` sets `markets.poster_asset_url`/`video_asset_url` under the market row lock, emits `VideoAttached` → versioned WS `asset` frame `{v:1, outbox_seq, market_id, kind, url}`; `market_snapshot` and REST list/detail carry both nullable fields (a client connecting pre-attach sees nulls → branded placeholder; post-attach rehydrates from snapshot).
- **Share cards (codex B5 + grok B2):** NOT jobs — `GET /users/{id}/share_card/{market_id}` renders on demand post-resolution only, cached content-addressed (`sha256(user, market, realization digest, spec version)` filename in a `cards/` subroot); same escape/serve posture; fields pinned: handle, question, side, payout, return %, our brand mark — no referral code this phase (deferral stated).

---

### Task 5.3: Web (wave; owns `web/**` + `docs/copy/curation.md`)

As R1 draft plus: slot-fill metric + `SlotUnfilled` admin notification surfaced on the dashboard; draft rows show `source` and a fallback banner when `fallback_from` is set (grok M3); schedule rail shows reserved slots + conflicts; poster-first copy for flash tier (grok M4); share-card button embeds via `<img src>` **only** (the stated product rule); hot-swap on the `asset` frame + snapshot rehydration test. Curator strings in `docs/copy/curation.md`; no lorem. `pnpm build` + `pnpm test` green (own scoped gates).

---

### Task 5.4: LLM adapters + async moderation escalation (wave)

- **Adapters (codex M5):** `LlmDraftEngine` + `LlmModerationPreflight` over the injected `HttpTransport`; the sole real transport in `llm/transport.rs` (audited module); env validation all-or-none (`LLM_API_KEY` + `LLM_BASE_URL` — half-configured → typed startup config error); deny-transport tests assert the **exact outbound request** (method, URL, headers with redacted key, versioned prompt constant, delimiters, body bytes) before returning handwritten fixtures (valid, invalid-JSON, refusal, over-length, injection-shaped).
- **Moderation escalation — two-stage durable protocol (codex P5R2 N5; supersedes the single-cursor sketch):** comment posting stays deterministic-sync (Phase 4 path untouched). **Stage 1** — the `moderation` cursor consumer does ONLY what the proven cursor pattern allows atomically: cursor lock → read `CommentPosted` events → **materialize idempotent `moderation_jobs` rows** (0007 table: `comment_id unique`, status queued, the same `available_at/claim_token/lease_expires_at/attempts` columns) → advance cursor, one transaction, zero remote calls. **Stage 2** — a leased job runner (the SAME leasing machinery as video jobs: short SKIP-LOCKED claim, remote preflight outside any lock/tx, token-fenced CAS completion incl. errors, `>=` attempts boundary) applies `EscalateComment` on a Shadow verdict (comment row lock; **visible→shadow only — never unshadow, never Blocked**; the severity join is application code). Unavailable engine → jobs sit queued (env-gated runner). Brief-public-then-shadow accepted and stated. Tests: cursor-stage atomicity (crash between materialize and advance replays; unique absorbs), two runners claim disjoint jobs, only-tighten enforcement, unavailable idle, injection fixture cannot flip a verdict.

---

### Task 5.5: Phase 5 exit barrier (solo)

- [x] **Frozen-manifest verification** (`scripts/frozen_manifest.sha256` — any unmediated drift fails the phase). Then full standing gates on the union (fresh DB 0001–0007, fmt/clippy/test/coverage 100%×3/deps-check incl. the transport grep/gate-test; converse pytest; web build+test; OpenAPI artifacts regenerated once, twice+`cmp`).
- [x] `scripts/e2e_content.sh` (grok M6 additions included): template draft → curator edit+approve (slot reserved) → sweep publishes via the saga (poster placeholder live) → poster job leases/renders/attaches (WS `asset` frame observed; snapshot rehydration checked with a second probe connect) → vote+trade on the machine-made market → resolve → share card renders with real PnL (and the adversarial-question card is inert) → **two approved drafts get two distinct slots/markets; empty queue at a flash boundary publishes nothing and emits one `SlotUnfilled`; publish_now ∥ sweep converge on one market; two job workers claim disjoint jobs; keyless LLM draft falls back with `fallback_from` visible**. Poll-with-deadline; `PHASE 5 E2E GREEN`.
- [x] Docs sync: plan checkboxes, PLAN.md row, README status.

## Deferred (explicit)

GenAI video/TTS/HLS vendors (ports ship; keys+vendor choice are ops), trend ingestion, Remotion/ffmpeg motion pipeline, referral codes on share cards, LLM live-call evals in CI (manual with keys), mid-market LP pause (Phase 6 control plane, restated).

## Reviewer checkpoints (R2 verifies specifically)

1. 5.0's pure-move proof (inventory + numerator/denominator + symbol manifest) and the skeleton's coverage-cleanliness mechanism.
2. Ownership matrix completeness NOW (Cargo/lib/mod/pg-core/fakes-core/errors/models/events/frames/justfile/fixtures all 5.0-owned; no 0007b; hash-manifest policing).
3. The publication saga's stage machine (resume-at-stage, replay-id fix in SeedMarket, one authority for sweep/publish_now, SlotUnfilled dedupe).
4. Job leasing (short claim tx, CAS complete, reclaim, saturating backoff, the partial unique's exact status set).
5. Renderer/serve posture (escape-at-build, byte-identity scope honestly stated, CSP/nosniff/inline, traversal-proof serving, share-card content addressing).
6. LLM: transport confinement + request-shape assertions + all-or-none env + async escalation's only-tighten join in application code.
7. Hot-swap rehydration (snapshot + DTO fields + frame versioning) and placeholder semantics.
8. Wave isolation (per-worker target dirs + DBs, scoped gates, barrier-only workspace gates) — believable four-way parallelism under no-VCS, with the frozen-manifest check as the enforcement.
