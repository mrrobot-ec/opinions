# Opinions — Phase 2 (Core Loop UX + Oracle Defense) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The product loop becomes *live and visible*: markets advance and resolve on the clock without an admin, prices/trades/tallies stream over WebSocket (RPC-in/WS-out, D11 — no polling anywhere), the vote-oracle minimum integrity bar (D21) enforces in `CastVote`, and an installable **PWA** lets a person browse markets, vote with a crowd guess, and trade preview→confirm against the real core.

**Architecture:** Unchanged and binding — `domain ← application ← adapters ← main` with `scripts/check_dependency_rule.py`; new behavior follows the same shapes: scheduler = an application use case driven by a `main` loop; WS fanout = an adapter fed by the transactional outbox (the relay marks `published_at`, spec §5.6); integrity checks = `CastVote` rules over new port methods; web = a separate `web/` tree talking REST+WS only (it is a client, not a crate — the dependency rule script ignores non-cargo trees).

**Tech Stack additions:** tokio broadcast + axum WS (already in axum 0.8); Next.js 15 + TypeScript + vitest for `web/` (pnpm); no new Rust deps beyond what adapters already carry.

## Global Constraints (Phase 2 deltas)

Everything from Phase 0/1 Global Constraints still binds, plus:

- **NO GIT, EVER** (user directive; the repo is intentionally not under version control): never run `git init/add/commit/log/status` or any VCS command — they will fail or, worse, recreate state the user deleted. Cross-worker dependency detection is by **file existence/content** (`ls`, `grep`), and every worker lists its changed files in `worker_done`. Do not create backup copies of files you edit.
- Coverage floors unchanged (domain 100 / application ≥90 / adapters ≥90); new application/adapters code is inside those gates. `web/` has no coverage gate this phase — it must `pnpm build` clean and pass its vitest smoke tests.
- **No polling in any client path**: the web app never `setInterval`-fetches prices/tallies; live data arrives over WS. (REST is for commands + initial page loads.)
- WS payload numbers come from the same server-computed sources as REST (`price_micro`, trade rows) — clients never derive money math.
- All new tunables are typed configs injected in `main` (env-backed for now): `SchedulerConfig { tick: Duration }`, `VoteIntegrityConfig { max_votes_per_window: u32, window_secs: u64, near_close_secs: u64, min_account_age_secs: u64 }`.

**Worker protocol (codex P2R2 B2 — the Rust tasks share `main.rs`, routes/state, `ports.rs`, and `fakes.rs`, which is not safely mergeable across workers with no VCS):** **one worker builds the Rust chain sequentially: 2.0 → 2.1 → 2.2 → 2.3.** Only 2.4 (`web/` — fully disjoint tree) runs in parallel with the chain. 2.5 last. Within the chain the shared-file overlap is a non-issue (single owner).

---

### Task 2.0: Outbox relay + WebSocket gateway

**Files:**
- Create: `crates/adapters/src/relay.rs`, `crates/adapters/src/http/ws.rs`
- Modify: `crates/adapters/src/http/mod.rs`/`routes.rs` (mount `/ws`), `crates/main/src/main.rs` (spawn relay; pass broadcast handle into `AppState`)

**Interfaces:**
- **Manifest edits first (codex M3 — the sketch doesn't compile without them):** adapters + main add axum feature `ws` and tokio feature `time`; adapters dev-deps add pinned `tokio-tungstenite` (real TCP upgrade tests — `Router::oneshot` cannot exercise an upgrade).
- **Delivery semantics, stated (codex M1/M2):** the relay is **at-least-once** — it broadcasts before the `published_at` commit, so an update/commit failure can re-broadcast after retry. Every frame carries the outbox `seq`; clients dedupe by `(seq, frame_type)` and all frames are idempotent to re-render (state frames overwrite; tape frames keyed by seq). `broadcast::send` returning "no receivers" is **success** (ephemeral fanout — the mark still commits). **Phase 2 core+WS is declared a singleton deployment**: the broadcast bus is process-local, so replicas would each see only their own claimed batches; SKIP LOCKED exists to make overlapping pumps (restart overlap, a second accidental instance) safe for *publication marking*, not to make multi-node fanout work. Cross-process fanout (NATS) is a later phase.
- `relay.rs`: `pub struct OutboxRelay { pool: PgPool, tx: broadcast::Sender<WireEvent> }`; `pump_once` — one tx: `SELECT seq, event_type, aggregate_type, aggregate_id, payload FROM events_outbox WHERE published_at IS NULL ORDER BY seq LIMIT 128 FOR UPDATE SKIP LOCKED` → broadcast each → `UPDATE ... SET published_at = now() WHERE seq = ANY($1)` → commit → count. `run(self, tick)` loops (main spawns).
- **Event names come from the code, not this plan's imagination (codex B1):** the write path emits `TradePlaced` (see `place_trade.rs`), `MarketSeeded`, `MarketAdvanced`, `MarketResolved`, `MarketVoided`, `VoteCast`, `DepositCredited`, `CuratorNeeded`. Frame mapping below uses those names; the real-trade integration test asserts the derived frames end-to-end.
- **Frame data sources are declared, not improvised (codex P2R2 B3, signatures closed in the addendum):** (a) new query port `MarketQueries::market_snapshot(&self, m: MarketId, now: OffsetDateTime) -> Result<MarketSnapshot, StoreError>` — **the caller passes the clock and the port makes the tally-visibility decision atomically** — `MarketSnapshot { state, price_yes_micro, price_no_micro, tally: Option<(i64, i64)>, closes_at, tally_hidden_at }` (WS adds `server_now = now` to the frame; REST detail uses the same port). (b) **outbox payloads are enriched at the source, with the sources named:** `TradeWriter::insert_trade` returns `InsertedTrade { id, trade_seq, created_at }` (not a bare id — the DB assigns seq/timestamp at insert), and `TradeTx` gains a `UserReader { async fn handle(&mut self, u: UserId) -> Result<String, StoreError> }` role so `PlaceTrade` can construct the full `TradePlaced` payload `{handle, side, action, collateral_micro, trade_seq, created_at}` in-transaction; `MarketResolved` carries `{final_vote_bps, redemption_yes_micro, redemption_no_micro}` (ResolveMarket already computes them); `VoteCast` carries `{market_id}` only. The relay never queries per event except the price recompute from `pool`. (c) Frames name their sequences **distinctly**: `outbox_seq` (dedupe key on every frame) vs `trade_seq` (tape ordering) — never a bare ambiguous `seq`.
- `ws.rs`: `GET /ws` upgrade; **versioned tagged protocol** (`ServerFrame` enum, every frame includes `v:1` and `outbox_seq` where sourced from the outbox):
  - client → `{"op":"subscribe","market_id":"…"}` / `{"op":"unsubscribe","market_id":"…"}`
  - server → on subscribe, an immediate **`snapshot`** frame (codex M4/grok): `{type:"snapshot", v:1, market_id, server_now, state, price_yes_micro, price_no_micro, tally: {yes,no}|null, closes_at, tally_hidden_at}` — **`closes_at`/`tally_hidden_at` ship in the snapshot (and REST detail)** so `server_now` can actually drive countdowns (server deadlines + server now + client monotonic delta; never the client wall clock).
  - `price` frame on `TradePlaced`/`MarketSeeded` (recomputed from `MarketQueries::pool` at send time), `trade` frame on `TradePlaced` (payload fields above), `lifecycle` frame on `MarketAdvanced`/`MarketResolved`/`MarketVoided` (new state; resolution frames include the enriched redemption fields), `tally` frame on `VoteCast` **only while tallies are visible** (computed via `market_snapshot`) — **during `Closing` (and from `tally_hidden_at` onward) no vote-derived data crosses the wire at all** (grok B2; the suppression test sits on both sides of the boundary).
  - `GET /markets/{id}` (REST detail) gets the same visibility rule: tally fields null once hidden (test included).
  - Per-connection task: `broadcast::Receiver` filtered by subscription set; lagged consumers are disconnected (bounded memory); the client's recovery is **reconnect → resubscribe → snapshot rehydration** (no server-side cursor replay this phase — REST + snapshot is the rehydration path, stated). §7.1 per-user notification frames are explicitly Phase 4.
- No auth on `/ws` (read-only public data — matches the public-tape stance; write routes keep their tokens).

**Tests:** unit — subscription filtering; tally-visibility boundary (VoteCast at `tally_hidden_at − 1s` emits, at `+0s` suppressed; snapshot tally null in Closing); integration (DATABASE_URL-gated) — a real seeded trade pumps exactly once (`pump_once`=1 then 0), `published_at` set; zero-subscriber pump still marks published; rollback-injected retry double-broadcasts and the test client dedupes by seq (documenting at-least-once); real TCP WS test via tokio-tungstenite — subscribe → snapshot arrives first, then `price` + `trade` frames from a live trade; lagged-consumer disconnect.

- [x] Failing tests → implement → green (`cargo test -p adapters`, coverage floor holds) → deps-check green.

---

### Task 2.1: Market lifecycle scheduler (auto-advance + auto-resolve)

**Files:**
- Create: `crates/application/src/advance_due.rs`, `migrations/0004_scheduler.sql` (`alter table markets add column curator_flagged_at timestamptz;`)
- Modify: `crates/application/src/ports.rs` (+`due` query on MarketQueries; +`flag_curator_needed`/`clear_curator_flag` on the resolve/advance write roles), `crates/application/src/fakes.rs`, `crates/adapters/src/pg/store.rs` (impl), `crates/main/src/main.rs` (spawn loop)

**Interfaces:**
- Migration `0004_scheduler.sql` (owned by this task):

```sql
alter table markets add column curator_flagged_at timestamptz;
-- Durable lifecycle-command idempotency (codex P2R1 B2: an advisory lock alone turns
-- the racing loser into IllegalTransition; a persisted receipt turns it into a no-op replay).
create table lifecycle_commands (
  key text primary key,              -- e.g. sched:<market>:<event> or admin-supplied
  market_id uuid not null references markets(id),
  event text not null,
  resulting_state text not null,
  applied_at timestamptz not null default now()
);
-- Due-boundary partial indexes (codex M5) — one per sweep arm:
create index markets_due_open_idx    on markets (opens_at)        where status = 'scheduled';
create index markets_due_freeze_idx  on markets (tally_hidden_at) where status = 'live';
create index markets_due_close_idx   on markets (closes_at)       where status = 'closing';
create index markets_due_resolve_idx on markets (closes_at)       where status = 'closed' and curator_flagged_at is null;
create index trades_market_time_idx  on trades (market_id, created_at);  -- 2.3 chart scan
```

- **`AdvanceMarket` becomes durably idempotent:** it inserts its `key` into `lifecycle_commands` in the same transaction as the transition; on key-exists (checked after `serialize_key`, so no race — lookup-first, never unique-violation recovery) it returns the recorded receipt as a no-op replay. A failed attempt rolls the key back — retry works. Contract tests: two concurrent same-event commands → one transition, both receive success (one replayed); failed attempt then retry succeeds.
- **Settlement projects into positions (codex P2R2 B4; port shape closed in the addendum):** `SettlementIo::holdings` returns rows that carry their position identity — `Holding { account: domain::ledger::AccountId, owner: HoldingOwner, outcome: OutcomeId, side: Side, shares: MicroShares }` with `HoldingOwner { User(UserId), Pool }` — and `ResolveTx` adds the existing **`PositionWriter`** role to its supertrait. Inside the same settlement transaction, `ResolveMarket` (tally and void paths alike) maps each non-pool payout back to its `(user, outcome)` position via `position_for_update`/`save_position`: `realized_pnl += payout − cost` (full remaining cost relieved), then `shares = 0, cost = 0`. Idempotent by construction (the `resolve:<market>` key replay skips the whole transaction). Tests: post-resolve positions show zeroed shares + correct realized PnL for winner and loser; pool holdings settle to the ledger only (no position row); conservation unchanged; replay leaves positions untouched; the e2e's "payout visible in positions" assertion now has a real source.
- **`FlagCuratorNeeded` is at-most-once under racing sweeps (codex P2R2 B4-adjacent):** the flag update carries an atomic winner predicate — `UPDATE markets SET curator_flagged_at = now() WHERE id = $1 AND curator_flagged_at IS NULL RETURNING id`; zero rows → another sweep won → skip the event append. Test: two concurrent flag attempts → one event row.
- Port (on `MarketQueries`): `async fn due_markets(&self, now: OffsetDateTime, limit: u32) -> Result<Vec<DueMarket>, StoreError>` — bounded ordered `UNION ALL` over the four indexed arms (`Scheduled && now >= opens_at`, `Live && now >= tally_hidden_at`, `Closing && now >= closes_at`, `Closed && not curator-flagged`), default limit 256.
- Use case `AdvanceDue` with `pub async fn sweep(&self) -> Result<SweepReport, StoreError>` (codex M5: the initial query's failure is sweep-level, per-market failures stay isolated in the report):
  - **Cascade within one sweep** (codex M8, bound fixed P2R2): loop the due query + transitions until a pass applies zero transitions (safety ceiling 8 passes). The bound is on *passes*, not markets — under a 256-row backlog any single overdue market still traverses all its due boundaries within one sweep because each pass re-queries; the ceiling exists only to guarantee termination. Test: an overdue flash market resolves in one sweep even with 300 other due markets queued.
  - `Scheduled→Live`, `Live→Closing`, `Closing→Closed` via `AdvanceMarket` (keys `sched:<market>:<event>`).
  - `Closed` → `ResolveMarket`: success / `NeedsCuratorDecision` → **`FlagCuratorNeeded`** (its own small atomic tx: set `curator_flagged_at` + append `CuratorNeeded` outbox event together; already-flagged markets are excluded by the due query) / per-market errors collected.
  - `SweepReport { advanced: u32, resolved: u32, curator_needed: u32, errors: Vec<(MarketId, String)> }`.
- **Curator decision path (codex B4 — flagging without a clearing action is a dead end):** `POST /admin/markets/{id}/resolve` gains a body `{"decision": "resolve_at_tally" | "void"}` used only for curator-flagged markets: `ResolveMarket` accepts `curator_override: Option<CuratorDecision>` — `resolve_at_tally` settles at the actual tally despite thin participation (the curator judged it legitimate), `void` settles neutrally; **both clear `curator_flagged_at` in the same settlement commit**. Without the flag, a body is rejected (422) — the override exists only where the sweep created the question.
- `main`: `tokio::spawn` interval loop (`SchedulerConfig.tick`, default 1s) calling `sweep`; logs non-zero reports. Two schedulers racing are safe *for correctness* via the durable keys (deployment is still singleton per Task 2.0's declaration).

**Tests (fakes):** each boundary advances exactly once (second sweep no-op via `lifecycle_commands` replay); an overdue flash market cascades Closing→Closed→Resolved in ONE sweep; NeedsCuratorDecision flags once across two sweeps (event emitted once, market excluded after); curator `resolve_at_tally` and `void` both settle + clear the flag; a poisoned market's error doesn't stall others; sweep propagates a query-level `StoreError`.

- [x] Failing tests → implement → green (`cargo test -p application` + `-p adapters` for the query impl) → coverage floors hold.

---

### Task 2.2: Vote-integrity minimum bar in CastVote (D21)

**Files:**
- Modify: `crates/application/src/cast_vote.rs`, `crates/application/src/ports.rs` (VoteReader additions), `crates/application/src/fakes.rs`, `crates/application/src/error.rs`, `crates/adapters/src/pg/vote_tx.rs` (impls), `crates/adapters/src/http/error.rs` (status mapping)

**Interfaces:**
- `VoteReader` additions (in-tx): `async fn votes_count_since(&mut self, u: UserId, since: OffsetDateTime) -> Result<u32, StoreError>` (across all markets), `async fn user_created_at(&mut self, u: UserId) -> Result<OffsetDateTime, StoreError>`, `async fn user_has_channel(&mut self, u: UserId, channel: &str) -> Result<bool, StoreError>` (rule 0 passes `"imessage"` — channel-specific, matching the rule text below).
- **Velocity semantics, reconciled (codex B3 + grok; lock mechanics fixed per codex P2R2 B1):** D21's *user-level* control in Phase 2 is the **global rolling-window cap** — a farm's dominant signal is one identity voting across many markets (within one market, `unique(user_id, market_id)` already caps at 1). Because per-vote market row locks do NOT serialize one user voting in different markets concurrently, `VoteTx` gains an explicit role — `UserLockGuard { async fn lock_user(&mut self, u: UserId) }` — called **after the replay lookup and before `market_for_update`** (fixed order: idempotency → user → market; deadlock-free because every writer takes them in that order). Advisory locks use the **two-key namespaced form** so lock classes cannot collide: class 1 = idempotency keys (`pg_advisory_xact_lock(1, hashtext($key))` — `serialize_key`'s implementation moves to this form), class 2 = user locks (`pg_advisory_xact_lock(2, hashtext($user_id::text))`). Both stores implement the role; a two-connection deadlock/concurrency contract test rides with it. *Market-level* vote-arrival velocity is Phase 3 anomaly-sweep material, stated in the deferred list.
- `CastVote` rules (after replay short-circuit, under the locks above):
  0. **Phone-linked identity required** (grok B1; channel-specific per codex P2R2): `user_has_channel(u, "imessage")` — the port takes the channel type; an arbitrary non-phone channel row does not satisfy the rule — false → `AppError::PhoneVerificationRequired` (403). Uniqueness itself is structural (`unique(channel, address)`); OTP possession-proof is the stated Phase 3 deferral.
  1. **Velocity**: pre-count `>= max_votes_per_window` → `AppError::VoteVelocityExceeded` (429). At-limit semantics explicit (codex ckpt 4): pre-count `max−1` passes (becomes the max-th vote), pre-count `max` rejects.
  2. **Young-account friction near close**: `now >= closes_at - near_close` (inclusive) and `now - user_created_at < min_account_age` → `AppError::AccountTooYoungNearClose` (403, human-readable envelope copy; publish the rule per §3.4).
  3. **Seq side-channel closed (grok P2R2 residual; visibility rule made non-contradictory in the addendum):** the vote receipt returns `seq: null` **iff the window is hiding and the market is unresolved** — precisely: hidden when `now >= tally_hidden_at && state ∉ {Resolved, Paid, Voided}`, visible otherwise (pre-window receipts always show it; post-resolution replays reveal it — the plain `now >= tally_hidden_at` test would wrongly hide forever). DTO: `seq: Option<i64>`; tests: pre-window visible, in-window null, post-resolution replay visible.
- **Frozen-window HTTP mapping fix (codex M8, owned here):** `PlaceTrade` state checks re-ordered so `Closing` (or `Live && now >= tally_hidden_at`) deterministically maps to `TradingFrozen` → **423**; only states outside {Live, Closing} map to `MarketNotOpen` → 409. Route test pins both statuses.
- Defaults in `main` (env-overridable): window 3600s / max 30, near_close 600s, min_age 72h (spec §3.4). Device fingerprinting **explicitly deferred** (no client signal exists).

**Tests:** phone-unlinked user rejected (and the seeded linked user passes); pre-count max−1 passes / max rejects; **cross-market concurrent boundary** (two votes by one user racing the last slot from two connections → exactly one passes — Pg contract test, the fake honors the same semantics); young-account near-close matrix (young+near rejects, young+early passes, old+near passes, boundary `== closes_at - near_close` is near); replay short-circuits before integrity rules (an accepted vote replays cleanly even when the user is now over-limit); PlaceTrade in Closing → 423, in Resolved → 409.

- [x] Failing tests → implement (fake + Pg) → green, coverage floors hold.

---

### Task 2.3: Price history + public tape queries (charts feed)

**Files:**
- Modify: `crates/application/src/ports.rs` (MarketQueries), `crates/adapters/src/pg/store.rs`, `crates/adapters/src/http/routes.rs`/`dto.rs`, regenerate `openapi.json` + `api_models.py`

**Interfaces (codex M6 — async, validated, numeric-safe):**
- `async fn price_history(&self, m: MarketId, bucket_secs: u32, since: OffsetDateTime) -> Result<Vec<PricePoint>, StoreError>` — `bucket_secs` validated `1..=86_400` at the route (422 otherwise). Buckets are **UTC half-open** `[start, start+bucket)`. SQL computes everything in `numeric` (`collateral_micro::numeric * 1000000 / shares_micro::numeric` per trade; weighted avg = `sum(price_numeric * collateral)::numeric / sum(collateral)`), converting to `i64` only at the end with a checked cast (values ≤ 1e6 by construction post-division; the *intermediates* are what overflow bigint — hence numeric). NO-side normalization `price_yes = 1_000_000 − price_no` applied per trade before aggregation. `PricePoint { bucket_start, avg_price_micro: i64, volume_micro: i64, trades: u32 }`. Chart scan uses `trades_market_time_idx` from 0004.
- `async fn tape(&self, m: MarketId, limit: u32) -> Result<Vec<TapeRow>, StoreError>` — `{ handle, side, action, collateral_micro, created_at, seq }` (public by design, D6; seq doubles as the WS dedupe key), limit clamped ≤ 200.
- Routes: `GET /markets/{id}/chart?bucket=60&since=…`, `GET /markets/{id}/tape?limit=50` — utoipa'd; regenerate both artifacts with the existing scripts. Reproducibility check = run each script twice and `cmp` the outputs (the scripts' old "CI git-diff" wording is updated — there is no VCS; the ci.yml file remains as future reference only).

**Tests:** bucketing fixture (two buckets, exact expected weighted averages, documented rounding); a max-bound fixture (reserve-scale trades near 10^15 micro — the case that overflows bigint without numeric); NO-side normalization; `bucket=0` → 422; tape ordering/limit/clamp; http route tests against the fake.

- [x] Failing tests → implement → green; artifacts regenerated reproducibly.

---

### Task 2.4: Web PWA shell (`web/`)

**Files:**
- Create: `web/` — Next.js 15 + TypeScript app (pnpm): `app/page.tsx` (markets list), `app/m/[slug]/page.tsx` (market detail), `app/portfolio/page.tsx`, `lib/api.ts`, `lib/ws.ts`, `lib/types.ts` (hand-mirrored from `openapi.json` — cite each type's source path in a comment), `app/manifest.ts` + minimal service worker (installable PWA: name, icons (generate simple SVG-based), standalone display), `.env.example` (`NEXT_PUBLIC_CORE_URL`, `NEXT_PUBLIC_WS_URL`, `DEMO_TOKEN` server-side only), vitest + 2–3 component/logic smoke tests, `README.md` (run instructions)

**Product requirements (spec §1 parity targets, trimmed to shell scope):**
- **Markets list:** live cards (question, YES/NO prices in cents, closes-in countdown, state badge incl. the frozen-window "voting ends soon — trading paused" state).
- **Market detail:** price header updating **live via WS** (no polling); vote flow first (side buttons + crowd-guess slider 0–100 + submit → shows "#N" public vote number from the receipt); trading unlocks only after the vote (the gate made visible — pre-vote the trade panel renders blurred/disabled with copy, matching the  mechanic); trade ticket (side, $ amount quick-chips 1/5/20, preview call showing shares/avg price/fee/max payout, then an explicit **Confirm** button that calls `POST /trades` with a client-generated idempotency key); public tape (initial REST + live WS appends); simple price chart from `/chart` (SVG polyline is fine — no chart lib needed).
- **Portfolio:** positions with cost/realized PnL (user id from a dev login box storing the UUID + demo token in localStorage — Phase 1 auth posture, clearly labeled DEV).
- **Design bar (trust & polish is a superiority dimension — this must not look like a bootstrap template):** dark-first palette, one accent color for YES and a complementary one for NO used consistently, system font stack with tightened letter-spacing on numerals (`font-variant-numeric: tabular-nums` for prices), generous whitespace, visible live-ness (subtle pulse on price change), every state (loading/empty/error/frozen/resolved) designed with copy — no raw spinners-only screens.
- Mobile-first responsive; Lighthouse-installable (manifest + SW + icons) — verify with `pnpm build` + the Next PWA checklist manually noted in the report.

**Post-resolve UX (grok):** the detail page renders terminal states as first-class screens — final vote %, per-side redemption values, and (for the dev-logged-in user) their payout from realized PnL; voided markets say why (participation threshold) and show the neutral redemption. No dead-ends.

**Scaffold + installability, pinned (codex M7, corrected P2R2 M1):** corepack-pin an exact pnpm (e.g. `corepack use pnpm@9.15.4` — record the chosen exact); scaffold with `pnpm dlx create-next-app@15 web --yes --ts --app --no-eslint --no-tailwind --import-alias "@/*" --use-pnpm --disable-git` — **`--disable-git` is mandatory (this repo intentionally has no VCS; the default would git-init `web/`)**, `--yes` suppresses every prompt, and the unsupported `--src-dir=false` spelling is dropped (omit the flag). Vitest pinned exact in devDeps with a `test` script. The WS probe gets its own `scripts/package.json` with a pinned `ws` dependency (`cd scripts && pnpm install` — the probe resolves its own deps; nothing global). PWA: `public/manifest.json` + `public/sw.js` + **registration** in a client component, icons at **192 and 512** (generated SVG→PNG or maskable SVG per Next docs). **Token posture, honestly stated:** the demo token in the dev-login box lands in localStorage — that is deliberate *local-demo exposure*, labeled in the UI with a DEV banner; it is not "server-side only" (a server proxy is Phase 3+ hardening if needed). WS client: reconnect with capped backoff → resubscribe → snapshot rehydration; dedupe frames by `(seq, type)`; countdowns from snapshot `server_now` + monotonic delta.

**Constraints:** REST+WS only through `lib/api.ts`/`lib/ws.ts`; no client-side money math (display formatting only: micro→dollars/cents helpers with tests); no polling; `pnpm build` must pass with zero type errors; vitest green.

- [x] Scaffold (pinned, non-interactive) → types/api/ws libs with vitest for the formatting + frame-dedupe helpers → pages → manifest/SW/icons + registration → `pnpm build` green.

---

### Task 2.5: Phase 2 exit verification (in-worktree — there is no VCS)

1. Full Rust gates: fmt, clippy `-D warnings`, `cargo test --workspace` (fresh DB up: drop/recreate + migrations), `just coverage` (all floors), `just deps-check`, `just gate-test`.
2. Python: converse tests green (unchanged code, but re-run — the ws/relay changes share the DB).
3. **Live-loop demo extension** (`scripts/e2e_live_loop.sh` + `scripts/ws_probe.mjs` with its own `scripts/package.json` pinning `ws` — do NOT assume `websocat` exists): seed a short flash market (`closes_at = now + 90s`, `tally_hidden_at = now + 60s`, `min_votes_to_resolve = 1` — one voter must produce a score); start core (relay + scheduler) + converse; text-vote + text-trade as before; the probe subscribes and must capture, in order: `snapshot` → `price`+`trade` (from the trade) → `lifecycle: closing` → `lifecycle: closed` → `lifecycle: resolved`. **No fixed sleeps anywhere (codex M8/grok): every assertion polls with a deadline** (boundary waits get `boundary + 10s` deadlines; the 1s scheduler tick and cascade make transitions prompt but not instant). During `Closing`, a trade attempt must return **423** and the probe must see **no tally-bearing frames**. **Payout latency is measured from the `lifecycle: closed` frame timestamp to the positions API showing realized PnL — budget 2s** (spec §4.3 ×2 for the demo env), not from a wall-clock sleep. Then assert `vote_scores` row exists, per-currency ledger sums zero, and the market escrow account sits at exactly 0 (payouts + dust fully drained).
4. Web: `pnpm build` green; `pnpm test` green; manual checklist in the report (list renders against live core, WS price moves on a trade, vote→trade gate visible, install prompt available).
5. Docs sync: tick plan checkboxes, update PLAN.md index row for Phase 2, README status — **no VCS commands anywhere**.

- [x] Exit verification green (2026-08-12): `scripts/e2e_live_loop.sh` prints `LIVE LOOP GREEN`; fmt/clippy/test/coverage(100% line domain+application+adapters)/deps-check/gate-test; converse 33 pytest; web pnpm test+build.

## Deferred from Phase 2 (explicit)

Device fingerprinting (no client signal yet — Phase 3 with the full sybil stack), **phone OTP verification** (rule 0 requires a *linked* channel; proving possession is Phase 3 identity hardening — for iMessage-originated votes the channel is self-proving, the gap is web-originated dev users), **market-level vote-arrival velocity/anomaly detection** (Phase 3 sweep; Phase 2's velocity control is the user-level global window), NATS / multi-node WS fanout (core+WS is a declared singleton this phase), WS cursor replay (reconnect = snapshot rehydration), §7.1 per-user notification frames (Phase 4), rep/leaderboards + fee discounts (Phase 3), comments/social (Phase 4), real authn / server token proxy (embedded wallets — pulled forward if beta timing demands), Playwright golden-path (Phase 6 swarm), video pipeline (Phase 5).

## Reviewer checkpoints (verify specifically)

1. **Relay correctness:** SKIP LOCKED batch + `published_at` mark inside one tx — can an event be broadcast then the mark rolled back (double-broadcast on retry)? Is at-least-once acceptable for these frame types (idempotent client rendering), and is that STATED?
2. **WS backpressure:** lagged-receiver disconnect policy — right call vs unbounded buffering? Reconnect story for the web client?
3. **Scheduler races:** two replicas sweeping the same due market — do the idempotency keys + AdvanceMarket/ResolveMarket locks actually make double-fire impossible, or just unlikely? `sched:<market>:<event>` key reuse across retries after a *failed* attempt?
4. **Integrity-rule ordering:** replay short-circuits before integrity rules — is that the right semantic (a replayed vote shouldn't newly 429), and is the near-close boundary inclusive/exclusive consistently?
5. **Chart math:** collateral-weighted average in integer math — precision/overflow; NO-normalization correctness.
6. **PWA scope honesty:** is the dev-login/token posture safe enough for a local demo (token only server-side except the labeled dev box), and does the plan keep ALL money math server-side?
7. **No-VCS discipline:** any step that implicitly assumes git (scripts, tooling, CI references) that would break or mislead?
