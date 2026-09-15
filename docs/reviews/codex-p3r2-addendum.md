# Phase 3 Round 2 — Codex verification addendum

[VERIFIED 1] The resolve path now has `save_vote_scores(batch)` and `append_batch`, explicitly fixes statement count with respect to voter cardinality, and makes the 1,000-voter `< 2s` assertion a hard passing gate; the later report instruction does not soften a failed test.

[VERIFIED 2] The sweep now commits the converged report in Tx A before the existing `ResolveMarket` transaction begins, and `ResolveOutcome::HeldForReview { due_at }` provides the typed hold result used by `SweepReport.held` rather than inferred state.

[VERIFIED 3] Validation now covers fee and ppm bounds, seed-time `base_fee >= min_fee`, positive/nonnegative fields, explicit zero semantics for the rep floor and flip window, a positive leaderboard minimum, nonempty enabled device secret, CIDR parsing, startup failure tests, and the stated multiplier exception up to 100,000,000 ppm.

[VERIFIED 4] `realizations` now carries the composite `FOREIGN KEY (outcome_id, market_id) REFERENCES outcomes(id, market_id)`, so a realization cannot pair an outcome with the wrong market.

[VERIFIED 5] Migration 0005 now adds indexed `markets.settled_at`, settlement writes it atomically beside `lp_pnl_micro`, the LP query uses its half-open time window, and the contract explicitly includes void settlements.

[VERIFIED 6] `last_buy_at` is specified on lock-free `MarketQueries` and authoritative `TradeTx`, PreviewTrade receives the same injected `Clock`, and both paths test the strict `now - last_buy_at < window` boundary with equality classified as not a flip.

ADDENDUM VERDICT: sound-to-build
