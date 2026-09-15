R1 FIX VERIFICATION:

B1 — VERIFIED — Task 2.0 now maps the built `TradePlaced` event and requires a real seeded-trade integration test that observes both price and tape frames.
B2 — VERIFIED — `lifecycle_commands` makes a successful transition durable, the existing-key lookup is explicitly after `serialize_key`, and a failed transaction rolls the command back so the same key can retry; implementation must perform the lookup before mutation rather than recover from a PostgreSQL unique-violation.
B3 — REGRESSED — The global user cap is now explicit, but no `VoteTx` role can acquire the required user lock before `market_for_update`, and the proposed one-argument advisory key shares a collision namespace with idempotency locks, so the stated deadlock-free order is not implementable safely (NEW B1).
B4 — REGRESSED — The curator decision and same-commit flag clearing are specified, but “set flag + append event atomically” does not make flagging at-most-once when two schedulers act on stale due rows; the update needs an atomic winner predicate and test.
M1 — VERIFIED — At-least-once delivery, `(outbox seq, frame type)` dedupe, zero-receiver success, rollback/retry, and zero-subscriber tests are now explicit.
M2 — VERIFIED — Phase 2 now honestly requires a singleton core/WS deployment and limits `SKIP LOCKED` safety claims to competing publication pumps.
M3 — VERIFIED — The required Axum WS and Tokio time features, a pinned direct WS test dependency, and a real TCP upgrade test are all named.
M4 — REGRESSED — Versioning, lag recovery, tally suppression, and the Phase-4 notification deferral landed, but the snapshot lacks the server deadlines needed to turn `server_now` into a countdown and the plan does not add the query/projection sources for several promised fields (NEW B3).
M5 — VERIFIED — The four partial due indexes, bounded ordered query, flagged-row exclusion, sweep-level `Result`, and per-market error collection are specified.
M6 — VERIFIED — The ports are async `Result`s, bucket bounds and half-open UTC buckets are explicit, PostgreSQL `numeric` covers the dangerous intermediates, narrowing is checked, NO prices are normalized before weighting, and the index/max-bound test landed.
M7 — REGRESSED — The amended scaffold remains interactive/unsafe for this repository because it omits `--yes` and `--disable-git`, uses the unsupported spelling `--src-dir=false`, and conflicts with the stale Files-list claims about manifest and token placement (NEW M1).
M8 — REGRESSED — Closing→423, deadlines, the checked-in probe, and `min_votes_to_resolve=1` landed, but a global four-pass/256-row cascade does not guarantee four transitions for any one overdue market under backlog, and the payout assertion targets a position projection settlement never updates (NEW B4).

GROK FIX VERIFICATION:

Phone rule 0 — REGRESSED — Replay-first placement is correct, but `user_has_channel(any row)` proves neither a phone/iMessage channel nor verification, so an arbitrary channel row satisfies the claimed phone-linked rule.
Tally suppression — VERIFIED — Both WS tally frames and REST detail use the same strict `Live && now < tally_hidden_at` visibility rule, including suppression exactly at the boundary.
Snapshot frame — REGRESSED — The immediate snapshot/reconnect behavior is sound, but `server_now` without `closes_at` and `tally_hidden_at` cannot drive the promised countdowns, and no MarketQueries snapshot/tally contract is added.

NEW FINDINGS:

[B1] — docs/plans/phase2-core-loop.md Task 2.2 vs crates/application/src/ports.rs lock protocol: `CastVote` cannot request the user advisory lock through `VoteTx`, while acquiring it inside a later read would violate the required pre-market order; furthermore `hashtext('vote_user:' || user_id)` occupies the same one-key advisory namespace as caller-controlled idempotency keys, permitting cross-class collision and lock-order inversion. FIX: Add an explicit user-lock role to `VoteTx`, call it after replay lookup and before `market_for_update`, implement it in both stores, and use a disjoint two-key advisory namespace such as `(USER_LOCK_CLASS, hashtext(user_id))` with a concurrency/deadlock contract test.

[B2] — docs/plans/phase2-core-loop.md Worker protocol and Tasks 2.0–2.3 Files: the plan declares 2.0, 2.1, and 2.2 independent although all require overlapping edits to `main`, HTTP routes/state/OpenAPI, ports, and fakes, which is not safely mergeable in the mandated shared no-VCS worktree and the per-task file lists omit several of those owners. FIX: Sequence all overlapping Rust tasks (or assign them to one worker) and make each Files list exhaustive before dispatch.

[B3] — docs/plans/phase2-core-loop.md Task 2.0 frame contract vs existing outbox/query APIs: the snapshot has no declared `MarketQueries` source for its tally/state/visibility projection, `TradePlaced` lacks handle, trade timestamp, and trade sequence, the lifecycle event lacks redemption values, and “seq” ambiguously means outbox sequence in WS dedupe but trade sequence in the tape. FIX: Define the snapshot and event-enrichment query ports, add every necessary source-field/query change, expose `outbox_seq` and `trade_seq` as distinct names, and include `closes_at`/`tally_hidden_at` in snapshot/detail contracts.

[B4] — docs/plans/phase2-core-loop.md Tasks 2.4–2.5 vs crates/application/src/resolve_market.rs: settlement pays ledger accounts but never transforms `positions`, so the promised terminal UI and exit test cannot observe payout via “positions API showing realized PnL,” regardless of scheduler latency. FIX: Atomically project settlement into positions (zero shares/cost and add payout minus relieved cost to realized PnL) with conservation/replay tests, or define a separate settled-position/payout projection and point both UI and e2e at it.

[M1] — docs/plans/phase2-core-loop.md Tasks 2.4–2.5 scaffold/probe commands: the stated create-next-app invocation can prompt and initialize Git, `--src-dir=false` is not a supported flag form, pnpm is only major-pinned, and the root `scripts/ws_probe.mjs` has no declared direct `ws` dependency from which Node can resolve it. FIX: Pin exact tool versions, invoke create-next-app with `--yes --disable-git` and omit `--src-dir`, reconcile the Files list to the public manifest/browser-token design, and declare/install `ws` in a package visible to the root probe.

FINAL VERDICT: fix-first
