VERDICT: fix-first

Reviewed `docs/plans/phase2-core-loop.md` against spec §§1/3.4/4/7.1, D6/D11/D21/D22, `cast_vote.rs` / `resolve_market.rs` shapes, and the converse graph (corridor unchanged). Phase 2's *direction* (scheduler + outbox→WS + PWA shell + CastVote friction) is right; several product-integrity gaps would ship a live loop that still fails the oracle/UX claims.

## FINDINGS

[B1] docs/plans/phase2-core-loop.md §2.0 + §2.2 vs docs/spec.md §3.4 / D21: Phase 2 claims the **vote-oracle minimum integrity bar**, but Task 2.2 only adds global user velocity + young-account near-close friction and **explicitly defers device fingerprinting** without adding **unique verified phone per voting account** — still listed launch-blocking in D21 and §3.4. REST still accepts arbitrary `user_id` (Phase 1 posture); farm accounts without phones remain free. FIX: either (a) make phone uniqueness + "vote requires linked verified channel" a hard Phase 2 CastVote rule (port: user must have `user_channels` row; unique phone already DB-side), or (b) rewrite the goal to "partial D21 friction only" and list phone uniqueness as launch-blocking still open — do not claim the min bar is shipped.

[B2] docs/plans/phase2-core-loop.md §2.0 WS protocol vs D22 / §3.4: during `Closing`, tallies/vote counts must be **hidden**, but `VoteCast` still fires (voting continues) and the plan never forbids fanout of running tallies/seq/vote-count frames. Spec §5.1 historically fans "votes-count changes" over WS. If any price/lifecycle/trade/tally path re-exports aggregate counts or chart buckets that include late-window votes, the hidden window is a UI lie. FIX: specify **tally-suppression policy** for the freeze window: no `tally`/`vote_count` frames while `state==Closing`; REST chart/tape/market detail must not expose running vote share; only `tally_hidden:true` + lifecycle; document that `VoteCast` is outbox-only / admin until `Closed`.

[M1] docs/plans/phase2-core-loop.md §2.2 velocity: `votes_count_since(user, window)` is **global across markets**, while D21/§3.4 say **"per-market vote velocity"**. With one-vote-per-user-per-market, per-user global rate limits farm *breadth*; it does **not** implement market-level burst limits (many fresh accounts pile onto one market in the last hour — that's the sybil shape). FIX: clarify threat model in plan: (1) keep global per-user rate as account-farm friction, **and** (2) add or explicitly defer to Phase 3 a **per-market aggregate velocity** check (`votes on market since T` / anomaly flag) — align D21 wording with what ships.

[M2] docs/plans/phase2-core-loop.md §2.0: WS subscribe has **no initial-state / snapshot frame** — client must REST then race the socket. A trade between REST and subscribe is invisible until the next event; reconnect after lag-disconnect same hole. FIX: on `subscribe`, server emits a snapshot (`lifecycle` + last price + `tally_hidden` if Closing + optional last N tape rows) before streaming; document reconnect = re-subscribe + REST reconcile.

[M3] docs/plans/phase2-core-loop.md §2.4 countdown "closes-in": no **server time** source of truth — client clocks will show wrong freeze/resolve boundaries vs D22 API cutoffs (server `now` already authoritative for trades). FIX: include `server_now` on market detail REST and/or a cheap `GET /time` / WS hello; countdown = `closes_at - server_now` with skew correction.

[M4] docs/plans/phase2-core-loop.md §2.0 relay: batch is `SELECT … FOR UPDATE SKIP LOCKED` → **broadcast** → mark `published_at` → commit. Crash after broadcast, before commit → **at-least-once re-broadcast**. Acceptable only if clients render by `seq` idempotently — not stated. FIX: state at-least-once explicitly; require clients key UI updates by `seq` (ignore `seq ≤ last_seen`); optional: mark published before broadcast is worse (drop on crash) — prefer current order + client idempotency.

[M5] docs/plans/phase2-core-loop.md §2.5 e2e live-loop: 60s/90s walls with **1s scheduler tick** are usually fine (`now >= boundary`), but fixed `sleep` relative to seed start will flake if seed/core startup overruns or host is slow; "payout within 2s of close" races resolve+WS+REST. FIX: poll `GET /markets/{id}` until state transitions (timeout 120s) rather than blind sleep; assert payout with a short poll loop; record `tally_hidden_at`/`closes_at` from API after seed, not assumed offsets.

[M6] docs/plans/phase2-core-loop.md §2.4 PWA scope: vote-gate blur + slider + live prices is the right minimum **if** resolution feedback exists — plan under-specifies **post-resolve UX** (positions refresh, "paid" badge, score reveal). Without it the "live loop" ends in a cliff after freeze. FIX: require market detail to handle `lifecycle` → Resolved/Paid/Voided with copy + portfolio refresh (REST once on lifecycle event, still no polling).

[m1] docs/plans/phase2-core-loop.md §2.0: no auth on `/ws` is fine for public tape, but pair with B2 — never put private PnL on the same channel without auth later.

[m2] docs/plans/phase2-core-loop.md CI/Phase 1 hangover: Phase 1 OpenAPI freshness still documents `git diff --exit-code` (ci.yml / phase1 plan); Phase 2 correctly switches to dual-run compare. Workers following old CI copy will try git. FIX: update `.github/workflows/ci.yml` OpenAPI step (and any remaining plan text) to dual-run `cmp`/`diff` without git when you next touch CI.

[m3] Device-fingerprint deferral is **engineering-sound** (no client signal) — keep it deferred, but don't use it as cover for missing phone uniqueness (B1).

## CHECKPOINTS

1. **Relay correctness:** Broadcast-then-mark is at-least-once; double-broadcast on crash is real; acceptable if clients de-dupe by `seq` — **must be stated** (M4). SKIP LOCKED batch ownership is sound.
2. **WS backpressure:** Disconnect lagging receivers is the right call vs unbounded buffers. Reconnect story is **under-specified** (M2) — re-subscribe + snapshot required.
3. **Scheduler races:** `sched:<market>:<event>` + row locks + existing use-case keys make double-fire **impossible for success paths**, not merely unlikely. Failed-then-retry reuses the same key → replay-safe. `CuratorNeeded` once-flag is correctly designed.
4. **Integrity-rule ordering:** Replay before integrity is **correct** (a committed vote must not start 429ing). Near-close inclusive boundary (`now >= closes_at - near_close`) is stated and testable — keep exclusive/open docs consistent with that.
5. **Chart math:** Collateral-weighted avg with integer division is OK if overflow uses i128 intermediates (specify); NO→YES flip is correct. Bucket edge cases need a property/fixture test as planned.
6. **PWA scope honesty:** Dev-login + demo token labeled DEV is acceptable for local; money math stays server-side — good. Missing server time, WS snapshot, resolve UX make the shell feel hollow (M2/M3/M6).
7. **No-VCS discipline:** Phase 2 plan is clean; **CI OpenAPI freshness still assumes git** (m2). Worker protocol correctly uses file existence over commits.

## WHAT CHECKS OUT

- Scheduler driving existing `AdvanceMarket`/`ResolveMarket` is the right reuse — no second lifecycle brain.
- D22 trade freeze already enforced in PlaceTrade; Closing still allows votes (cast_vote correctly ignores `tally_hidden_at`) — product-correct.
- Outbox as WS source of truth matches D9/D11 better than ad-hoc pubsub.
- Vote-gate blur + crowd-guess slider + explicit trade confirm matches  parity targets without boiling the ocean.
- Deferring NATS, full sybil, rep, comments, Playwright is scope-honest for Phase 2.

## BUILD READ

Do not start 2.0–2.2 until **B1/B2** plan text is fixed (integrity claim + tally-suppression). M2–M5 should land in the same amend. Task 2.4 can scaffold in parallel once snapshot/server-time requirements are written.
