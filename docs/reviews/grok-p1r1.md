VERDICT: fix-first

Reviewed `docs/plans/phase1-core-write-path.md` against domain surface (`crates/domain`), converse gate (`services/converse/.../graph.py`), D12–D23, and spec §§5.5/5.6/7.5/10.3. Hexagonal intent is serious and mostly well-sequenced; several integrity holes would bake wrong use-case shapes if built as written.

## FINDINGS

[B1] docs/plans/phase1-core-write-path.md §1.5 + services/converse graph: pending is **consumed in the lexical gate before** `core.place_trade` returns — today `pending_gate` calls `store.consume` then `execute_pending` calls the client; a failed/timeout trade leaves the pending dead, the user cannot re-confirm, and `idempotency_key=pending_action_id` never gets a retry path that re-enters execute. FIX: move consume into the same core transaction as place/cast (or consume only after 2xx); gate may *select* the pending id but must not mark `consumed_at` until money/oracle write commits; document retry: re-send confirm with same pending id while unconsumed OR replay by key after partial success.

[B2] docs/plans/phase1-core-write-path.md §1.1 PlaceTrade rules: freeze/vote-gate checks are not mandated **inside** `TradeTx` after `pool_for_update` / market row lock — `Store::user_voted` / `market_by_ref` are CQRS-lite reads outside the UoW, so a user can pass the gate then race into the hidden window (D22) or vote-revoke edge once such exists. FIX: specify PlaceTrade sequence as open `trade_tx` → `market()` + `user_voted()` + `pool_for_update()` **all inside the tx** → quote → ledger → commit; forbid pre-tx authority checks except pure preview.

[B3] docs/plans/phase1-core-write-path.md §1.5 seed + Global Constraints SEED mapping: e2e seed must create a live market with pool inventory, but there is **no SeedPool/SeedMarket use case** and SEED is only `house −S · escrow +S` — missing `pool_reserves` mint of complete sets (yes=no=S) and pool OwnerType account linkage; raw “via use cases + ledger only” cannot invent a missing port. FIX: add `SeedMarket` (or extend admin advance) with explicit steps: market rows + SEED ledger + `save_reserves(Pool::new(S,S,fee))` + outbox; ban raw SQL in seed.rs for money/reserves.

[M1] docs/plans/phase1-core-write-path.md §1.1 rule 4 + §1.3: concurrent duplicate `idempotency_key` is incomplete — only `txn_by_key` hit → replay is specified; both racers can see miss then one unique-violation. FIX: require `ledger_apply` maps unique violation → `DuplicateKey`; use case **must** on DuplicateKey re-load receipt and return `replayed: true` (contract test: two concurrent identical PlaceTrade → one write, both receipts equal).

[M2] docs/plans/phase1-core-write-path.md §1.4 + deferred list: **no authentication** — `user_id` is a client-supplied UUID on every money/oracle route; deferred section never names auth. Acceptable for a localhost demo only if locked: FIX: state explicitly “Phase 1 is bind-127.0.0.1 + shared secret optional; client-supplied user_id is trusted-dev identity; real authn is Phase 2+”; refuse 0.0.0.0 default; consider requiring `x-demo-token` on write routes same as admin.

[M3] docs/plans/phase1-core-write-path.md §1.2 D21: `NeedsCuratorDecision` is only an error — D21 also requires **one voting-window extension** then curator decision when OI ≥ floor. FIX: either implement `ExtendCloseOnce` market event + flag `extension_used` column in 0002, or ADR “Phase 1 curator path = admin-only resolve/void, extension deferred” and drop the claim of full D21 fidelity.

[M4] docs/plans/phase1-core-write-path.md §1.0 `check_dependency_rule.py`: script only sees **direct** workspace edges via `cargo metadata --no-deps`; misses (a) kind filtering clarity (dev vs normal — document that both fail), (b) **type leakage** (sqlx types in application signatures) which needs clippy/API lint or “no sqlx in application” grep, (c) feature-unification footguns. FIX: extend CI with `rg 'sqlx|axum' crates/application` fail; parse `dep[\"kind\"]` explicitly; keep crate graph check as necessary but not sufficient.

[M5] docs/plans/phase1-core-write-path.md ports `TradeTx` supertrait bundle (7 roles): ISP is real at the *role* grain but every fake/Pg still implements the god-shaped `TradeTx` / `Store` facade — new read-only use cases shouldn't need `LedgerWriter`. FIX: keep role traits; make `Store` factory methods the only composition point; document blanket `impl<T: ...> TradeTx for T`; don't require PreviewTrade to go through TradeTx.

[M6] docs/plans/phase1-core-write-path.md §1.5 e2e: proves keyword router → preview → “yes” → positions, **not** extractor-quality or malicious-router-with-real-core; step 7 `sum(ledger_entries)=0` is necessary but weak (cross-currency zero can still be wrong without per-currency check); “escrow == pool value backing” is undefined. FIX: keep unit malicious-router test mandatory post-wire; e2e assert per-currency sums; define escrow invariant formula (e.g. escrow_usdc == Σ open user costs + pool residual collateral accounting) in the plan.

[M7] docs/plans/phase1-core-write-path.md §1.2 void settle at neutral 50/50: matches D21 current-state redemption (good) but **pool inventory + house LP PnL at void** needs explicit holdings inclusion (same as resolve) and fee/dust rules — easy to under-specify vs `settle_market`. FIX: void path reuses `settle_market(..., actual_yes_bps=5000)` with full holdings; test conservation.

[m1] docs/plans/phase1-core-write-path.md Clock uses `#[async_trait]` on a sync `now()` — noise; make it a plain trait.

[m2] docs/plans/phase1-core-write-path.md OpenAPI freshness: utoipa key order / rustc version can flake `git diff` — pin sort or normalize JSON before diff.

## CHECKPOINTS

1. **Txn boundaries / TOCTOU:** Incomplete as written — CQRS reads on `Store` invite pre-tx checks; mandate re-read under row locks inside `*Tx` (see B2). After that fix: one `*Tx` per use case is sound.
2. **ISP / TradeTx:** Role traits are minimal enough; `TradeTx` as supertrait alias is acceptable if blanket impl + Store factories avoid forcing every fake method onto every use case — god-risk is `Store`, not the roles. Contract suites catch semantic divergence, **not** isolation/locking (adapter-specific tests must stay mandatory).
3. **Idempotency races:** Not race-safe until DuplicateKey → reload is specified and dual-racer tested (M1). Single-threaded replay is fine.
4. **Trigger SQL:** Per-currency deferred sum is correct for multi-currency; empty-header deferred check is correct if entries insert before commit; append-only is fine for Phase 1 (no mutation migrations). Cross-currency overall-zero alone is rightly rejected by grouping.
5. **Ledger mapping SELL / escrow:** Quote math + pool-favoring rounding should keep proceeds ≤ available if seed/buy accounting matches; **state the invariant** and e2e-assert per market. Seed must create matching reserves (B3) or escrow≥0 proofs are vacuous.
6. **OpenAPI gate:** Works if JSON is normalized; raw diff can false-fail (m2).
7. **Coverage + DB:** CI with always-on Postgres service can hold 90% on adapters; document that local skip-without-DATABASE_URL must not be how CI measures. Integration-heavy coverage is OK if contract suites run against Pg.
8. **E2E corridor honesty:** Happy path proves lexical confirm for iMessage **if** only `execute_pending` calls `place_trade` and router never does; REST `/trades` still bypasses the gate by design. Malicious-router unit test must remain post-wiring; e2e alone does not replace it. **Consume-before-success (B1) undermines corridor integrity under faults.**

## WHAT CHECKS OUT (signal, not silence)

- Layering story domain ← application ← adapters ← main matches D12 and is the right Phase 1 cut.
- Deferred per-currency triggers + append-only entries match R2/R3 ledger lessons.
- D22 freeze at application (`now < tally_hidden_at`, Live only) is the right enforcement locus; voting through Closing is correct.
- Outbox append in-tx with relay deferred is the right Phase 1 split.
- Neutral void redemption aligns with D21 (not historical reverse replay).
- Python OpenAPI model generation closes D15 directionally.
- Phase 0 domain surface (`quote_*`, `Transaction`, `transition`, `settle_market`, `score_vote`) is sufficient for the planned use cases once Seed is added.

## BUILD READ

Do not start Tasks 1.1–1.5 until B1–B3 plan text is amended (and M1–M3 decided). Task 1.0 triggers/deps-check can proceed in parallel.
