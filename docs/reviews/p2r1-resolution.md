# Phase 2 plan — round 1 resolution map

Both reviewers fix-first ([codex-p2r1.md](codex-p2r1.md), [grok-p2r1.md](grok-p2r1.md)). All findings applied to `docs/plans/phase2-core-loop.md` (no VCS — the plan file in the working tree is the single source):

| Finding | Resolution |
|---|---|
| codex B1 — plan referenced `TradeExecuted`; code emits `TradePlaced` | Frame mapping rewritten against the real event names (TradePlaced, MarketSeeded/Advanced/Resolved/Voided, VoteCast, DepositCredited, CuratorNeeded); real-trade WS integration test asserts derived frames |
| codex B2 — `sched:` advisory keys not durable → racing loser gets IllegalTransition | `lifecycle_commands` table (0004); AdvanceMarket inserts key in the transition tx, key-exists → recorded-receipt replay; failure rolls key back; concurrent + retry contract tests |
| codex B3 / grok velocity semantics — cross-market races; global-vs-per-market ambiguity | User-level global window is the Phase 2 control (per-market is structurally 1 vote; market-level arrival anomaly deferred to Phase 3, stated); count guarded by a user advisory lock in the fixed lock order; cross-market concurrent boundary test |
| codex B4 — curator flag had no clearing action | `POST /admin/…/resolve` gains `{"decision": "resolve_at_tally" \| "void"}`; ResolveMarket `curator_override` settles accordingly and clears `curator_flagged_at` in the same commit; FlagCuratorNeeded is its own atomic tx |
| grok B1 — D21 phone-uniqueness leg unenforced | CastVote rule 0: `user_has_channel` required (`PhoneVerificationRequired` 403); uniqueness structural via `unique(channel,address)` |
| grok B2 — vote frames during hidden window leak the oracle | Tally frames emitted only while `Live && now < tally_hidden_at`; from the boundary on, no vote-derived data crosses WS or REST detail; boundary tests both sides |
| codex M1 / grok — at-least-once unstated | Stated; frames carry outbox seq; client dedupe `(seq, type)`; zero-receiver send = success; rollback/retry + zero-subscriber tests |
| codex M2 — process-local broadcast breaks the replica claim | Phase 2 core+WS declared a singleton deployment; SKIP LOCKED scoped to overlapping-pump safety; NATS = later phase |
| codex M3 — manifests don't compile as sketched | axum `ws` + tokio `time` features, pinned tokio-tungstenite dev-dep, real TCP upgrade test |
| codex M4 / grok — no snapshot/reconnect/server-time; §7.1 unaddressed | Versioned `ServerFrame`; subscribe → immediate `snapshot` (server_now, state, prices, tally-if-visible); reconnect = resubscribe + snapshot rehydration; countdowns from server_now + monotonic delta; §7.1 user frames explicitly Phase 4 |
| codex M5 — due query unindexed/unbounded; sweep can't express query failure | Four partial indexes in 0004; bounded ordered UNION ALL (256); `sweep() -> Result<SweepReport, StoreError>`; flagged markets excluded by the query |
| codex M6 — chart sync signatures, bigint overflow, bucket=0, missing index | Async `Result` ports; numeric SQL with checked narrowing; bucket validated 1..=86400; half-open UTC buckets; `trades_market_time_idx`; max-bound fixture |
| codex M7 — unpinned scaffold; SW not installable; token claim false | Pinned pnpm + create-next-app@15 non-interactive; sw.js + registration + 192/512 icons; token posture restated as labeled local-demo exposure |
| codex M8 / grok — e2e flakiness; Closing mapped to 409; one-vote market unscored | PlaceTrade maps Closing → `TradingFrozen` 423 (owned by 2.2); sweep cascades all due transitions in one tick; e2e polls with deadlines (no sleeps), measures payout from the `closed` frame, seeds `min_votes_to_resolve=1`, ships `scripts/ws_probe.mjs` |
| grok — post-resolve UX thin | Detail page renders resolved/voided as first-class screens (final %, redemptions, your payout) |
| codex ckpt 4 — at-limit ambiguity | Pre-count max−1 passes, max rejects — stated with tests |
| codex ckpt 7 — scripts referenced "CI git-diff" | Reproducibility = run twice + `cmp`; ci.yml kept as future reference only (no VCS exists) |
