# Phase 5 plan — round 1 resolution map

codex **BLOCKED** ([codex-p5r1.md](codex-p5r1.md)); grok **fix-first** ([grok-p5r1.md](grok-p5r1.md)). The plan was rewritten wholesale; every finding maps:

| Finding | Resolution in the revised plan |
|---|---|
| codex B1 / grok M5 — wave not file-disjoint (DraftSpec/DraftEngine cross-ownership, 0007b conflict, Cargo/lib/mod/pg-core/fakes-core/errors/models/frames/justfile/fixtures unowned) | 5.0 exclusively owns and pre-writes ALL of it; 0007b abolished (0007 is the sole schema file); the matrix now lists every surface; frozen-file **sha256 manifest** recorded by 5.0 and verified at the 5.5 barrier (the no-VCS freeze check); frozen changes are coordinator-mediated and re-manifested |
| codex B2 — pre-wiring not compilable/coverage-clean; no NotImplemented variant | 5.0 ships a compiling skeleton: final trait signatures, `StoreError::Unavailable(&'static str)`, behavior-asserted tests on every placeholder branch, config-gated loop wiring with startup tests; the full barrier gates run green at the END of 5.0 before the wave |
| codex B3 / grok B3 — publication "same tx" impossible; SeedMarket replay echoes caller UUID; publish_now review integrity | Crash-recoverable **saga** with persisted `publish_stage` + pre-generated `published_market_id`; SeedMarket replay-id fix (returns the stored id — regression test); ONE authority for sweep and publish_now; publish_now legal only from `approved`; final-row re-validation under the draft lock; crash tests at every stage |
| codex B4 — job schema can't do two workers/crash-replay; no real unique | `available_at`/`claim_token`/`lease_expires_at`; short SKIP-LOCKED claim tx committing before render; CAS completion on `(id, claim_token, 'rendering')`; expired-lease reclaim; saturating backoff; partial unique `(market_id, kind) where status in (queued,rendering,ready)` in 0007 |
| codex B5 / grok m1 — no durable asset refs; share cards collide in (market,kind) | `markets.poster_asset_url`/`video_asset_url` written at attach; snapshot + list/detail DTOs + versioned `asset` frame carry them; placeholder semantics stated; **share cards removed from video_jobs** — on-demand render, content-addressed cache keyed by (user, market, realization digest, spec version) |
| codex B6 / grok M7 — moderation seam doesn't exist as claimed; sync remote under locks; only-escalate enforcement point | Application-owned `ModerationPreflight` port; **async escalation decided**: a third outbox consumer (proven cursor pattern) runs the preflight outside any lock and applies visible→shadow only — the severity join is application code; LLM may only Shadow (never Blocked, never unshadow); brief-public-then-shadow accepted and stated |
| codex M1 / grok ckpt8 — split proof gameable; concurrent workspace gates unsafe | Pure-move proof = identical test inventory + per-crate coverage numerator AND denominator + sorted symbol manifest (`cmp`); wave isolation: per-worker `CARGO_TARGET_DIR` + per-worker databases + scoped tests only; workspace gates barrier-only |
| codex M2 / grok M2 — slotting/TTL/backpressure boundaries | Integer slot arithmetic pinned (floor division, remainder to last slot, strictly-after boundary rule); slot **reservation** via `unique(tier, publish_at)` with bump-to-next-free within `max_slot_horizon_secs`; `expires_at` persisted at creation; admission under an advisory lock; due order `(publish_at, created_at, id)` |
| codex M3 / grok ckpt5 — SVG byte-identity threats | RenderSpec v1: Vec-ordered, integer geometry, literal colors, own money formatter + own XML escape at build, fixed wrapping, no clock, template-ordered attributes; **byte identity is the contract; viewer-font rasterization variance stated**; golden byte+digest tests across fresh instances |
| codex M4 — artifact crash/path safety | UUID-derived filenames only; temp + fsync + atomic rename under canonicalized root; serve by job lookup never path input; nosniff + CSP `default-src 'none'` + inline disposition; traversal/symlink/partial-write tests |
| codex M5 / grok ckpt6 — no-network proof weak; injection posture unproven | Mandatory transport injection; sole real constructor in one audited module; deny-transport tests assert the exact outbound request (incl. versioned prompt constant + delimiters + body bytes); all-or-none env validation; deps-check grep targets HTTP-client construction outside the module |
| grok B1 — cadence is a human-fed queue, unstated | Stated loudly: supply is curator-fed (batch topic-pool creation, human approval always the gate); **unfilled slots lapse — no unreviewed market ever publishes**; `SlotUnfilled` event → admin notification + dashboard fill-rate metric |
| grok B2 — public SVG injection/serve posture | Escape-at-build + serve headers + img-only product rule + adversarial-question property test + pinned card fields (no referral this phase, stated) |
| grok M1 — tier defaults/floors unpinned | `ContentConfig.tier_defaults` + floors that refuse approve (seed, min_votes ≥ D21 floor, hidden < open); re-validated at approve AND publish |
| grok M3 — silent LLM fallback | `source` = actual generator; `fallback_from` column + dashboard banner; e2e asserts visibility |
| grok M4 — flash video parity overclaim | Poster-first flash stated in constraints + curation.md; no "video every hour" copy |
| grok M6 — e2e proves one market, not the cadence | e2e adds: two drafts → two slots/markets; empty boundary → zero markets + one SlotUnfilled; publish_now ∥ sweep; two-worker claim test |
| grok m2 — auto-publish vs LP exposure | Approve refused when kill-switch tripped + `daily_seed_budget_micro` cap |

## Round 2 outcome

**grok: sound-to-build** ([grok-p5r2.md](grok-p5r2.md)) — all R1 items verified; its `SlotUnfilled` watermark note folds into the slot-key dedupe already specified. **codex: fix-first** ([codex-p5r2.md](codex-p5r2.md)) — 5 verified; 5 bounded blockers, all applied:

| P5R2 item | Resolution |
|---|---|
| N1 — manifest inventory incomplete (main.rs, seed_market.rs, tooling unlisted) | Frozen set names both files + the manifest tooling; sorted path inventory compared BEFORE digests; added/removed target fails first |
| N2 — job issuance not durable across the enqueue→stage gap | `unique (draft_id, kind) where draft_id is not null` on video_jobs — idempotent across ALL statuses; crash test on the exact gap |
| N3 — no fenced error transition; attempts off-by-one | Error completion CASes on `(id, claim_token, 'rendering')`; `attempts >= max` boundary everywhere (claim-failure and reclaim); lease/token cleared on requeue |
| N4 — expiry had no authority; budget not reserved | One draft-locked `pending && now >= expires_at → expired` transition shared by review + a bounded sweep arm (race-tested); day-scoped advisory lock reserves the seed budget counting approved+in-flight+published |
| N5 — cursor protocol incompatible with remote calls | Two-stage: cursor tx materializes idempotent `moderation_jobs` (atomic with cursor advance, zero remote calls); a leased runner (same machinery as video jobs) does the preflight + fenced escalation |

Codex verifies the five in a bounded addendum; **5.0 starts on its verdict** (the wave follows 5.0's green barrier).
