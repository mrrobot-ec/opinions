BLOCKED

Adversarial re-review (round 3, grok) of `docs/plans/phase6-simswarm-ops.md` revision 3 against `docs/reviews/grok-p6r2.md`, `docs/reviews/p6r2-resolution.md`, `docs/copy/scoring.md`, and the shipped Phase 3 sweep (`crates/domain/src/integrity.rs` `verdict` / `share_check` / `vote_burst`; `IntegritySweepConfig` / `VoteIntegrityConfig` / `RepConfig` defaults; `resolve_market.rs` hold gate; `main.rs` `FEE_DISCOUNT_BP_BY_TIER`; `seed.rs` `market_windows`).

Disposition map matches intent. The r2 money doors landed in mechanism: no `reprice-market`, remedial credit is unwind-grade, voting pause expires at hidden, receivables are facts with a withdraw lien, convoy is last-minute-of-Live, hidden-secs is an offset. That is real progress. Two proofs the e2e will still stamp green-or-red wrongly are **not** plan-closed: the wash 100-not-90 assertion has no published discount vector, and the sybil two-signal inequality is written for `H ≤ 15` while §3's namesake full profile is 2,000 agents. Do not dispatch W1–W4 until those two sentences exist. The other hunts (auto-expiry dump, withdraw-409 innocents) are real but not new extract buttons.

Shipped numbers used below (unchanged from r2; still not plan-invented):

| Knob | Shipped default | scoring.md launch |
|---|---|---|
| `burst_multiplier_ppm` | 2_000_000 (strict `>`) | two-or-more independent flags |
| `device_share_max_ppm` / `subnet_share_max_ppm` | 600_000 (strict `>`) | — |
| `min_votes_for_ratios` | 4 | — |
| `min_metadata_coverage_ppm` | 500_000 | — |
| zero-baseline burst | `last_window > 4` | — |
| `near_close_secs` / `min_account_age_secs` | 600 / 72h | last **10 minutes** / **72 hours** |
| `payout_hold_threshold_micro` | `i64::MAX` | **$500** = 500_000_000 |
| hold gate | `escrow >=` threshold → `Resolving` | — |
| `RepConfig::fee_discount_bp_by_tier` | `[0; 5]` | **`[0, 0, 10, 20, 30]`** |
| `FEE_DISCOUNT_BP_BY_TIER` in `e2e_economy.sh` | `0,10,10,10,10` (not published) | — |
| `FLASH_TALLY_HIDDEN_SECS` in `seed.rs` | **offset from now**, `hidden.min(closes)` | — |

## Per r2 finding

### 1. Independent personas cannot express coordinated attacks — RESOLVED

Four rings, shared HMAC / /24 / channel prefix, published scoring seed, pinned `x-device-id` / forwarded-for, split near-close vs sybil markets, wash member rep-seeded to tier 2. That is the r1/r2 shape.

Smoke arithmetic of the written inequality checks out against shipped code (see N1). Nightly 2k is a **new** hole (NEW-1), not a reopen of the 100-agent theater.

### 2. D25 pauses CastVote / breaks D22 — RESOLVED

Asked items all landed: `voting_paused` auto-expires at `tally_hidden_at` via a CastVote fence-point **read** (not a late job); two-phase distinct principals; CastVote takes only `voting-market:{id}`; e2e set-in-Live → 423, at hidden → 200 with no admin action; write of a new pause inside hidden refused unless voiding.

Carry-into-hidden (freeze last public tally and ride D22) is closed. Fence coupling (votes waiting on a trading exclusive) is closed.

Auto-expiry does open a synchronized vote-on window at hidden — that is D22 restored, not a new mint. See NEW-3.

### 3. Unwind is a captured-superadmin refund button — RESOLVED

Still closed. No `Paid` path. Dual control, one tx, house-leg shortfall, facts not a ledger class. N2/N5 were the amendment doors; those have their own rows.

### 4. D24 tunables: no bounds / live-immutability / role matrix — RESOLVED

Catalog now enumerates the r2-sensitive set and puts **caps, flip window, integrity ppm, velocity, hold, min_votes, voting pause** on two-phase. `confirmer_token_id ≠ proposer_token_id` is schema-enforced. `reprice-market` is deleted (N3). Emergency revert is a new dual-controlled proposal from `config_changes.old`, not a trading pause.

Residual (not a reopen): `fee_discount_bp_by_tier` is still absent from a catalog that claims to be complete — see N9 / NEW-2.

### 5. Surprise pause = house-informed dumping — RESOLVED

Unchanged and still holds: public frames, no resume in `(tally_hidden_at, closes_at]`, pause ≠ D22, pause-as-config-hatch forbidden in ops.md.

### 6. 100/5 min theater; nothing closes; replay leaks wall time — RESOLVED

Pinned `FLASH_CLOSES_SECS=180` + `FLASH_TALLY_HIDDEN_SECS=120` with offset semantics stated (N7). Trade convoy from `tally_hidden_at − 60s`, 2xx-before-hidden fraction, p95 on 2xx vs in-window 423s split, nightly vote convoy + 1k-voter settlement as the other worst case (N6). Hold named $500. Intent-schedule determinism kept.

Residual: still no wall-clock bound on the smoke; `sweep_delay_secs` still unpinned (default 180). Poll-until-Paid is enough if the script does not claim a 5-minute gate.

### 7. Chaos missing clock / PG / disk / storm / consumer-death — RESOLVED

Unchanged named list + crash-point port. Out of this review's economic beat; no regression.

### 8. All-Playwright deferral / spec-anchor citation — RESOLVED

Unchanged. Header still anchors spec §§9–10 + §13 item 7 ops half.

### 9. Young-account posture disables D21 or voids every market — RESOLVED

80/20 + never-zero `VOTE_MIN_ACCOUNT_AGE_SECS` + dedicated `FLASH_CLOSES_SECS=720` near-close market + aged-only sybil ring. Young burst on a 180s book is no longer asked to produce a flag.

Residual: the 422 assertion is untimed and the 720s book cannot be created by the same `FLASH_*` env as the 180s book (`seed.rs` reads one global pair). See NEW-4.

### 10. Preview/place config race — RESOLVED

409 only for this-market / this-user preview-relevant drift via `config_changes` (cap, or flip window when the pending would flip). Global next-market fee never 409s an existing book (e2e 200 at old pool fee). Converse: expire pending, new preview, new lexical yes; never auto-execute; never loop the same version. Flip-window churn does not 409 a non-flip buy. Caps and flip are now two-phase, so the r2 “bump generation every few seconds” fatigue path needs two principals.

### 11. Faucet money door; ban/shadow-limit silently missing — RESOLVED

Two-factor mount (`OPINIONS_ENV=staging` **and** `STAGING_FAUCET=1`), finance/superadmin, per-call cap, audit, startup non-mountability. Same arm gates `created_at` / rep-seed. Ban / shadow-limit / deposit-withdraw switches stay Phase 7.

### 12. ops.md must publish pause ≠ D22 and unwind ≠ void — RESOLVED

D25 / D30 / §0 still require the honesty paragraphs (pause ≠ D22, pause ≠ config hatch, void is terminal, unwind-from-Voided is wrong-fraud-decision, remedial credit is not a refund). Rev 3 dropped the numbered 6.5 barrier from the body; §3 does not assert the file. Content is specified; add the file to §3.7 or workers will skip it. Not a reopen.

### N1. [BLOCKER] four rings cannot trip two-signal Flag at smoke scale — RESOLVED (smoke) / see NEW-1 (nightly)

Written inequality vs shipped `integrity.rs`:

- Flag iff `≥ 2` checks `flagged` (`verdict`).
- Device: `top_device_count / device_observed > 600_000` ppm **and** coverage `device_observed / total ≥ 500_000`. `k=31`, `H=15`, everyone hashed: `31/46 = 673_913 > 600_000`, coverage 100 %. Strict `>` still holds (`30/45` also holds; `3/5` does not).
- Burst, zero prior: `last_window > min_votes_for_ratios` (4). A 31-vote close burst fires.
- Young-share is 0 on an aged ring (correct — young cannot fill `[closes_at−60, closes_at)` on a book whose whole life is near-close).
- Hold: `escrow >= 500_000_000` enters `Resolving`; pot ≥ $500 is pinned. Sweep actually runs.
- `DEVICE_HASH_SECRET` non-empty + `TRUSTED_PROXY_CIDRS` covering egress are pinned, so device/subnet are not coverage-skipped.

That is a real two-signal proof (device + burst) at the **stated** roster. It is **not** a proof if the fat pot also absorbs the 2k load roster (NEW-1).

### N2. [BLOCKER] remedial credit is the refund button — RESOLVED

Unwind-grade: dual control, distinct token ids, ≥T, reason, two audits, per-market **and** daily house caps (422 over), refuse self/linked principals, honesty paragraph, e2e finance-alone 403 / over-cap 422 / `Paid` unwind 409. Staging-or-prod-with-honesty was the r2 fork; they picked prod+honesty. Acceptable.

Residual: cap **magnitudes** are unnamed in 0008. A $10^15 cap makes “capped” theater. Pin numbers (e.g. per-market $500, daily $2_000) in the catalog seed. Not enough to reopen N2.

### N3. [BLOCKER] `reprice-market` restores live fee extraction — RESOLVED

Route deleted. Live `pool.fee_bps` immutable. `trade_fee_bps` is new-market-only. Live repricing is Phase 7 + real auth. Existing-book place after a global fee change is 200 at the old stamp. The r1 mid-window rebate is gone.

### N4. [MAJOR] two-phase deadlock / one token both roles — RESOLVED

15 min expiry, either principal rejects (audited), schema `confirmer_token_id ≠ proposer_token_id`, same-token 403 e2e, `config_changes.old` dual-controlled one-step revert, pause-as-config-hatch forbidden. Flip window and caps moved **into** the sensitive 2P set, which also closes the r2 leftover “caps-up + faucet is a single finance click.”

### N5. [MAJOR] receivable farm — RESOLVED (lien residual → NEW-5)

Open receivable ⇒ `TxnKind::Withdrawal` 409 (b2 e2e). House-receivable outstanding capped (new unwind confirms 422 + page). ops.md: unwind-from-Voided is “void was the wrong fraud decision,” never t0 cleanup. The farm that withdraws, voids, unwinds, repeats is no longer a self-serve loop.

Whether the 409 punishes innocents is NEW-5, not a reopen.

### N6. [MAJOR] convoy-at-hidden−15 s wrong duration — RESOLVED

Trade convoy starts at `tally_hidden_at − 60s`. On the pinned 180/120 book that is t=60, 60 s of Live. Smoke (tens of serialized PlaceTrades at the 150–300 ms budget) fits. 2xx-before-hidden fraction + p95-on-2xx / in-window-423 split is the metric I asked for. Nightly vote convoy at close + 1k-voter settlement is named as the other worst case.

Residual: the 2xx fraction is unnamed (1 % goes green). Pin it (e.g. ≥ 50 % of trade-capable agents on the convoy market). Nightly 2k still cannot all 2xx in 60 s of a serialized `market_for_update` — that is why the vote convoy is separate. Do not put the 2k roster on the 180 s trade-convoy book and call p95 honest.

### N7. [MAJOR] `FLASH_TALLY_HIDDEN_SECS=60` off-by-one-window — RESOLVED

§3 states the offset semantics and pins `180` / `120` so the hidden **window** is 60 s. Convoy `hidden − 60s`, burst `[closes_at − 60, closes_at)`, and the hidden window now describe one clock on that book.

### N8. [MAJOR] 409 on global next-trade drift breaks lexical confirm — RESOLVED

Market/user-relevant staleness only; converse expire + new preview + new yes; no auto-execute; no same-version loop; flip-window churn does not 409 a non-flip buy; global fee change does not 409 an existing book. With `reprice-market` gone there is no live-fee 409 path at all.

### N9. [MINOR] wash-ring assertion vacuous at tier 0 — PARTIAL

Rep-seed `≥ 400_000` micro (tier 2) + assert ledger fee **100 bp not 90 bp** is the right test **if and only if** a 10 bp tier-2 discount exists to remove.

Rev 3 never pins `FEE_DISCOUNT_BP_BY_TIER`. D24's “complete” catalog lists `trade_fee_bps`, `min_fee_bps`, `discount_flip_window_secs`, `position_cap_micro_by_tier`, `rep tier thresholds` — not the discount array. `RepConfig::default()` is still `[0; 5]`. §3's “published scoring seeds incl. 100bp/10bp/3600s/caps/quality-floor” names base / min / flip / caps / floor; the `10bp` is `min_fee`, not tier 2's discount.

Counterfactual:

- discounts `[0; 5]` → every sell is 100 bp → `assert fee == 100 && fee != 90` is true of *every* agent, seeded or not. Anti-churn is not hit. N9 theater, green.
- `e2e_economy.sh` default `0,10,10,10,10` happens to put 10 bp on tier 2 (so the assertion can fail if flip is broken) but is **not** the published table `[0, 0, 10, 20, 30]`.
- published table + flip on + last increase inside 3600 s (automatic on a 180 s book) → 100 bp; same sell with flip off → 90 bp. That is the proof.

**Fix.** In D24 catalog + 0008 + §3 env: `FEE_DISCOUNT_BP_BY_TIER=0,0,10,20,30` (finance+2P, each `≤` published and monotone, never below `min_fee_bps`). Keep the 100-not-90 assert. Do not inherit `e2e_economy.sh`'s vector.

### Sanity table (hold unnamed, OI floor, RepConfig seeds) — RESOLVED with residuals

Hold `$500` named. OI floors split (suppress low / pad positive). Published scoring seeds *declared* mandatory. `DEVICE_HASH_SECRET` / `TRUSTED_PROXY_CIDRS` pinned.

Residuals: pad OI is “positive” not a number (any `> 0` splits D21; pin e.g. `$50`); remedial-credit caps unnamed (N2 residual); `FEE_DISCOUNT_BP_BY_TIER` omitted (N9).

---

## NEW findings (amendment regressions + the three hunts)

### NEW-1. [BLOCKER] D28 / §3 — pinned inequalities do **not** hold at nightly 2k scale unless the fat-pot roster is isolated, and §3 does not isolate it

**Scenario.** D28's proof is `device_share = k/(k+H) > 0.6` with `H ≤ 15`, `k ≥ 31`. That is a **market-specific** roster, not a swarm size. §3 then says Flag fires on the fat pot **and** “Full profile (2,000 agents; ≥1k-voter settlement; vote convoy at close).”

If the 2k load roster votes on the fat pot:

- `H ≈ 1969`, `k = 31` → device = `31/2000 = 1.55 %` ≪ 60 %. Subnet the same if the ring is the only shared /24.
- Burst still fires (1k–2k votes in `[closes_at−60, closes_at)` vs near-zero prior, or last_window ≫ 4).
- One signal = `Pass`. Flag never fires at published ppm.
- Integrity ppm is superadmin+2P and only ±20 % of shipped defaults — workers **cannot** drop device to 2 % to go green. They will either shrink H (correct, but unstated in §3) or “fix” coverage by unsetting `DEVICE_HASH_SECRET` / `TRUSTED_PROXY_CIDRS` (then device *and* subnet skip; only burst; Pass; theater).

Smoke (if ~100 agents all vote the fat pot) already loses: `31/100 = 31 %` < 60 %. D28's `H ≤ 15` is doing all the work; §3 never says the other 85 / 1,969 agents stay off that book.

Same-host nightly: 1k honest voters, one CI NAT, XFF not unique → `subnet_concentration = 100 %` + close burst = **false Flag** on the lifecycle pot. The tempting fix is to drop `TRUSTED_PROXY_CIDRS`, which then also blinds the sybil pot.

**Fix.** One roster rule, in D28 **and** §3:

- Fat pot voter set = aged ring `k ≥ 31` + **≤ 15** honest, even in the 2k profile. The 2k load and the 1k-voter settlement hit the convoy/lifecycle market only.
- Non-ring agents carry **unique** `x-device-id` and unique XFF `/24`s. Ring members share one of each.
- E2e asserts Flag on the fat pot **and** Pass (or at most one signal) on the 1k-voter book.

Until that is plan-text, the phase named “simswarm to 2,000 agents” cannot claim a Flag proof at the scale it advertises.

### NEW-2. [MAJOR] D24 / §3 — “complete” catalog + published-seed list omit `fee_discount_bp_by_tier`

Covered under N9. Called out as new because revision 3 *added* the 100-not-90 assertion and the “catalog (complete)” sentence in the same amend. Completeness that skips the one vector the assertion needs is a regression of the claim, not just an inherited env gap.

**Fix.** Same as N9. Add the key to the D24 table (bounds, finance+2P, apply-to next-trade or new-market-only — pick one and 409 only if this user's effective fee would change).

### NEW-3. [MINOR] D25 — auto-expiry does create a last-second vote-on window; it is D22 restored, not a new extract

**Hunt answer.** At `tally_hidden_at` CastVote flips 423 → 200 with no admin action. A ring waiting on that known timestamp dumps into the first second of hidden. Public tape is already frozen (D22). Trading is already frozen (D22). That dump is what every unpaused market already allows for the whole hidden window. Carry (r2) had been *denying* D22; auto-expiry gives it back. The e2e that requires 200 at hidden is correct.

The Live interval `(voting_paused_at, tally_hidden_at)` still has a frozen oracle and an open book. That mixed halt **pre-existed** r2's split keys (trading pause ≠ voting pause); auto-expiry does not lengthen it (it shortens the vote freeze by cutting it off at hidden). Public `MarketVotingPaused` is the finding-5 mitigation. Optional hardening, not a blocker: while a Live voting pause is in force, PlaceTrade/Preview also 423, so the book cannot take risk against a frozen tape. Do **not** keep the pause through hidden to “fix” the dump — that is the carry attack.

### NEW-4. [MAJOR] §3 / `seed.rs` — 720 s near-close proof is untimed and not stampable next to the 180/120 book

**Scenario.** On a 720 s market with `near_close=600`, young accounts **can** vote in `[0, 120)`. Hidden-offset 120 s (if the global env is reused) makes that window exactly Live. A smoke that fires every vote at t=0 gets **young 200**, fails the “young 422” e2e, and will then shrink the book back to 180 s (always near-close) or zero the age rule.

`seed.rs` `market_windows` reads one `FLASH_CLOSES_SECS` / `FLASH_TALLY_HIDDEN_SECS` pair for every flash it creates. One process cannot emit both the 180/120 convoy book and the 720 s near-close book from those env vars.

**Fix.** Create the five markets via explicit stamps (admin/public create, not the single-env seeder). Pin: young 422 is attempted at `closes_at − 60s` (inside the 600 s window); a control young vote at `t=0` on the 720 s book is **200**. State `FLASH_*` env applies only to the convoy book.

### NEW-5. [MAJOR] D30 — withdraw-409 without auto-collection freezes cash that could satisfy the lien

**Hunt answer.** Unwind already drains user cash first; a leftover open receivable means cash is 0 at that moment, so 409 on a $0 withdraw is vacuous. The innocent-user hit is the **next deposit**:

- User (or a wrongly-voided winner) later deposits $200 against a $50 receivable.
- Plan: any open receivable ⇒ every `Withdrawal` 409. They cannot withdraw $150. They also cannot self-serve the $50 (collection is a named path, not a withdraw/deposit hook, role unspecified).
- Finance-asleep (the N4 3am case) = user frozen on all future cash-out, including unrelated winnings, until ops collects.
- Dust receivable (1 micro) freezes $10k.

That is a collections lien, which is the right *shape* against the N5 farm. It is the wrong *implementation* if collection is not applied before the 409.

Users who never received a receivable are unaffected. Users who still had enough cash at unwind never get one. The guard does **not** punish bystanders; it punishes debtors (and mis-booked debtors) on all future withdrawals.

**Fix.** On deposit and on withdraw: auto-collect `min(cash, Σ open receivables)` (user cash → house, fact → collected) in the same tx, then 409 only if a receivable remains **and** a withdraw was requested. E2e: deposit $200 on a $50 receivable, withdraw $150 → 200, receivable collected, house +$50. Keep b2 (withdraw with receivable and **zero** cash still 409). Self-serve, no extra principal.

---

## Hunt answers (one line each)

| Hunt | Answer |
|---|---|
| Auto-expiry last-second vote dump? | Yes, at `tally_hidden_at`. It is D22 hidden voting restored. Not a new mint. Optional: 423 trades during *Live* voting pause. |
| Inequalities at nightly 2k? | **No**, not if 2k vote the fat pot. `31/2000 < 60 %`. Need roster isolation in §3 (NEW-1). |
| Withdraw-409 punish innocents? | Bystanders no; post-deposit debtors yes, until finance collects. Auto-collect then 409 (NEW-5). |

## What is sound (still not a pass)

- No live repricing. Two-phase identity is schema, not prose.
- Voting pause cannot ride D22; votes do not take trading fences.
- Remedial credit is no longer a single-finance mint.
- Receivables are facts; identity 3 stays cash-only; void is terminal.
- Smoke sybil inequality is arithmetically honest against `integrity.rs` at `H ≤ 15`.
- Hidden-secs offset and last-minute trade convoy are the right clocks.
- 409 converse cannot auto-confirm a new quote.

## Build read

r2 BLOCKERs N1 (smoke) / N2 / N3 are closed in mechanism. N9 is still theater without a discount vector. NEW-1 makes the Flag proof false at the 2k scale the phase is named for. Land **N9 + NEW-1** as plan-text (discount pin + fat-pot roster isolation, including unique honest device/XFF). Land **NEW-4 / NEW-5** in the same amend so the 720 s 422 and the lien cannot go green-wrong. NEW-3 may be accepted as a residual. Then 6.0a/6.0b may proceed.
