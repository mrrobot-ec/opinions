# Phase 1 plan — round 1 resolution map

Both reviewers verdicted **fix-first** on the Phase 1 plan ([grok-p1r1.md](grok-p1r1.md), [codex-p1r1.md](codex-p1r1.md)). Every finding applied to `docs/plans/phase1-core-write-path.md` (and spec/graphs where the finding touched locked design):

| Finding | Resolution |
|---|---|
| grok B1 — pending consumed before core success (confirm eaten on failure) | **Consume-after-success**: gate selects; `consumed_at` written only after core 2xx; stable `idempotency_key = pending_action_id` makes crash/retry replay-safe. Plan §1.5, spec §7.5, both graph files |
| grok B2 / codex B2 — authority checks outside the transaction (TOCTOU) | Fixed sequence: `serialize_key` → `market_for_update` → `pool/position_for_update` → writes; `Store` stripped to tx factories; lock-free reads moved to `MarketQueries` (previews/views only). PlaceTrade rules 1–8 rewritten |
| grok B3 / codex B5 — seed unimplementable (no use cases; house unfunded) | `EnsureGenesis` (idempotent External→House capitalization), `CreateUser` (+channel link), `SeedMarket` (market rows + SEED ledger + complete-set reserve mint + outbox), `AdvanceMarket`; seed.rs is use-cases-only and idempotent |
| codex B1 — unlocked ledger accounts allow concurrent double-spend | `ledger_apply` locks touched account rows FOR UPDATE in id order, validates aggregated post-balances under locks; two-connection double-spend contract test |
| codex B3 / grok M1 — idempotency check-then-insert not race-safe in PG | `IdempotencyGuard::serialize_key` (`pg_advisory_xact_lock(hashtext(key))`) is step 1 of every write tx; read-or-create after the lock; concurrent-duplicate tests for trade/vote/resolve/deposit |
| codex B4 — settlement writes forbidden zero legs; escrow assumed not read | `SettlementIo::escrow_balance` (locked) feeds `settle_market`; zero-valued legs omitted; all-zero → no ledger txn; tests at 0 / 5000 / 10000 bps |
| codex B6 — converse demo runs on memory stores with no identity | Task 1.5 expanded: `pg_stores.py` (PgPendingStore w/ consume-after-success, PgRecorder default, AsyncPostgresSaver, webhook dedupe, advisory turn lock), `user_channels` (0003) + `GET /users/by-channel`, deterministic regex extractor via the AgentNode seam, malicious-router suite re-run on production wiring |
| codex M1 — account get-or-create had no unique identity; mutable reclass | Migration 0003: partial unique indexes (owned + singleton classes), immutability trigger on owner/currency |
| codex M2 — contract harness signature couldn't type-check | Async boxed-tx factories; role helpers take `&mut (dyn Role + '_)`; concurrency suites take two factories |
| codex M3 — routes without ports/use cases (list, admin advance) | `MarketQueries::list_markets` + `AdvanceMarket` use case behind the admin route |
| codex M4 — scoring delegated to adapter; untyped config | `vote_facts` out / `save_vote_score` in; `domain::scoring` runs in the use case; typed `ResolveConfig { oi_floor }`; D21 extension explicitly ADR-deferred (grok M3 concurs) |
| codex M5 — position sell accounting unwritable | `position_for_update`/`save_position` + the exact partial-sale formula (cost relief, realized PnL, InsufficientShares) |
| codex M6 / grok m2 — OpenAPI pipeline nondeterministic/unbuildable order | Offline exporter (`adapters` example bin), `jq -S`, `--disable-timestamp`, generator pinned in pyproject, CI regenerates + diffs BOTH artifacts, sqlx-cli pinned |
| grok M2 — unstated no-auth | Stated posture: 127.0.0.1 default bind, `x-demo-token` on write routes, user_id = trusted dev identity, real authn named Phase 2 |
| grok M4 — deps-check insufficiency | Script checks normal+dev kinds; `rg 'sqlx|axum' crates/application/src` gate added |
| grok M5 — Store god-facade risk | Store = factories only; blanket impls; PreviewTrade never opens a TradeTx |
| grok M6 — weak e2e asserts | Per-currency ledger sums; escrow invariant defined (`escrow == total YES == total NO` incl. pool reserve); consume-after-txn ordering asserted |
| grok M7 — void underspecified | Void = `settle_market(holdings, 5000, escrow)` verbatim, full holdings, same conservation |
| grok m1 — async Clock noise | Plain sync trait |

Task 1.0 was cleared by grok's build-read and is **already built green** (triggers + deps-check, commit on main).
