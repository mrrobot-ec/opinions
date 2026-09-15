# Phase 3 Round 2 — Codex verification

## Finding verification

[VERIFIED B1] The K table is correct under decimal round-half-up: H10 `933_032.9915… → 933_033`, H20 `965_936.3289… → 965_936`, and H40 `982_820.5985… → 982_821`; the enum removes runtime roots/K=UNIT, operands and `i128` arithmetic are bounded, and exact recurrence vectors replace the false ±1 claim (H20 endpoints are `500_004` and `499_996`).

[REGRESSED B2] Backfill, creation-path guarantee, canonical preflight locking, and set-based rep writes are specified, but “scores (bulk as before)” is false against the current one-row `save_vote_score` port and tier events still use one-row `append`; the 1,000-voter path therefore retains N+1 writes, and “report if >2s” is not a passing performance gate.

[VERIFIED B3] `PreviewTradeCmd.user_id`, the authoritative in-tx rep read after the class-2 user lock, one `effective_fee` policy, explicit quote fee, canonical `Pool.fee`, and preview/trade-row/ledger/Converse equality tests close the original fee divergence and stale-tier paths.

[REGRESSED B4/M2] The two settlement authorities, both negative bypass cases, atomic delayed hold, tightened flag predicate, due index/arm, and held counters are present, but the plan never defines whether the report transaction commits before invoking the existing new-transaction `ResolveMarket`; an uncommitted report is invisible to that use case, and no Held receipt/return variant is specified for incrementing `SweepReport.held`.

[VERIFIED B5/M4] Immutable signed realization facts cover sell, settlement, void, and zero-payout losses under a replay uniqueness key; half-open-window SUM is implementable on the new time index without payout/sell double counting, and the fees-account summary is honestly split into trade fees and payout dust.

[VERIFIED M1] `market_position_cost` sums both outcomes under the market lock, enforces aggregate cost plus buy gross, explicitly includes fees, and leaves sells exempt under the non-increasing proportional-relief invariant.

[VERIFIED M3] `VoteStats` now carries both observed denominators, all missing thresholds/ages/horizons are represented, close-anchored prior/final windows and zero-baseline behavior are defined, and IPv4 `/24`, IPv6 `/64`, coverage, strength, and config-version audit fields are explicit.

[REGRESSED M5] Fail-fast constructors and array ordering are stated, but validation remains incomplete: no bounds are specified for `min_fee_bps`, share/coverage ppm fields, or base-fee ≥ floor; `min_votes_for_ratios`, secret/CIDR parsing, and whether zero is legal for the rep pot, flip window, and leaderboard minimum are also left undefined.

[VERIFIED M6] The seed check is inside the transaction under the global class-3 lock, uses positive `max_loss_micro` with `sum <= -max_loss`, audits force in-commit, and explicitly accepts seed-time-only protection while deferring live-market exposure control.

[VERIFIED m1] Forwarded addresses require a trusted direct peer and use the last-untrusted-hop rule, device identifiers are HMACed instead of stored raw, and report/copy strength labels describe subnet and client identifiers as weak heuristics rather than identity proof.

## New blockers introduced or exposed

[N1] Migration 0005 gives `realizations.market_id` and `outcome_id` independent foreign keys, so it permits an outcome from another market and regresses the schema's existing cross-market invariant. FIX: replace the outcome FK with `FOREIGN KEY (outcome_id, market_id) REFERENCES outcomes(id, market_id)` and contract-test the rejection.

[N2] `lp_pnl_sum(window)` is defined over markets “resolved in the window,” but migration 0005 adds no settlement/resolution timestamp and `markets.created_at` is not that timestamp; neither the query nor an O(window) index is therefore specified. FIX: add an indexed `lp_pnl_realized_at`/`settled_at` written beside `lp_pnl_micro` in the settlement transaction, and state whether voids participate.

[N3] The new flip-window rule requires PreviewTrade to compare `last_buy_at` with an authoritative current time, but PreviewTrade has no `Clock` and the only sketched `last_buy_at(&mut self, …)` role fits TradeTx rather than lock-free `MarketQueries`. FIX: add the read to both authority surfaces, inject the same Clock into preview, and pin the exact inclusive/exclusive window boundary so preview and execution cannot diverge.

FINAL VERDICT: fix-first
