BLOCKED

## Round-2 finding verification

### Round-1 carryovers that were PARTIAL or UNRESOLVED in round 2

1. **RESOLVED — r1#1, config outbox ordering.** D24 now asks the feasible pair of Pg proofs: writer B blocks behind writer A on `config_generation`, and a missed wake-up is healed independently from the authoritative generation.

2. **RESOLVED — r1#2, kill-switch linearization.** D25 separates trading-global, trading-market, and voting-market fences, takes the applicable shared fence only after downstream row admission and through commit, and forbids pause writers from taking downstream locks. A replay hit performs no new authoritative write and therefore need not take the fence; the remaining replay-precedence ambiguity is a new contract finding below.

3. **PARTIAL — r1#3, atomic snapshots and complete catalog.** Whole-snapshot validation, one generation, and the missing §10.2 key families are now present. The catalog nevertheless claims bounds, apply-to class, role, and max-delta for *each* key while omitting one or more of those for `min_fee_bps`, rep thresholds, `rep_score_min_pot_micro`, `daily_seed_budget_micro`, payout hold, sweep delay, cadence, feature flags, and faucet caps; NEW-2 also shows that a live fee input has no coherent apply/staleness policy.

4. **PARTIAL — r1#4, atomic audit of existing admin mutations.** D26's single-tx audit and durable-command model is sound in principle, and `AdminContext::Machine` preserves automatic paths. It still does not land against the actual call graph: the matrix omits several existing mutation use cases and HTTP handlers, and the new publication command has no owned consumer or response contract (NEW-4).

5. **RESOLVED — r1#5, RBAC and faucet containment.** The fail-closed capability layer, legacy-token removal, startup validation, explicit audit-read capability, and the two-factor `OPINIONS_ENV=staging && STAGING_FAUCET=1` mount rule close the finding.

6. **PARTIAL — r1#6, unwind lifecycle and non-negativity.** Booking an unavailable user debit to house cash and recording a separate non-cash claim preserves cash-ledger non-negativity. The receivable authority/collection model is not yet sufficient to implement identity 7, and a `Voided` unwind still fails to enumerate reversal of the already-committed neutral payout and dust.

7. **PARTIAL — r1#7, unwind authority, lineage, and derived state.** One transaction, a market-primary authority row, namespaced reversal keys, and unique entry lineage close the prior saga/exactly-once ambiguity. The enumerated terminal-void unwind still omits the prior payout/dust transaction and LP-PnL repair, while collection/write-off and compensating-realization lineage are not specified.

8. **PARTIAL — r1#8, ledger-exact invariants.** D27 correctly keeps receivables outside cash identity 3 and adds a seventh reconciliation. The stated receivable row and operations do not retain the links and movements that identity 7 needs, so the Pg sweep cannot compute the promised equality (NEW-5).

9. **RESOLVED — r1#10, replay determinism.** D28 now distinguishes network-free `PlannedOpportunity` artifacts from live `DecidedAction` traces and pins manifest/schema versions, ordering, RNG consumption, normalized outcomes, retry scheduling, and WS gap recovery.

10. **RESOLVED — r1#11, deterministic mid-payout crash.** D29 names an async application port, one in-transaction call site, a production Noop, a staging-only adapter, readiness, composition ownership, and fake/Pg transparency contracts.

11. **UNRESOLVED — r1#13, compiling file-disjoint wave.** Splitting 6.0a/6.0b improves sequencing, but the matrix is still not exhaustive: existing admin handler/use-case files needed for `AdminContext` and audit are unowned, the publication consumer is unowned, the preview version has no owned REST carrier, and the withdrawal guard names a use case that does not exist (NEW-2, NEW-4, NEW-6).

### Round-2 NEW findings

1. **RESOLVED — r2 NEW-1, fence convoy drain.** D25's three namespaces, after-row-lock acquisition, one-way pause-writer graph, exclusive-before-generation order, and 2,000-writer contract directly close the convoy and voting-coupling failures.

2. **PARTIAL — r2 NEW-2, saga-aware audit.** The audited durable-command boundary is the correct boundary for `publish_now`, replay, and refanout. The plan does not define or assign a consumer for publication commands and does not assign all existing admin mutations that must receive `AdminContext` plus in-tx audit (NEW-4).

3. **PARTIAL — r2 NEW-3, receivables versus cash identities.** Moving the claim to a non-cash subledger and using a house cash leg fixes identity 3. The proposed `(user, market, amount, status)` fact is not enough to trace shortfalls, collections, or write-offs, and its relationship to compensating realizations remains incomplete (NEW-5).

4. **RESOLVED — r2 NEW-4, deterministic artifact contract.** The manifest/trace split and exit criterion now describe artifacts that dry-run and replay can actually implement.

5. **RESOLVED — r2 NEW-5, crash-point seam.** `ResolutionCrashPoint`, its sole call site, Noop, armed adapter, readiness handshake, and explicit owners close the clean-architecture and ownership defects.

6. **UNRESOLVED — r2 NEW-6, exhaustive ownership matrix.** The matrix still omits required current files and one entire withdrawal surface; its “one owner per file” claim is false. The concrete omissions are listed in NEW-2, NEW-4, and NEW-6 below.

7. **RESOLVED — r2 NEW-7, durable proposal authority.** The proposal row now pins the immutable patch, base generation, principals, expiry, status, and resulting generation; confirmation locks proposal → fences → generation, revalidates, and applies once.

8. **PARTIAL — r2 NEW-8, fee preview coherence and change history.** Revision 3 consistently makes the pool fee new-market-only and deletes live repricing. The change-log DDL cannot represent a multi-key generation, the request path does not carry the stamped generation, and `min_fee_bps` can still change a live quote without triggering staleness (NEW-1 and NEW-2).

## NEW findings

### NEW-1 — BLOCKER — D24: `config_changes` cannot represent an atomic patch, and its hot-history contract is unbounded

**Scenario:** D24 defines `config_changes(generation pk, key, old, new, ...)`, but `SetConfig` is explicitly a multi-key typed patch with one generation. The second changed key in the same patch therefore violates the primary key; logging only one key instead makes selective staleness and emergency revert incomplete. The required query is by `(key, generation range)`, yet no such index, retention/archival boundary, or conservative behavior for a preview older than retained hot history is defined, so the immutable log also becomes an ever-growing scan surface.

**Fix:** Make the authority `config_generations(generation pk, ...)` plus `config_changes(generation fk, key, old, new, primary key(generation,key))`, add `(key,generation)` indexing, and require one row for every changed key in the patch transaction. Define a bounded hot-history/archival policy; if a supplied preview generation predates the hot watermark, `PlaceTrade` must conservatively return `StaleConfig`, while emergency revert can read the archived immutable history.

### NEW-2 — BLOCKER — D24/D25a/§2: the stamped version cannot reach `PlaceTrade`, and `min_fee_bps` silently changes confirmed economics

**Scenario:** Task 6.0a adds `TradePreview.config_version`, but neither `adapters/src/http/dto/market.rs` nor `adapters/src/http/routes/market.rs` has an owner in the exhaustive matrix, and D25a never defines a required execute-side version field. A REST/converse execution therefore has no specified generation to compare. Even if one is added, D25a's exhaustive 409 list contains only effective position-cap drift and flip-window drift; changing catalog key `min_fee_bps` changes the effective fee on an existing pool in both preview and execution, so a lexical “yes” can execute different fee/proceeds without 409.

**Fix:** Add an explicit expected preview generation (or a signed/stored preview id carrying it) to the public PlaceTrade contract, DTO/OpenAPI, pending action, command, request fingerprint, and every call site, with one named owner for both existing market files. Classify every effective-fee input: either stamp it immutable on the market or include it in the market/user-relevant change set; in particular `min_fee_bps` and any tier/discount input that can alter this user's quote must stale the preview.

### NEW-3 — BLOCKER — D25/D25a: pause/config validation has no pinned precedence over idempotent replay

**Scenario:** A trade commits successfully, then the market is paused or a relevant config key changes, and the client retries the same idempotency key. D25 says paused requests return 423 and D25a says PlaceTrade returns 409 iff relevant drift occurred, while the repository's money corridor requires an existing key to return the original receipt before row/fence work. Either literal implementation can reject a successful replay or, if the key is reused with a different payload/version, return another request's receipt without detecting key misuse.

**Fix:** Pin the sequence: serialize the idempotency key; on a hit, compare a persisted canonical request fingerprint (including market/user/action/amount and expected preview authority) and return the original receipt without pause/config checks; reject a fingerprint mismatch as an idempotency conflict. Only a miss takes row locks, shared fences, authoritative pause/config reads, and writes. Add replay-after-pause, replay-after-relevant-drift, concurrent retry, and same-key/different-version-or-payload fake+Pg contracts.

### NEW-4 — BLOCKER — D26/§2: `publish_now` has a durable command producer but no command authority schema, consumer, or compatible response path

**Scenario:** The existing `PublishDraft` saga is invoked synchronously and the periodic publisher consumes due drafts, not publication commands. W2 owns a new `publish_now_command.rs` and a handler swap, but neither `content/publisher.rs` nor `content/publish_draft.rs` is assigned for leasing/resuming those commands. A crash after command+audit commit therefore strands the command. The existing endpoint also returns HTTP 200 with a market id, while an authorization-only command has neither a completed market nor a specified 202/status/poll contract. Separately, the matrix omits `create_draft.rs`, `review_draft.rs`, `resolve_market.rs`, `routes/market.rs`, and `routes/social.rs`, even though their `/admin/**` mutations must receive the authenticated actor and write audit under their own final transaction locks.

**Fix:** Define the publication-command table, states, lease/retry/idempotency rules, result link, and HTTP response semantics; assign exactly one worker the current publisher/saga/model/DTO files and prove crash-after-authorize recovery. Expand the file matrix to every existing admin mutation handler and use case, including draft create/edit/approve/reject, market resolve/void, advance, and moderation, and add the promised per-entry-point failure-injection contracts.

### NEW-5 — BLOCKER — D27/D30: the receivable “fact” cannot support identity 7, collection/write-off, or realization repair

**Scenario:** D30 specifies only `(user, market, amount, status)` and mutates status on collection/write-off. There is no origin reversal transaction/entry link even though D27 requires one, no movement rows or collected/written-off amounts, no collection ledger transaction link, and no owners for collection/write-off use cases. One unwind can create multiple user shortfalls while its house entries aggregate to one house-account delta, so “each receivable's house leg equals the booked shortfall” is not testable from the described data. A status-only write-off also forgives the liability without defining the compensating realization/PnL fact, leaving the immutable leaderboard facts inconsistent with the economic outcome.

**Fix:** Use a receivable authority row linked uniquely to the unwind and origin reversal transaction plus append-only receivable movements (`opened`, `collected`, `written_off`) carrying amount, actor/audit, idempotency key, and any cash-ledger/realization transaction ids; derive outstanding rather than overwriting history. Reconcile sums per reversal transaction (not one aggregate house leg per row), define partial collection and write-off realization semantics, and assign collection/write-off application, Pg, fake, route, and contract files.

### NEW-6 — BLOCKER — §2/§3.7: the required withdrawal guard has no executable surface or owner

**Scenario:** W2 says to add the guard “in the deposit/withdraw use case,” and exit criterion 7(b2) requires an HTTP-visible 409. This repository has a deposit use case and `TxnKind::Withdrawal`/schema rows, but no withdrawal application use case or withdrawal route. No new withdrawal file, DTO, route, Pg role, fake, or contract is named, while real rails and withdrawal Playwright are explicitly out of scope. The exit test therefore cannot be implemented from the frozen skeleton or the wave ownership list.

**Fix:** Either add and assign a narrow Phase-6 withdrawal-command surface (with ordinary balance/idempotency semantics plus the receivable guard) and specify how smoke funds its non-rail side, or move the executable 409 proof to Phase 7 and test Phase 6 through a named `WithdrawalEligibility` application/Pg/fake contract. Do not leave an acceptance criterion dependent on a nonexistent use case.
