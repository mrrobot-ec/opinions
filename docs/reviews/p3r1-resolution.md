# Phase 3 plan — round 1 resolution map

codex verdict **BLOCKED** ([codex-p3r1.md](codex-p3r1.md)); grok verdict **fix-first** ([grok-p3r1.md](grok-p3r1.md)). The plan was rewritten wholesale; every finding maps below.

| Finding | Resolution in the revised plan |
|---|---|
| codex B1 — EWMA math false as written (floor drift 5–29 µ, ±1 claim untrue; no root algorithm; K=UNIT freeze risk) | `HalfLife` enum with **precomputed pinned K_PPM** (933_033/965_936/982_821); round-half-up; i128 checked; operands validated 0..=1_000_000; **golden integer recurrence vectors** replace the ±1 claim; no runtime root; K<UNIT by construction |
| codex B2 / grok M7 — rep rows missing; deadlockable lock order; N+1 at 1k voters | 0005 backfill + `insert_user` creates rep row (contract-tested); preflight `voter_ids` → canonical-UUID-order `reps_for_update` **before** `market_for_update` (same global user→market order as trades); set-based `save_reps`; 1,000-voter timed Pg fixture with a report-if-slow rule |
| codex B3 — previews discard user; stale-tier read; fake persists per-user fee as market fee | `PreviewTradeCmd.user_id` threaded (REST + converse); PlaceTrade reads rep in-tx after its user lock; **`domain::fee_policy::effective_fee` is the single authority**; quote functions take an explicit fee param while `Pool.fee` stays canonical in `pool_after`; four-combo preview==execute==ledger tests |
| codex B4 / grok B3-adjacent — two flagged-pot bypasses; non-atomic hold; report race | Settlement from `Resolving` requires exactly (pass-report ∧ unflagged) or curator override; **no report → 423 UnderReview** even for admin; hold = one tx (`sweep:` command + transition + `integrity_due_at`); report insert `ON CONFLICT DO NOTHING` then act on the stored row; tightened flag predicate; both bypass paths have negative tests |
| codex B5 / grok M2 — trader PnL not derivable; gross ≠ PnL; losses invisible | **`realizations` table** — immutable signed facts written in the same tx as sell/settlement/void (zero-payout losers included), `unique(txn_id,user,outcome)`; leaderboard = SUM over half-open window; "settled PnL, not mark-to-market" in the contract |
| codex M1 — cap only counted one side | `market_position_cost` sums both outcomes under the market lock; metric named "aggregate open cost basis per market, fees included"; opposing-side boundary test |
| codex M2 — Resolving unschedulable; same-tick review; no held accounting | `markets.integrity_due_at` + partial index; due arm `integrity_due_at <= now`; cascade can't spin (future due-time); `SweepReport { held, swept_pass, swept_flag }` |
| codex M3 — VoteStats/config incomplete; IPv6 undefined | `subnet_observed/device_observed` + coverage rule; device threshold + young-age + prior-horizon + zero-baseline defined; windows anchored at locked `closes_at`; IPv4 /24 + IPv6 /64 stated; per-check jsonb `{name,value,threshold,coverage,strength,note,config_version}` |
| codex M4 — indexes don't serve reads; fee total dishonest | `realizations_window_idx`, `ledger_txn_kind_time_idx`, `ledger_entries_account_time_idx`; fee summary = "fees-account revenue" split by source (trade vs dust) via txn kind |
| codex M5 — configs unvalidated, silent defaults | Validated constructors that **fail startup** on present-but-invalid env; all cross-field invariants enumerated (ascending thresholds, cap monotonicity, discount ≤ 10_000, min_fee, nonzero windows); saturation behavior tested |
| codex M6 / grok M3 — kill-switch raceable; sign convention; seed-only unstated | Class-3 advisory lock inside the seed tx; positive `max_loss_micro` with `sum ≤ −max_loss`; window = markets **resolved** in window; force override audited in-commit; seed-time-only limitation stated in the task body + admin copy with the Phase 6 follow-up named |
| codex m1 / grok M4 — XFF forgeable; "device_hash" wasn't a hash; overclaim risk | `TRUSTED_PROXY_CIDRS` peer check + last-untrusted-hop rule + spoof test; HMAC-SHA256(secret, id) stored; labels `ip_prefix_proxy`/`client_asserted_device_id`; web copy "heuristics, not a guilt finding" |
| grok B1 — fee discounts subsidize wash churn | `is_flip_within_window` in `effective_fee`: sells closing shares bought within `discount_flip_window_secs` pay base fee; anti-churn rule published on the scoring page; test pinned |
| grok B2 — EWMA tier farming via thin markets | Rep quality floor: updates only when `votes ≥ min_votes_to_resolve && escrow ≥ rep_score_min_pot_micro`; scores still recorded; documented |
| grok B3 — false positives lock capital | Verdict requires **≥2 flagged checks** (single flag = pass-with-note, settles); strengths recorded; published copy sets expectations ("a few minutes"); scheduled due-time honors the stated 2–5 min |
| grok B4 / m1 — curators can't find flags; notifications deferred blindly | `GET /admin/markets/flagged` (with reports) ships in 3.2; user notifications explicitly Phase 4; admin discoverability NOT deferred |
| grok M1 — voter board rewards volume | `avg_score_bp` + `markets_scored ≥ leaderboard_min_scored` + tier ≥ 1; formula published |
| grok M5 — scoring page under-specified | `docs/copy/scoring.md` source of truth with the exact formulas/rules; page renders it (3.4 checklist) |
| grok M6 — tier on tape? | Explicit non-goal: no tier badges on the public tape |

R2 = both reviewers verified this revision. **grok: sound-to-build** ([grok-p3r2.md](grok-p3r2.md)) — three doc notes applied (anti-churn taxes the exit leg and says so; `last_buy_at` doc comment; integrity reports are admin-only, never public — a public per-check breakdown would be a calibration oracle). **codex: fix-first** ([codex-p3r2.md](codex-p3r2.md)) — 7 verified; regressions + new blockers applied same-day:

| P3R2 item | Resolution |
|---|---|
| B2 regressed — scores/events still one-row; timing gate soft | `save_vote_scores(batch)` + `OutboxWriter::append_batch`; settlement tx = constant statement count; **1,000-voter fixture < 2s as a hard passing gate** |
| B4/M2 regressed — report-commit vs settlement undefined; no Held return | Two-transaction sweep protocol (report commits in Tx A, settlement reads it in Tx B); `ResolveOutcome { Settled, HeldForReview, CuratorRequired, Voided }` feeds the sweep counters |
| M5 regressed — validation incomplete | Full validation set enumerated (min_fee bounds + seed-time base-fee check, ppm ranges, zero-semantics for pot/flip/leaderboard minima, secret non-empty, CIDR parsing) each with a startup-failure test |
| N1 — realizations cross-market FK missing | Composite `(outcome_id, market_id)` FK + rejection contract test |
| N2 — LP window had no time axis | `markets.settled_at` written in settlement + partial index; window = settled_at; voids participate (stated) |
| N3 — flip-window preview had no clock/read | `last_buy_at` on MarketQueries (preview, with injected Clock) AND TradeTx (execution); boundary pinned strict-less-than, tested on both surfaces |
