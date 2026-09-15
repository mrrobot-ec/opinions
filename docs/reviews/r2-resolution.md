# Review round 2 — resolution map

Both reviewers re-verified the R1 fixes and verdicted **fix-first** on new findings + incomplete landings. Full reports: [codex-r2.md](codex-r2.md), [grok-r2.md](grok-r2.md). All items below applied by the coordinator.

## Incomplete R1 landings, now completed

| Item | Resolution |
|---|---|
| grok B1 / codex M3 — Task 8 skeleton still routed `load_session → router`, Intent kept CONFIRM/CANCEL | Task 8 topology now `load_session → pending_gate → (execute \| clear \| reprompt \| router)`; CONFIRM/CANCEL **removed from the Intent enum** (execution vocabulary doesn't exist LLM-side); adversarial test: a malicious always-trade fake router provably cannot reach the execute node |
| grok B4 / codex m1 — §3.2 and D4 still claimed 75/25 "defuses" bag voting | Retracted in both; scoring rewards crowd-reading, the §3.4 eligibility stack is the bag-defense; tie definition added to published-copy requirements |
| grok M4 / codex m1 — residual "event-sourced" language (spec header, §5.5, §9, README) + stale README coverage claim | All four sites now say state-primary + transactional outbox; README coverage claim matches the stable/nightly/mutation policy |
| grok M5 / codex M3 — votes lacked `pending_action_id`; trades lacked `txn_id`; `pending_actions.run_id` FK implicit | `votes.pending_action_id` (unique) + `trades.txn_id` + explicit `pending_actions.run_id` FK in DDL + ER |
| codex B1(r1 landing) — single-External and currency semantics unproven | See new findings below (B2) — currency is now a domain dimension with per-currency balancing and one External per currency |

## New findings, resolved

| Finding | Severity | Resolution |
|---|---|---|
| codex B1 — duplicate-account entries computed from stale balances can mint money (`A:-7, A:-6, B:+13` vs `A=10`) | Blocker | `Balances::apply` **aggregates per account in i128 before validation**; exact regression test + repeated-account property test added (PLAN Task 2) |
| codex B2 — transactions balanced on one scalar could convert `usdc_credit` → `usdc`; multiple Externals defeat the contra invariant | Blocker | `Currency` added to the domain model; `Transaction::new` keeps the structural all-zero check, `apply` validates **per-currency zero sums** (`UnbalancedCurrency`); credit conversion modeled as a four-leg per-currency-balanced transaction; `DuplicateExternal` enforced (one External per currency); `internal_total(currency)` invariant per currency |
| codex B3 — single-flight had no cross-replica enforcement; pending index non-unique; no atomic consume | Blocker | Partial **unique** index (one active pending per thread); consumption is `UPDATE … WHERE consumed_at IS NULL AND expires_at > now() RETURNING`; per-thread Postgres advisory lock for cross-replica turn serialization (spec §7.5, Task 7 DDL, graph) |
| codex M1 — caps not closed under state transitions; two test bugs | Major | Quotes reject results whose reserves would exceed `MAX_RESERVE` (cap is an invariant of reachable states); `sell_cannot_drain_pool` accepts `InputTooLarge`; isqrt generator bounded to `1..=u64::MAX` + explicit edge cases (Task 3) |
| codex M2 — separate market/outcome FKs allowed cross-market rows | Major | `outcomes unique(id, market_id)` + composite FKs on trades/votes; `pools unique(id, market_id)` + pool_reserves carries `market_id` with both composite FKs (Task 7) |
| codex M4 / grok(r1 B3 landing) — void rule had no executable lifecycle; `min_votes` defaulted to 0 | Major | `Voided` terminal state + `VoidLowParticipation`/`VoidByAdmin` events with legal-edge table (Task 4); refund = §10.2 full ledger unwind (reversal replay, Phase 1); `min_votes_to_resolve` has `check (> 0)` and no default (Task 7); spec §4.2 updated |
| codex M5 — mutation recipe masked failures; audit missing from `just ci`; unpinned tools | Major | Exit-code-aware mutants recipe (only 0/2 accepted); gate script rejects timeouts/baseline failures/unknown schemas + fixture tests; `just ci` includes audit; taiki-e installs pinned (Task 0) |
| codex M6 — promised partitioning incompatible with PK/global dedupe | Major | Decision: agent tables stay **unpartitioned** with scheduled retention/archival/redaction jobs; promise corrected in §10.3 + DDL comment |
| grok M1 — void rule itself gameable (suppress to void / pad to resolve) | Major | **D21 anti-void rules**: auto-void only if participation < threshold AND OI < floor; above floor → one window extension then curator decision; suppression/padding feed the anomaly sweep (spec §3.4) |
| grok M2 — buys-frozen window still allowed informed dumping into LP | Major | **D22 amended: full trade freeze during the hidden window, both sides** (10 min daily / 2 min flash, config); "price keeps trading" marketing line replaced by "fair quiet close" (spec §3.4, ER note) |
| grok M3 — allowlist normalization unspecified; tapbacks would nuke pending actions | Major | Normalization pipeline specified (lowercase → strip punct/emoji → collapse ws → drop politeness tokens); empty/tapback/emoji-only → **re-prompt keeping pending**; only substantive text auto-cancels (spec §7.5, graph, Task 8 tests) |

## Build-start reads from R2

- grok: Tasks 0–6 (domain math) sound; Task 7 after M5-equivalents; Task 8 after gate fixes — all now applied.
- codex: VERIFIED on settle-completeness, vote-seq allocation, deferred-trigger gating, dust contract, just syntax, rounding bound, scoring proptest, async seam, integer score schema.
