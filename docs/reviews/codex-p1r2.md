R1 FIX VERIFICATION:

1. codex B1 — VERIFIED: `ledger_apply` now locks all touched accounts in UUID order, aggregates before checking post-balances, preserves the External exception, and requires a two-connection double-spend test.
2. codex B2 — REGRESSED: `market_for_update` and the freeze-race test close the trade TOCTOU, but the exact PlaceTrade sequence still says `market()` and omits `serialize_key`, while CastVote has neither a `Clock` contract nor an in-lock `now < closes_at` check, so a lagging `Closing` row can accept a post-cutoff oracle write.
3. codex B3 — REGRESSED: the advisory guard is the right race-free mechanism, but the literal `VoteTx` and `DepositTx` aliases omit `IdempotencyGuard`, and the business rules still prescribe the impossible post-unique-violation reload instead of guard → replay lookup → write.
4. codex B4 — VERIFIED: the locked actual escrow read feeds `settle_market`, all zero-valued payout/dust/escrow legs are explicitly omitted, the all-zero case writes no forbidden empty header, and 0/5000/10000 tests are required.
5. codex B5 — REGRESSED: the named bootstrap use cases exist, but `Store` has no `bootstrap_tx`, `OwnerRef` has no pool owner, no port creates the `pools` row/pool ledger account before `save_reserves`, and SeedMarket both goes Live internally and is followed by `AdvanceMarket` to Live.
6. codex B6 — VERIFIED: Task 1.5 now schedules PostgreSQL recorder/pending/checkpointer/dedupe/turn-lock wiring, channel identity, deterministic extraction, real core-client use, and the malicious-router production-wiring test.
7. codex M1 — REGRESSED: migration 0003 adds the intended partial unique indexes and immutability trigger, but it does not constrain owned accounts to non-null `owner_id`, so PostgreSQL NULL-distinct semantics still permit unlimited duplicate `(owner_type, NULL, currency)` rows.
8. codex M2 — REGRESSED: the boxed object and consuming `commit` design remains viable, but the proposed `Future<Output = Box<dyn TradeTx + '_>>` bound does not compile (`E0637`, `'_` is reserved in that position).
9. codex M3 — VERIFIED: `MarketQueries::list_markets` and an application-layer `AdvanceMarket`/`AdvanceTx` with domain transition plus outbox now make both routes implementable without HTTP-layer business logic.
10. codex M4 — REGRESSED: vote facts now leave the adapter and `ResolveConfig::oi_floor` is typed, but the locked D21 extension is merely called an ADR-deferment without changing `docs/decisions.md`/the spec or adding the required extend-once persistence.
11. codex M5 — VERIFIED: `position_for_update`/`save_position` and the stated floor-rounded proportional cost-relief formula correctly cover partial sales, full sales, realized PnL, and insufficient shares.
12. codex M6 — REGRESSED: offline sorted OpenAPI and no-timestamp two-artifact diffing are correct, but the generator pin is scheduled only in Task 1.5 after Task 1.4 must run it, the script invokes the venv tool bare rather than through `uv`, and the CI amendment does not specify installing/syncing that pinned Python tool before freshness checks.

GROK ENGINEERING VERIFICATION:

1. consume-after-success — VERIFIED: selection happens under the whole-turn advisory lock, core uses stable `pending_action_id` idempotency, failures keep pending active, and crash-after-2xx replays before consumption.
2. MarketQueries split — VERIFIED: lock-free views are separated from transaction factories and PreviewTrade is explicitly read-only; handlers can bind `Store` and `MarketQueries` independently.
3. position formula — VERIFIED: the formula preserves nonnegative remaining shares/cost, relieves all remaining cost on a full sale, and accumulates realized PnL from net proceeds.
4. coverage gate — REGRESSED: task-local `cargo llvm-cov` commands are feasible, but no task updates `just coverage`/CI from its current domain-only recipe to execute the promised application and adapters 90% gates.

NEW FINDINGS:

[B1] — docs/plans/phase1-core-write-path.md Task 1.1 contract harness: The newly proposed async factory signature cannot compile because `'_` is illegal in the future output bound. FIX: Make each contract generic over `S: Store + ?Sized` and open one or two transactions through `&S`, or introduce an explicit lifetime/HRTB boxed-future factory whose returned trait-object lifetime is named.

[B2] — docs/plans/phase1-core-write-path.md Tasks 1.1–1.2 idempotency: The executable aliases and rules contradict the global guard-first protocol, leaving vote/deposit races unsynchronized and instructing PlaceTrade/CastVote to recover inside an aborted PostgreSQL transaction. FIX: Add `IdempotencyGuard` to every write alias and make every rule sequence `open tx → serialize_key → load original → on miss acquire authority/data locks → write → commit`, deleting DuplicateKey-reload language for identical keys.

[B3] — docs/plans/phase1-core-write-path.md Task 1.2 bootstrap: The new seed API remains unimplementable because there is no bootstrap factory, pool owner/account identity, or pool-creation operation, and its lifecycle is internally contradictory. FIX: Add `bootstrap_tx`, `OwnerRef::MarketPool(MarketId)`, and an explicit create-pool-with-reserves port; have SeedMarket leave one stated lifecycle state and make `seed.rs` advance only the remaining legal edges.

[B4] — docs/plans/phase1-core-write-path.md Task 1.2 `AdvanceMarket`: Passing arbitrary `MarketEvent` through this generic admin use case permits `Resolve`, `Pay`, `VoidLowParticipation`, or `VoidByAdmin` to change state without settlement, scoring, or payout. FIX: Whitelist only nonfinancial lifecycle events in `AdvanceMarket` and route every resolve/pay/void event through the conservation-checked ResolveMarket settlement transaction.

[M1] — docs/plans/phase1-core-write-path.md migration 0003: The owned-account unique index is ineffective for null owners because ordinary PostgreSQL unique indexes treat NULL values as distinct. FIX: Add a CHECK requiring non-null `owner_id` for user/pool/escrow and null `owner_id` for fees/house/external (or use `NULLS NOT DISTINCT` with equivalent ownership-shape checks).

FINAL VERDICT: fix-first
