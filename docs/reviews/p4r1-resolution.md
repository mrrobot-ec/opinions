# Phase 4 plan — round 1 resolution map

codex **BLOCKED** ([codex-p4r1.md](codex-p4r1.md)); grok **fix-first** ([grok-p4r1.md](grok-p4r1.md)). The plan was rewritten; every finding maps:

| Finding | Resolution |
|---|---|
| codex B1 — no caller identity on comment writes / mark-read / subscribe_user | Self-asserted `user_id` in every write body (validated to exist), `viewer_id` token-gated, mark-read scoped `POST /users/{id}/notifications/read`, `subscribe_user` replaces the socket's single subscription post-token-check pre-snapshot; posture honestly restated as non-authentication |
| codex B2 — hot cursor unstable across hour boundaries; SQL truncation unpinned | `as_of`-pinned hot cursor `(as_of, hot_score, created_at, id)` reused across pages; SQL `numeric` + explicit `trunc(...)::bigint`; value-equality grid incl. ties and future-time error |
| codex B3 — notifier cursor unseeded/unlocked; SKIP LOCKED could lose events forever | Cursor row seeded in 0006 + `FOR UPDATE` before read; events fetched with a plain ordered read, NO row locks; max-fetched-seq advance in the same tx; the eight enumerated crash/concurrency tests |
| codex B4 — exactly-once rows ≠ reliable frames; untyped bus; framel dedupe fields missing | Honest contract: rows exactly-once, frames best-effort at-least-once; post-commit broadcast + injected commit-to-send crash test proving snapshot/REST recovery; typed `BusEvent` enum; `notif` frames carry `{v:1, id, source_seq}` |
| codex B5 / grok M3 — spam rule unserialized; counter drift; threshold-after-restore ambiguity; vote/report on shadow | `UserLockGuard` on post (author lock in the global order); insert-unique-first then mutate counters; threshold event in-tx; **restore = epoch reset (report rows deleted)**; vote/report/reply rejected on non-visible comments |
| codex M1 — 0006 corrupts pre-existing threads | Recursive-CTE depth backfill with cycle guard + failure raise, reply_count aggregate, body_hash backfill, non-negative/status checks, composite same-market parent FK; pre-0006 nested fixture test |
| codex M2 — fanout cardinalities unimplementable; event payloads thin | Terminal-facts aggregation (settlement/void only, summed per user), voter anti-join, both-outcomes single row, void = union; `CommentPosted` carries author/parent ids, `MentionCreated` payload pinned; 1,000-participant < 2s hard gate |
| codex M3 — indexes don't fit the list/fanout shapes | `comments(market, created desc, id desc)`, `notifications(user, id desc)`, `positions(outcome, cost desc, user)`, `realizations(market, source, user)` + `(user, created desc)` |
| codex M4 / grok B3 — moderation nondeterministic; hash normalization undefined; borrowed mentions | One pinned pipeline (NFKC → lowercase → strip Cf/controls → collapse ws → trim) for hash AND checks; Unicode-scalar length cap; `https?://` link tokens; owned lowercase `Vec<String>` mentions; golden collision vectors; multi-account copy-paste explicitly out of scope until the Phase 5 LLM screen |
| grok B1 — report brigading cheap | Reporter floor (age OR tier, D21 primitives) AND per-reporter velocity; e2e proves unqualified accounts cannot shadow; restore epoch reset makes re-brigading cost full threshold again |
| grok B2 — hidden-window comment side-channel unaddressed | **Decided: accepted speech** — structured surfaces stay protected, prose is not policed (liars make it worthless anyway); residual named in scoring.md; bounded by the new per-author comment velocity cap |
| grok M1 — mention harassment; toast fatigue; preferences missing from deferrals | Materializer drops mentions beyond `mention_notifs_per_hour` per recipient (logged); toasts only for `resolution_*`/`rep_tier_change`; preferences + mute/block added to deferrals |
| grok M2 — no comment velocity | `max_comments_per_window` under the author lock → `Blocked("rate")` |
| grok M4 — holder∧voter loses their score | Single `resolution_trade` row carries `{realized_delta, redemption, score_bp, side}` post-resolution |
| grok M5 — cast-timestamp visibility unstated; holders label | Stated: timestamps public (D6), side/score gated; holders "by committed capital" + MTM deferral in utoipa |
| grok M6 — copy under-specified | `docs/copy/notifications.md` with exact templates for every type + error/banner copy; web renders from it |
| grok M7 — deferral list incomplete | Preferences, comment images, X-linked profiles added; tape noted as reused Phase 2 surface |
| grok m1 — hot index mismatch | Candidate-window (last 500) stated; denormalized rank named as deliberate future work |
| grok m2 — no edit/delete | UI ships no such affordances; copy promises none |

## Round 2 outcome

**grok: sound-to-build** ([grok-p4r2.md](grok-p4r2.md)) — all R1 items verified; three residuals applied (scoring.md accepted-speech paragraph on the 4.4 checklist, `report_window_secs` added to config, legacy `body_hash` left NULL because SQL cannot reproduce the NFKC pipeline — never-matching is honest, and `spam_window_secs` bounds lookback so legacy rows exit any realistic window at migration time anyway).

**codex: fix-first** ([codex-p4r2.md](codex-p4r2.md)) — 5 verified; 5 bounded blockers, all applied:

| P4R2 item | Resolution |
|---|---|
| N1 — partial-index `ON CONFLICT` not inferable | Conflict target names the predicate: `ON CONFLICT (user_id, source_seq) WHERE source_seq IS NOT NULL DO NOTHING` |
| N2 — hot candidate set unfrozen across pages | Candidate subquery pinned to `created_at <= cursor.as_of` with deterministic `(created_at DESC, id DESC) LIMIT 500` |
| N3 — "at-least-once" contradicted the zero-frame crash test | Frames renamed **best-effort and deduplicable**; the crash test proves snapshot/REST recovery, which is the actual contract |
| N4 — legacy hash drift | Converges with grok's N3 resolution: legacy `body_hash` = NULL (never matches); duplicate window spans post-0006 posts only, stated |
| N5 — both-outcomes `redemption` undefined | Payload renamed and completed: `payout_total_micro` + per-side `redemption_yes/no_micro` rates + `realized_delta_micro` (+ `score_bp/side` when voted) — one well-defined row |

Codex verifies these five in a bounded addendum while the web task builds; the Rust chain starts on the addendum's verdict.
