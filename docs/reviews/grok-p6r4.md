APPROVED

Adversarial re-review (round 4, grok) of `docs/plans/phase6-simswarm-ops.md` revision 4 against `docs/reviews/grok-p6r3.md` and `docs/reviews/p6r3-resolution.md`. Checked the r4 sentences against shipped `fee_policy.rs` (`effective_fee` = flip? base : `max(base − discount[tier], min_fee)`), `integrity.rs` (`verdict` needs ≥2 flags; device share strict `>` 600_000 ppm), `seed.rs` `market_windows` (one global `FLASH_*` pair), and `TxnKind` (no withdraw use case; `domain` stays untouched).

Every grok-p6r3 closer is plan-text. The accepted residual (NEW-3 / D22 dump) is marked as such and the optional Live-book hardening landed. Residuals the resolution map accepted as landed (caps, $50 OI, ≥50% 2xx, `sweep_delay=180`, ops.md in §3.7) are in the revision-4 sentences. No new defect that mints wrong money, drops an event, leaves a contract unimplementable, or lets the swarm e2e stamp green on theater.

Shipped numbers unchanged from r3; r4 now actually pins the two that made proofs vacuous (`fee_discount_bp_by_tier = [0,0,10,20,30]`, fat-pot `H ≤ 15` at every scale).

## Per r3 finding

| ID | r3 | r4 |
|---|---|---|
| N9 / NEW-2 discount vector | PARTIAL / MAJOR | **CLOSED** |
| NEW-1 roster isolation at 2k | BLOCKER | **CLOSED** |
| NEW-4 720s book untimed / unstampable | MAJOR | **CLOSED** |
| NEW-5 lien freezes collectible cash | MAJOR | **CLOSED** (accepted reshape: eligibility + deposit auto-collect; no HTTP withdraw) |
| NEW-3 auto-expiry dump | MINOR | **CLOSED** as accepted residual; hardening adopted |
| Residual: remedial cap magnitudes | named ask | **CLOSED** ($500 / $2,000) |
| Residual: pad OI $50 | named ask | **CLOSED** |
| Residual: convoy 2xx fraction | named ask | **CLOSED** (≥50%) |
| Residual: `sweep_delay` pin | named ask | **CLOSED** (180) |
| Residual: ops.md in exit | named ask | **CLOSED** (§3.7 barrier-gated) |
| r2 #1–#12 / N1–N8 | RESOLVED in r3 | **still closed** — r4 did not reopen them |

### N9 / NEW-2 — CLOSED

D24 catalog now carries `fee_discount_bp_by_tier = published [0,0,10,20,30]`, finance+2P, next-trade, each entry ≤ published, monotone. 0008 seeds the vector. §3.1 asserts the wash sell at **100 bp not 90 bp against that vector**.

Shipped `effective_fee`: flip-on → 100; flip-off + tier 2 + published table → `max(100−10, min_fee=10) = 90`. The 100-not-90 assert now has a real counterfactual. `RepConfig::default()` `[0; 5]` and `e2e_economy.sh` `0,10,10,10,10` are no longer the smoke authority.

Read "never < `min_fee_bps`" as the **effective-fee floor** already in `fee_policy.rs` / scoring.md ("discounts never push fee below min_fee"), not a per-element lower bound on the discount array. The published `[0,0,10,20,30]` is legal under that reading; a worker who requires `discount[i] ≥ min_fee` would reject the seeded vector — do not do that. Not a reopen.

### NEW-1 — CLOSED

D28 roster-isolation paragraph is the r3 rule, word for word in mechanism: fat-pot voters = aged ring `k ≥ 31` + **≤ 15** honest **at every scale**; 2k load and ≥1k-voter settlement hit **only** the convoy/lifecycle book; unique `x-device-id` and unique forwarded-for /24 on every non-ring agent; ring members share one of each. §3 names the fat pot roster-isolated and requires **Flag on the fat pot AND at-most-one-signal Pass on the lifecycle book**. Integrity ppm stays ±20% two-phase; unset `DEVICE_HASH_SECRET` / `TRUSTED_PROXY_CIDRS` fails the pinned-env check.

Smoke arithmetic is unchanged and still honest (`31/46 > 0.6` and burst). Nightly can no longer drown the pot with `H ≈ 1969` or false-Flag the 1k book via a shared NAT.

### NEW-4 — CLOSED

§3: five proof markets via **explicit per-market stamps** (admin/public create). Global `FLASH_CLOSES_SECS=180` / `FLASH_TALLY_HIDDEN_SECS=120` applies **only** to the convoy/lifecycle book. Near-close book stamped 720 s; young 422 at `closes_at − 60s`; control young vote **200 at t=0**. Matches `seed.rs` (one env pair) and `near_close_secs=600` (young *can* vote in `[0, 120)` on a 720 s book).

D28's leftover "`FLASH_CLOSES_SECS = 720`" is duration shorthand for that proof market, not a second global env. §3 wins.

### NEW-5 — CLOSED (accepted reshape)

Resolution dropped HTTP withdraw (codex NEW-6). D30: deposits auto-collect `min(cash_after_deposit, Σ open receivables)` in the same tx; `WithdrawalEligibility` role + fake/Pg contracts; read-only `GET /admin/users/{id}/withdrawal_eligibility`; no phantom withdraw route. §3.7 (b2): zero-cash debtor blocked; faucet deposit $200 on a $50 receivable → eligibility clear, house +$50, movements ledgered.

That is the accepted smaller surface. Do not reopen the r3 "then withdraw $150 → 200" sentence.

§3.7 (b) still says "withdrawn-seller shortfall". There is no withdraw path. Implement (b) as cash already spent (second book / fixture), **not** by inventing `POST /withdrawals`. Naming leftover, not a missing surface.

Collection legs fit existing `TxnKind` (extra legs on the Deposit txn, or an internal user→house apply). 6.0a `domain` untouched stays feasible.

### NEW-3 — CLOSED as accepted residual

Resolution: D22 dump at `tally_hidden_at` is accepted. D25 adopted the optional hardening: while `voting_paused` is in force during **Live**, PlaceTrade/Preview on that market also 423; auto-expiry at `tally_hidden_at` lifts the overlay. D22 itself still freezes the book once `now ≥ tally_hidden_at` (`place_trade.rs` already returns `TradingFrozen`). "Releases both" means the pause overlay, not a reopen of hidden-window trading.

### Residuals the map said landed — CLOSED

- D24 remedial caps: per-market ≤ $500 = 500_000_000 micro; daily house ≤ $2,000; 422 over.
- §3 pad market OI floor **$50**; suppress stays low.
- §3.1 convoy: **≥ 50%** of trade-capable agents 2xx before hidden.
- §3 `sweep_delay_secs=180` named; script still polls to `Paid` (no five-minute wall-clock claim).
- §3.7: `docs/copy/ops.md` exists with the per-key table and pause/unwind/remedial honesty paragraphs, barrier-gated.

### r2 items — no r4 reopen

Replay-hit returns the original receipt **without** pause/config checks (D25). That is idempotent replay of an already-filled trade, not a new fill during a pause — finding 5 / N8 stay closed. Global `trade_fee_bps` remains new-market-only; existing-book place after that change is still 200 at the stamped pool fee. Voting pause still auto-expires at hidden; CastVote still takes only `voting-market:{id}`. No `reprice-market`. Remedial credit still unwind-grade. Receivables still facts; identity 3 still cash-only.

## NEW findings

None that meet the bar (wrong money, lost events, unimplementable contract, green-wrong e2e).

Noted and **not** blocking (do not amend for these):

- D25a says StaleConfig **iff** a preview-relevant key changed; D24's watermark is a second, conservative 409 when history is gone. Extra re-preview, not a silent reprice. Keep enough hot history that §3.3's one-generation-later "place 200 after a global fee change" is still a hit.
- §4 still describes the round-3 protocol.
- Converse `PlaceTradeRequest` / pending payload must grow `expected_config_version` (JSON payload + generated models). Unlisted converse files trip the existing "stop the wave / coordinator remanifests" rule; the Rust public contract and §3.3 converse re-preview path are specified.

## Build read

Revision 4 is buildable. 6.0a / 6.0b may proceed; W1–W4 may dispatch on a matching second APPROVED (or an explicit accepted residual from the other reviewer).
