# Grok — Phase 1 plan Round 2 VERIFY

Read `docs/reviews/p1r1-resolution.md` and `git diff 6b234af..HEAD` (plan + spec §7.5 + conversation-graph.mermaid). Checked against Phase 0 `services/converse/.../graph.py` only for corridor rewiring expectations (code change is Task 1.5).

## R1 FINDING VERIFICATION (mine)

| ID | Status | One line |
|---|---|---|
| **B1** consume-after-success | **VERIFIED** | Plan §1.5, spec §7.5, mermaid CF edge all say select under lock → core → consume only after 2xx; stable `idempotency_key=pending_action_id`; fault/retry tests named. Phase 0 `graph.py` still pre-consumes — **expected until Task 1.5 rewires** (plan owns that work). |
| **B2** in-tx authority | **VERIFIED** | PlaceTrade rules 1–8: open `TradeTx` → `market_for_update` + `user_voted` + `pool_for_update` → validate; Store is factories-only; freeze race test required. |
| **B3** SeedMarket | **VERIFIED** | `EnsureGenesis` + `SeedMarket` (SEED ledger + `Pool::new(S,S,fee)` reserves) + `AdvanceMarket` + use-cases-only seed.rs; house funded first. |
| **M1** concurrent idempotency | **VERIFIED** | `IdempotencyGuard::serialize_key` first (advisory lock — unique-violation can't recover mid-tx in PG); DuplicateKey→reload retained; dual-racer tests. |
| **M2** auth posture | **VERIFIED** | 127.0.0.1 default, `x-demo-token`/`DEMO_TOKEN` on write routes, user_id trusted-dev, real authn deferred Phase 2 (stated). |
| **M3** D21 extension | **VERIFIED** | Explicit ADR-deferral of `ExtendCloseOnce`; Phase 1 = `NeedsCuratorDecision` + admin resolve/void; backlog named. |
| **M4** deps-check | **VERIFIED** | Kind-aware script + `rg sqlx\|axum` on application/src (skip if absent); already shipped in Task 1.0. |
| **M5** Store god-facade | **VERIFIED** | Store = factories only; `MarketQueries` for lock-free reads; PreviewTrade no TradeTx; blanket `*Tx` impls. |
| **M6** e2e asserts | **VERIFIED** | Per-currency ledger sums; escrow == total YES == total NO (positions+pool); consume-after-txn ordering; malicious-router suite mandatory post-wire. |
| **M7** void settlement | **VERIFIED** | Void = `settle_market(holdings, 5000, escrow)` full holdings, same conservation. |
| **m1** Clock | **VERIFIED** | Plain sync `Clock` trait. |
| **m2** OpenAPI flake | **VERIFIED** | Offline exporter, `jq -S`, pinned generator, CI diffs both artifacts. |

## CODEX FIXES (architecture read)

| Item | Status | One line |
|---|---|---|
| **IdempotencyGuard placement** | **VERIFIED** | First call in every write tx via advisory xact lock; correct PG semantics (no catch-unique-in-same-tx fantasy). |
| **Store split / MarketQueries** | **VERIFIED** | Clean ISP: writes through locked `*Tx`, HTTP GET/preview through `MarketQueries`. |
| **ledger_apply account locks** | **VERIFIED** | FOR UPDATE in id order + aggregate-then-validate; double-spend contract. |
| **Settlement zero-legs + escrow_balance** | **VERIFIED** | Escrow from locked balance; omit zero legs; empty-all-zero → no ledger txn; 0/5k/10k tests. |
| **Bootstrap flow** | **VERIFIED** | EnsureGenesis → CreateUser → SeedMarket → Advance → CreditDeposit → CastVote, idempotent. |
| **Converse pg wiring realism** | **VERIFIED** | Plan §1.5: PgPendingStore consume-after-success, PgRecorder default, AsyncPostgresSaver, advisory turn lock, `user_channels` + by-channel lookup, deterministic AgentNode extractor, malicious-router on prod wiring. |
| **Position sell formula** | **VERIFIED** | Partial-sale cost relief + realized PnL + InsufficientShares specified. |
| **0003 account identity** | **VERIFIED** | Partial uniques + immutability trigger; get-or-create safe. |

## NEW FINDINGS

[m1] docs/plans/phase1-core-write-path.md §1.2: `VoteTx` and `DepositTx` trait aliases **omit `IdempotencyGuard`** while the locking-protocol prose and TradeTx/ResolveTx/SeedTx require `serialize_key` as step 1 of every write tx — implementers copying the alias literally will skip vote/deposit serialization. FIX: add `IdempotencyGuard +` to both aliases (and any other write `*Tx` missing it) before Task 1.2 build.

[m2] docs/plans/phase1-core-write-path.md PlaceTrade rule 1 still says `market()` in prose while the port is `market_for_update` — cosmetic; use the locked name everywhere.

[m3] services/converse `graph.py` still pre-consumes on confirm — not a plan regression (Task 1.5 owns rewire) but **do not ship 1.5 without deleting gate-side `consume` and the adversarial tests that assume pre-consume**; add core-500 / crash-before-consume cases from §1.5 to the suite.

No new mechanism side-effects of full consume-after-success or advisory-first idempotency that invalidate the design: concurrent double-confirm is serialized by thread lock + trade key lock; expiry-then-retry remains coherent; escrow complete-set invariant is now queryable and matches domain math.

## BUILD READ

| Slice | Ready? |
|---|---|
| Task 1.0 | Already green |
| Task 1.1 PlaceTrade/PreviewTrade | Yes |
| Task 1.2 (after m1 one-liner on VoteTx/DepositTx) | Yes |
| Tasks 1.3–1.6 | Yes |

FINAL VERDICT: sound-to-build
