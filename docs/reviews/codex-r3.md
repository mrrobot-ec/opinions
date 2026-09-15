R2 FIX VERIFICATION:

B1 aggregate-then-validate apply — VERIFIED: Task 2 aggregates every account's entries in checked i128 before per-currency validation or any write, includes the exact A:-7/A:-6/B:+13 atomicity regression, and requires a repeated-account conservation/atomicity property test.

B2 per-currency ledger + one External + four-leg conversion — REGRESSED: Currency, per-currency apply validation, DuplicateExternal, and the four-leg conversion are correct in the domain plan, but Task 7/ER still permit multiple External accounts per currency and the planned Phase-1 deferred trigger still checks only the transaction-wide scalar sum.

B3 pending uniqueness + atomic consume + advisory lock — REGRESSED: atomic conditional consumption and a cross-replica lock are described, but the unique predicate treats every unconsumed expired row as active forever, and Task 8 specifies neither the advisory-lock implementation nor the required multi-replica concurrency/expiry tests.

M1 closed AMM caps + corrected tests — REGRESSED: result-cap rejection and bounded isqrt generation are correct in prose, but the retained `sell_cannot_drain_pool` test still excludes `InputTooLarge` even though its fixed input exceeds `MAX_AMOUNT` and deterministically returns that error.

M2 composite market FKs — REGRESSED: Task 7's critical SQL correctly adds composite candidate keys/FKs, but the ER file that Task 7 must implement 1:1 still omits `POOL_RESERVES.market_id` and does not encode its two composite FKs.

M4 Voided lifecycle + positive min-votes — REGRESSED: the status/check and basic void edges landed, but Task 4 publishes conflicting MarketState enums, omits `Resolved -> Voided` while spec §4.2 promises admin void from any pre-paid state, and incorrectly claims reversal replay remains non-negative after downstream withdrawals.

M5 hardened mutation gate + pinned tools + audit — REGRESSED: exit handling, invalid-summary rejection, and `audit` in `just ci` landed, but `@0.6`/`@25`/`@1` plus rolling `stable` are not exact pins and the promised mutation-gate fixture tests have no file, recipe, or CI invocation in Task 0.

M6 unpartitioned retention — REGRESSED: spec §10.3 and Task 7 now choose unpartitioned retention correctly, but the ER still says monthly partitions and PLAN's final open-risk line still requires agent-table partitioning before production.

NEW FINDINGS:

[B1] — PLAN.md Task 7 pending-actions index: `where consumed_at is null` permanently blocks a thread after its pending row expires because the consume UPDATE refuses expired rows without marking them consumed. FIX: Under the same transaction-scoped per-thread advisory lock, atomically mark any expired unconsumed row expired/consumed before inserting or reading the next action, and test expiry plus concurrent inserts across two connections.

[B2] — PLAN.md Task 3 Step 1: the shown `sell_cannot_drain_pool` assertion cannot pass because `i64::MAX / 4 > MAX_AMOUNT`, so the quote returns `InputTooLarge`, which the assertion rejects. FIX: Add `Err(AmmError::InputTooLarge)` to the checked alternatives (or use an in-cap drain case) in the actual test snippet, not only the surrounding prose.

[B3] — PLAN.md Task 4 / spec §§4.2 and 10.2: reversing historical market transactions is not generally non-negativity-preserving when a credited seller has subsequently withdrawn or spent those proceeds, so the promised void flow can become unrepresentable under Task 2. FIX: Define void as current-state neutral redemption from funded market escrow (with Task 6 conservation/dust), or lock every market-derived credit until void risk ends; do not claim unrestricted historical replay is safe.

[M1] — PLAN.md Task 7 + docs/er-diagram.mermaid ledger accounts: the persistent schema has no one-External-per-currency constraint, so SQL can create two contra accounts and invalidate reconciliation despite the domain's DuplicateExternal check. FIX: Add `create unique index ... on ledger_accounts(currency) where owner_type = 'external'` to the DDL/ER contract and make the Phase-1 deferred balance trigger validate sums grouped by transaction and currency.

[M2] — PLAN.md Task 0 mutation gate: the plan claims fixture coverage and exact tool stability without creating/running those tests or pinning exact patch versions, so the JSON gate can drift or regress without CI noticing. FIX: Name the fixture/test files in Task 0, add their test command to local and hosted CI, and pin the compiler, install action, and each parsed-output tool to immutable exact versions/SHAs.

FINAL VERDICT: fix-first
