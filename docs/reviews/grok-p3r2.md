# Phase 3 plan — Round 2 VERIFY (grok)

Verified rewritten `docs/plans/phase3-economy-integrity.md` against [grok-p3r1.md](grok-p3r1.md) and [p3r1-resolution.md](p3r1-resolution.md). Codex R1 items checked only where they intersect mechanism honesty (realizations, settlement authority, kill-switch window).

## Own findings (R1) — VERIFIED / REGRESSED

| ID | Status | One line |
|---|---|---|
| **B1** anti-churn flip-window | **VERIFIED** (with residual) | `effective_fee` drops discount when `is_flip_within_window` (sell of outcome with buy inside `discount_flip_window_secs`) → **base fee on the sell leg**; scoring page must publish it. This **taxes** short flip churn (removes the discount subsidy on sell), it does **not** ban wash; residual: **buy leg still discounted** even inside the flip window — stated honesty should be “sell-side base fee on short flips,” not “wash closed.” Economically sufficient as a first control if copy is honest; not a full wash ban. |
| **B2** quality floor | **VERIFIED** | Rep updates only when `votes ≥ min_votes_to_resolve && escrow ≥ rep_score_min_pot_micro`; scores still written. Split is coherent: transparency of vote quality without free tier climb on thin pots; page checklist includes quality floor. |
| **B3** two-signal + due-time | **VERIFIED** | `verdict` needs **≥2** flagged checks; single flag = pass-with-note and settles; `integrity_due_at` schedules review (2–5 min honest). Single-flag pass-with-note does **not** help public attackers if `checks` jsonb stays off public REST (admin inbox only today); public surface is `under_review` bool + heuristic copy — acceptable. |
| **B4** curator inbox | **VERIFIED** | `GET /admin/markets/flagged` with reports is enough for Phase 3 ops (curl); notifications deferred without losing discoverability. |
| **M1** voter board | **VERIFIED** | `avg_score_bp` + `markets_scored ≥ leaderboard_min_scored` + tier ≥ 1 is the right anti-volume formula. |
| **M5** scoring copy | **VERIFIED** | `docs/copy/scoring.md` checklist (75/25, 25pp kernel, 50% tie, EWMA+floor+tiers, integrity bar, hold rule, anti-churn fee) is complete enough to fail a PR that ships vibes-only. |
| **M6** tape non-goal | **VERIFIED** | Explicit: no tier badges on public tape. |
| **M2**/codex **B5** realizations | **VERIFIED** | Immutable signed facts at sell/settlement/void; leaderboard = SUM window; fixtures cover flip, hold-to-payout, zero-payout loss, void, replay. Matches adversarial intuition: no double count of sold cost at later settlement. |
| **codex B4** two-path settle | **VERIFIED** | From `Resolving`: (pass report ∧ unflagged) or curator override; no report → 423 even admin; flagged+None → 409; negative tests named. |
| **codex M6**/own **M3** kill-switch | **VERIFIED** | Class-3 lock; window = sum `lp_pnl_micro` over markets **resolved** in window; trip `sum ≤ −max_loss`; seed-time-only **stated** in task body + Phase 6 named. |
| **M4** sybil honesty | **VERIFIED** | Trusted-proxy CIDR hop rule; HMAC device storage; Weak/Medium strengths; “heuristics not guilt” copy. |
| **M7** batch rep | **VERIFIED** | Canonical user locks before market; bulk ports; 1k-voter timed fixture with report-if-slow. |

No R1 items **REGRESSED**.

## NEW issues from the rewrite (cap 5)

[N1] **B1 residual buy-leg discount (honest labeling).** Short flip still gets **discounted buy fee + base sell fee**. Not a reopen of B1 if product copy says “sells of recently acquired shares pay the base fee,” but a worker who implements marketing as “anti-wash complete” would overclaim. **FIX (docs only):** one sentence in Task 3.1 + scoring.md: discount never applies to the sell leg of a flip; buy leg may still be discounted; this removes the sell-side subsidy, not all churn economics.

[N2] **`last_buy_at` is per-(user, outcome), not lot-level.** Any sell after a recent buy on that outcome pays base fee — **conservative** (can tax long-held inventory sold after a top-up). Acceptable anti-churn bias; pin in port comment so implementers don’t invent FIFO “sell old lots at discount.” No change required beyond a one-line comment in the plan/port.

[N3] **`integrity_reports` public exposure — implicit only.** Admin flagged list includes `checks` jsonb; no public route is specified. Single-flag pass-with-note still **persists** a report with full check values. **FIX:** one explicit non-goal in 3.2: “no public REST for integrity_reports or per-check breakdown; public only `under_review`.”

[N4] **Pass-with-note + settlement timing.** A single-flag market settles after due-time like a pass — good for false positives. Attackers who can only trip one weak signal (device) learn nothing public; if they collude to trip exactly one Medium signal, they still get paid. Intended. No fix.

[N5] **Curator list includes all `status='resolving'`** (not only flagged). That is correct for ops (see holds before due, stuck reports) but may clutter the inbox with clean holds waiting on `integrity_due_at`. Optional filter `due|flagged` later — not blocking.

## Adversarial spot-checks (requested)

- **Flip within window (realizations):** sell fact carries sell realized delta; later settlement only remaining shares → SUM is intuitive net. **OK.**
- **Partial sells:** same. **OK.**
- **Voids:** source `'void'` facts; losers not invisible. **OK.**
- **False-positive viral market:** needs two independent flags; single burst-only → pass-with-note and pay. **Much better than R1.** Residual risk if burst+young both fire on a real viral student market — curator path + published “a few minutes” still applies; acceptable Phase 3.

## Build readiness

N1–N3 are **doc/labeling** tightenings (minutes of plan text), not mechanism redesign. They do not reopen R1 blockers.

FINAL VERDICT: sound-to-build
