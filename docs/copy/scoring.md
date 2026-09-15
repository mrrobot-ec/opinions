# How scoring, reputation, and fairness work

This is the published product copy for Opinions. Numbers are launch defaults (config may change; the formulas do not).

## Vote scoring (per market)

After a market resolves to an actual YES share `a` (0–100% of YES votes):

```
accuracy = max(0, 1 − |crowd_guess − a| / 25)   // linear kernel; 0 if ≥25 points off
majority = 1 if your side matches the winning side, else 0
score    = 75·accuracy + 25·majority            // 0–100 per market
```

- **Accuracy (75%)** rewards reading the crowd, not herding alone.
- **Majority (25%)** rewards being on the winning side.
- **Tie rule:** at **exactly 50.00% YES**, *both* YES and NO count as winners for the majority component.
- **Voided markets** do not produce scores (no free majority reputation).

Scores are also stored as basis points (0–10,000) in the core: `score_bp = 100 × score` on the 0–100 scale above (equivalently, the integer path uses 75%/25% on 0–10,000 bp accuracy/majority components).

## Reputation (EWMA)

```
rep' = EWMA of per-market score_micro   // score_micro = score_bp × 100, range 0..1_000_000
half-life = 20 markets (K_PPM = 965_936)
```

- New accounts start at **rep 0 · tier 0**.
- **Quality floor:** a market updates reputation only when  
  `votes ≥ min_votes_to_resolve` **and** `escrow ≥ rep_score_min_pot` (launch default pot floor: **$50** = 50,000,000 micro-USDC; `0` would disable the floor).  
  Scores are still **recorded** when the market is non-void; the floor only blocks the EWMA update so thin pots cannot mint tier climb.
- **Tiers** (inclusive lower thresholds on `rep_micro`, max 1,000,000):

| Tier | Min rep (micro) | Position cap / market (open cost basis) | Fee discount | Effective fee (base 1.00%) |
|------|-----------------|------------------------------------------|--------------|----------------------------|
| 0    | 0               | $25                                      | 0 bp         | **1.00%**                  |
| 1    | 200,000         | $50                                      | 0 bp         | **1.00%**                  |
| 2    | 400,000         | $100                                     | 10 bp        | **0.90%**                  |
| 3    | 600,000         | $250                                     | 20 bp        | **0.80%**                  |
| 4    | 800,000         | $500                                     | 30 bp        | **0.70%**                  |

Base trade fee launch default: **100 bp (1.00%)**. Discounts never push fee below **min_fee = 10 bp (0.10%)**.

Voter rewards at launch are **points / reputation only** — no cash for voting.

## Anti-churn fee rule

Fee discounts reward real participation, not wash flips:

- If you **sell** shares of an outcome that you **increased within the flip window** (launch default **3,600 seconds / 1 hour**), that sell pays the **base fee** — the tier discount does **not** apply on that sell leg.
- The **buy** leg may still be discounted; this rule **taxes short flip sells**, it does not ban trading.
- After the window, sells of that inventory use the normal discounted fee again.

## Integrity bar (who may vote)

Launch-blocking eligibility (defense of the vote-oracle):

1. **Phone-linked channel required** — the voting account must have a linked `imessage` (or configured) identity channel. Web demo accounts without a link are rejected.
2. **Velocity cap** — at most **30** votes per rolling **3,600 s** window per user (across markets).
3. **Young-account friction near close** — accounts younger than **72 hours** cannot vote in the last **10 minutes** before `closes_at`.

Hidden tallies freeze **all** trading (both sides) in the close window; voting may continue until `closes_at`.

### Hidden window & comments (accepted speech)

The hidden window protects **structured** surfaces only: live tallies, vote receipt numbers, profile vote sides/scores, and system notification payloads. **Comments are free speech** — claimed vote sides in prose are unverified and unpoliced (anyone can type “I voted YES”; the platform does not treat that as oracle data). Coordinated “I voted X” spam is a known residual of that choice; it is bounded by per-author comment velocity, not by content filtering. Vote sides and scores on profiles appear only after the market resolves; cast **timestamps** on profiles remain public (D6).

## Review hold (large pots)

- Pots with escrow **at or above** the hold threshold (launch default **$500** = 500,000,000 micro-USDC) enter **`Resolving`** for an automated fairness review.
- Review is **scheduled** (typically **a few minutes**, config ~2–5 minutes) — not a same-tick instant spin.
- Heuristic checks (vote burst, young-account share, subnet / device concentration) are **automated heuristics, not a guilt finding**.
- **Flag** requires **two or more** independent check failures; a single soft signal still pays after review.
- Flagged markets **never** auto-settle — a curator chooses resolve-at-tally or void (neutral 50¢ redemption).
- Smaller pots settle on the normal fast path.

## Leaderboards (weekly)

- **Top voters:** average score (not sum), with a minimum number of scored markets and tier ≥ 1 — volume alone cannot buy the board.
- **Top traders:** **settled / realized PnL in the window** (not mark-to-market).
- Tier badges appear on profiles and leaderboards — **not** on the public trade tape.
