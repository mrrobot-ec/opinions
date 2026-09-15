BLOCKED

Adversarial re-review (round 2, grok) of `docs/plans/phase6-simswarm-ops.md` revision 2 against `docs/reviews/grok-p6r1.md`, `docs/reviews/p6r1-resolution.md`, `docs/copy/scoring.md`, spec §§9–10, and the shipped Phase 3 sweep (`crates/domain/src/integrity.rs`, `IntegritySweepConfig` / `VoteIntegrityConfig` / `RepConfig` / `ContentTierConfig` defaults). Disposition map is honest about intent. Several r1 holes are closed in mechanism, not just wording. The amendments also introduce new money doors and an integrity e2e that cannot fire at published thresholds. Do not dispatch W1–W4 until the BLOCKERs below are plan-text.

Shipped numbers used below (not plan-invented):

| Knob | Shipped default | scoring.md launch |
|---|---|---|
| `burst_window_secs` | 60 | (not published; “near close”) |
| `prior_horizon_windows` | 4 | — |
| `burst_multiplier_ppm` | 2_000_000 (2× prior avg) | two-or-more independent flags |
| `young_account_share_max_ppm` | 500_000 | — |
| `device_share_max_ppm` / `subnet_share_max_ppm` | 600_000 | — |
| `min_votes_for_ratios` | 4 | — |
| `min_metadata_coverage_ppm` | 500_000 | — |
| `payout_hold_threshold_micro` | `i64::MAX` (hold off) | **$500** = 500_000_000 |
| `sweep_delay_secs` | 180 | “a few minutes / ~2–5” |
| `near_close_secs` / `min_account_age_secs` | 600 / 72h | last **10 minutes** / **72 hours** |
| base fee / min fee / flip | `RepConfig::default()` is 0 / 0 / 0 | **100 bp / 10 bp / 3600 s** |
| caps by tier | `i64::MAX` | **$25–$500** |
| quality floor | 0 (disabled) | **$50** |
| flash open / hidden (Phase 5 tier) | 3600 / 300 | — |
| flash `min_votes_floor` | 3 | “positive before go-live” |
| `OI_FLOOR_MICRO` | 0 | unpublished |
| `FLASH_TALLY_HIDDEN_SECS` in `seed.rs` | **seconds from now until hidden starts**, not window length | — |

## Per r1 finding

### 1. Independent personas cannot express coordinated attacks — PARTIAL

Ring abstraction + four named rings + published scoring seed + pinned `x-device-id` / forwarded-for are the shape I asked for. Wash / pad / suppress / sybil are now first-class.

It is not economically closed:

- **Wash pair cannot fail at smoke scale.** scoring.md discounts start at tier 2 (10 bp). EWMA half-life is 20 markets; a 180 s flash does not mint tier 2. Every agent stays tier 0 / 100 bp. “Sell leg paid base fee, no discount harvest” is true of *every* sell. The assertion is theater unless a staging-aged agent is also **rep-seeded to ≥ 400_000 micro** and the sell is asserted at 100 bp not 90 bp.
- **Sybil ring as specified cannot produce the two-signal flag** it asserts (see N1). The ring is “young accounts + one device + burst in `[closes_at − burst, closes_at)`”. Young accounts cannot vote in the last 600 s; the whole 180 s market is inside that window. Even if they could vote, 20 % young on a fat pot that the other 80 % also vote cannot clear 50 % young-share or 60 % device-share.
- Comment last-look / report-brigade and EWMA farming remain unnamed residuals (acceptable if N1’s flag proof is real). Self-referral correctly stays Phase 7.

### 2. D25 pauses CastVote / breaks D22 — PARTIAL

Trading pause now binds only Preview/Place. Corridor quote of the D22 contract, e2e “votes 200 / trades 423”, `voting_paused` as superadmin+reason+public frame, write forbidden once `now ≥ tally_hidden_at` unless voiding — all landed.

The write restriction is not a fence on the **read**:

- `voting_paused=true` set during **Live** remains true when the book enters Closing. CastVote will still refuse through the hidden window. The operator froze the last *public* tally and then rode D22’s trading freeze with a stuck oracle. E2E §3.4 only refuses a *write* inside the hidden window; it will go green while this carry works.
- Dual-control on `voting_paused` (r1 fix) was dropped. After the carry hole, a single superadmin bearer is again an oracle kill switch, just pressed 60 s earlier.
- CastVote still takes the class-4 **trading**-pause fences. A `trading_paused` exclusive write stalls votes for the drain. Not a kill switch, but it couples the oracle to a trading-halt lock; pin that votes must proceed after the exclusive is released and must not read `trading_paused` as binding.

### 3. Unwind is a captured-superadmin refund button / reopens R3 — RESOLVED (with new doors: N2, N5)

The asked D30 rewrite landed: no unwind from `Paid`; legality is `Voided` or flagged-`Resolving` pre-payout; dual-control (superadmin proposes, finance confirms after ≥ T, two audit rows, distinct principals in e2e); receivables instead of negative rows / skipped legs; fee reverse-to-users documented; no EWMA rewrite; realizations compensating facts; e2e (a)–(e).

That closes the r1 R3 clawback. It does **not** close house-funded make-whole (N2) or receivable farming (N5). Those are amendment regressions, not a reopen of the original “restore t0 from Paid” text.

### 4. D24 tunables: no bounds / live-immutability / role matrix — PARTIAL

Per-key catalog (bounds, apply-to, role, max-delta), 0008 seed, live-immutable stamp on the market row, two-phase on fee/seed/integrity/min_votes, e2e 422 + live `tally_hidden_at` / pool fee unchanged — that is the catalog I asked for.

Left open, and one new back door:

- **`reprice-market`** (D25a) is an audited, bounded, role-unspecified hot-swap of a **live** pool’s `fee_bps`. That is the original mid-window rebate, moved to a second route. Two-phase on `SetConfig` is pointless if a single principal can reprice the live book (N3).
- Two-phase does not say propose/confirm must be **distinct token ids**. One `ADMIN_TOKENS_JSON` entry can carry `finance` ∪ `superadmin` (roles are explicit sets, not exclusive). Then dual-control is a second click by the same bearer (N4).
- No reject / expiry / emergency revert of a confirmed bad fee (N4). Blast radius is bounded by 20 bp / 5 min and [10, 200] bp — fail-closed, not a silent extract, but 3am ops is stuck or must misuse `trading_paused`.
- Integrity ppm / velocity / hold / flip-window / caps: flip-window and tier caps are `next-trade` and finance-writable, **not** in the two-phase sensitive set. Caps-up + faucet is still the D23 whale button, now with a catalog.

### 5. Surprise pause = house-informed dumping — RESOLVED

Public `TradingPaused` / `TradingResumed` in the same tx; unpause forbidden in `(tally_hidden_at, closes_at]`; reason + ops role; ops.md “pause ≠ D22”. Preview stays lock-free + version-stamped; PlaceTrade is the fence. This is the r1 fix.

Residual (not a reopen): using global pause as the 3am fee hatch (N4) reintroduces the trap-positions shape. Do not document pause as the config-incident runbook.

### 6. 100/5 min theater; nothing closes; replay leaks wall time — PARTIAL

Pinned `FLASH_CLOSES_SECS=180`, hidden 60 s, published scoring seed, product floors, hold low enough to enter `Resolving`, full Live→…→Paid plus one Voided, convoy, SLO split + `SLO_ENFORCE=0` persisted, nightly 2k with ≥1k-voter settlement, intent-schedule determinism + `--dry-run` / `--replay-log`, invariants off-spike — that is the r1 smoke rewrite.

Still not a proof:

- **`FLASH_TALLY_HIDDEN_SECS` in `crates/main/src/seed.rs` is an offset from now, not a window length.** `hidden=60` + `closes=180` ⇒ hidden **starts at t=60**, live trading is 60 s, closing is 120 s. Phase 5 / D24 “hidden window 60 s” means freeze at t=120. Workers copying `e2e_economy.sh` will ship the wrong book. Pin both env vars and the meaning (N7).
- **Convoy at `tally_hidden − 15 s` is the right side of D22 (last Live, not `closes_at`) and the wrong duration.** PlaceTrade serializes on `market_for_update`. D22 freeze does **not** drain the class-4-style queue — waiters that acquire after hidden get 423. 100 × ~150–300 ms ≈ 15–30 s; 2_000 cannot fit in 15 s. Spec §9’s worst case is last-**minute** pile-in. p95 of the trades that won the race will look fine; the SLO will not see the lock wait it claims to measure (N6).
- Hold + `sweep_delay_secs=180` is not pinned. A 180 s market + 180 s review already exceeds a 5-minute mental smoke. Fine if the script polls, but “published scoring defaults” vs “hold low enough” is an unnamed override (sanity section).
- `--duration 300s` was deleted (good) but no new wall-clock bound is named.

### 7. Chaos missing clock / PG / disk / storm / consumer-death — RESOLVED

Named list (1)–(9) matches the r1 minimum: clock jump, `pg_terminate_backend`, disk-full `render_dir`, 100×10×1 s webhook storm, reconciler kill+heal, simultaneous daily+flash, plus the original four. Two-factor arm + typed parse + `/healthz` fault set + Noop transparency + per-connection WS drops are the codex containment I was happy to inherit. Indexer/onramp correctly stay Phase 7.

### 8. All-Playwright deferral / spec-anchor citation — RESOLVED

Thin mobile Playwright in W4 + §3.8 (vote / trade / resolve / share-card `<img>`, no KYC/card/withdraw). Rails stay Phase 7. 5 % converse slice is not advertised as the 10 % rails slice. Header re-anchored to spec §§9–10 + §13 item 7 ops half + PLAN.md phase 6. Citation bug closed.

### 9. Young-account posture disables D21 or voids every market — PARTIAL

80 % aged (`created_at = now − 80 d`, staging-armed, audited) / 20 % young + ring metadata; integrity profiles never zero `VOTE_MIN_ACCOUNT_AGE_SECS`; e2e asserts near-close 422 and accepted aged votes; synthetic phones ≠ verified OTP — the policy I asked for.

It collides with finding 6’s 180 s book and finding 1’s sybil burst:

- `near_close_secs=600` > market life 180 s ⇒ **every** second is near-close ⇒ young agents **never** cast a vote, they only 422. That proves the 422, not a young-share signal.
- `vote_burst` is anchored at `closes_at` over the last 60 s, which is inside the 600 s young ban. A **young** burst ring is definitionally empty.
- Staging `created_at` override shares the `STAGING_FAUCET` arm. Fine as a single staging-only switch; say so in ops.md so a prod mis-set cannot backdate real users.

### 10. Preview/place config race — PARTIAL

`TradePreview.config_version` + PlaceTrade 409 `StaleConfig` when generation moved **and** a `next-trade` key changed + pause via fence-held authoritative read is the r1 shape.

Broken by D25a’s other sentence: pool `fee_bps` is stamped at creation; next-trade fee applies to **new** markets; live fee only via `reprice-market`. Then §3.3 still demands “a NEW preview quotes [the fee change] and a pre-change preview’s PlaceTrade gets 409.” On an existing book those cannot both be true unless 409 fires on an **irrelevant** generation bump (N8). Converse lexical-confirm handling of 409 is unspecified — “re-preview and re-confirm” can mean auto-yes at the new quote (silent reprice) or a 409 loop on the same pending until the 2-minute TTL (fatigue / stuck “yes”).

### 11. Faucet money door; ban/shadow-limit silently missing — RESOLVED

Faucet = finance/superadmin ∧ `STAGING_FAUCET=1` ∧ per-call cap ∧ audit ∧ startup non-mountability. Ban / shadow-limit / deposit-withdraw switches explicitly Out to Phase 7 rails. Smoke must not use the production admin token is implied by the capability matrix + arm; keep that sentence in §3.7.

### 12. ops.md must publish pause ≠ D22 and unwind ≠ void — RESOLVED

Barrier 6.5 requires `docs/copy/ops.md` per-key table (bounds / class / role / user-visible line) plus pause and unwind honesty paragraphs. That is the r1 docs gate. Honesty text must absorb N2/N5 (unwind is not “finish the void”; remedial credit is not a user refund) or the paragraph will lie again.

---

## NEW findings

### N1. [BLOCKER] §3.1 / D28 — four rings + 80/20 age policy cannot trip Phase 3’s two-signal flag at smoke scale

**Scenario.** §3.1 asserts “sybil flag report exists on the fat pot.” Verdict is `Flag` iff **≥ 2** checks fire (`domain::integrity::verdict`). Shipped bars: `vote_burst` > 2× prior-window average (zero-baseline: `last_window > 4`), `young_account_share` > 50 %, `device_concentration` / `subnet_concentration` > 60 % **and** metadata coverage ≥ 50 %. Hold only runs if escrow ≥ hold threshold (code default `i64::MAX` = never).

Arithmetic on the pinned smoke:

1. 100 agents, 20 young, 80 aged. Fat pot that also has honest aged votes: young-share ≤ 20 % , one-device share of the 20-young ring ≤ 20 %. Both under their bars.
2. Young cannot vote at all on a 180 s market (`near_close=600`). Young-share = 0. Burst window `[closes_at − 60, closes_at)` is inside that ban, so a young burst is empty.
3. Aged honest votes spread over 180 s make `vote_burst` borderline or a **single** signal. One signal = `Pass` (scoring.md: “a single soft signal still pays”).
4. Plan never pins `DEVICE_HASH_SECRET` or `TRUSTED_PROXY_CIDRS`. Without the secret, `x-device-id` is dropped and `device_observed=0` → coverage skip. Without a trusted proxy, XFF is ignored (Phase 3 honesty) → subnet skip. Then the only possible signals are burst and young-share; young-share is 0. **At most one signal. Flag never fires.**
5. Workers will then “make e2e green” by zeroing age, dropping ppm, or shrinking the honest voter set — exactly the theater r1 rejected.

**Fix.** Split the proofs onto two markets and pin the env:

- **Near-close market:** 80/20, published 600 s rule (so this market’s `FLASH_CLOSES_SECS` must be **> 600**, e.g. 720+), assert aged 200 / young 422. Do not ask this market to flag.
- **Sybil fat pot:** a **small** honest baseline (≤ 15 votes) plus an **aged** ring of `k ≥ 31` sharing one device HMAC and one `/24`, bursting in `[closes_at − 60, closes_at)` with near-zero prior (or prior avg such that `k * 4 / prior > 2`). Then device ≥ 60 % **and** burst fires. Young are the wrong members for a close-anchored burst.
- Pin `DEVICE_HASH_SECRET` (non-empty), `TRUSTED_PROXY_CIDRS` covering the swarm egress, and `PAYOUT_HOLD_THRESHOLD_MICRO` as a **named smoke override** so the pot actually enters `Resolving` (sweep does not run below hold).
- Write the inequality into D28 so a worker cannot “fix” a red e2e by turning the rule off.

### N2. [BLOCKER] D30 — remedial credit is the refund button D30 just killed

**Scenario.** From `Paid`, unwind is refused (correct, R3). The replacement is a **house-funded remedial credit**: finance-only, per-market cap (unnamed), audited, credits users from `house`, no clawback, “does not restore t0.” No staging arm (unlike the faucet). No dual-control. No T delay. No ban on crediting the caller’s own / linked users. Phase 6 still authenticates admin with a bearer map.

A stolen `finance` token is now a mint on every `Paid` market, up to the cap, in production shape. That is a cleaner extract than r1 unwind (no clawback fight, no non-negativity, no second principal). E2E §3.7 “remedial credit path works and is capped” will prove the door opens, not that it cannot be a rebate.

**Fix.** Same control as unwind, or do not ship it in Phase 6:

- Dual-control, distinct token ids, ≥ T seconds, two audit rows, mandatory reason.
- Named per-market **and** daily house cap in 0008; 422 over cap.
- Staging-only **or** explicitly “prod tool” with the same honesty paragraph as unwind: not a user-facing refund, not a PnL make-whole, not available from the public tape.
- Refuse credits to accounts linked to the proposing/confirming principals.
- E2e: finance alone 403; over-cap 422; `Paid` unwind still 409.

### N3. [BLOCKER] D25a — `reprice-market` restores live fee extraction

**Scenario.** Catalog says live pool `fee_bps` is immutable except via `reprice-market` (audited, bounded, **role unspecified**). r1’s attack was: set fee 0 (now 10), friend sizes in, set 1_000 (now 200). Two-phase on `SetConfig` does not bind this route. A single finance (or ops, if that is who they pick) token mid-window reprices one book. Preview is lock-free; unless 409 is **this-market** and converse requires a new yes, the friend executes the cheap quote or the rest of the tape pays the new one.

**Fix.** Either delete `reprice-market` from Phase 6 (next-trade applies to *new* pools only — already the right default) **or** give it the full sensitive-key protocol: two-phase, distinct principals, max-delta 20 bp / 5 min, bounds [10, 200], public `MarketRepriced` frame, PlaceTrade 409 only when **this** market’s `fee_bps` (or a preview-relevant cap) changed, e2e that a global fee `SetConfig` does **not** 409 an existing book.

### N4. [MAJOR] D26 / D24 — two-phase can deadlock legitimate 3am ops; one token can be both principals

**Scenario.** Sensitive keys require finance **or** superadmin to propose and the other to confirm. No reject, no proposal TTL, no emergency revert, no rule that propose/confirm be distinct `token_id`s. Unnamed whether a pending proposal exclusive-locks the key.

- Fat-finger fee (or pending exclusive lock) at 01:00: the other principal is asleep. Nobody can reject. Nobody can apply a fix. Superadmin cannot solo-revert a confirmed 20 bp miss. The crude hatch is `trading_paused` (ops, single principal) — which finding 5 just forbade as a fairness window and which traps positions overnight.
- One bearer with both capabilities clicks twice. E2e “superadmin cannot confirm **own** proposal” is written for unwind, not for `SetConfig`.

**Fix.** Pending proposals expire (e.g. 15 min) and can be rejected by either role (audit row). Confirm requires `confirming_token_id ≠ proposing_token_id`. After apply, keep `previous_value`; a one-step emergency revert is the same two-phase **or** superadmin+reason that *pages* finance and auto-opens a confirm-or-restore ticket — not a trading pause. E2e: same token both roles → 403; reject unblocks a subsequent proposal.

### N5. [MAJOR] D30 — receivable class is a farm if unwind-after-void is the cleanup

**Scenario.** User sells, withdraws, then the market is voided (void-suppress ring, or any thin book with `OI_FLOOR=0`) and ops runs unwind “to restore t0” (e2e (a) teaches that as the happy path). Clawback cannot hit a zero cash row, so the deficit books to `receivable` against house. Ban / shadow-limit / withdraw switches are Phase 7. The user keeps off-platform USDC and can repeat. Dual-control means this is an **ops-policy** farm, not a self-serve button — but the plan’s own e2e makes unwind-after-void look like the rest of settlement.

Void already paid 50 ¢ from escrow. Unwind-from-`Voided` is a **second** money movement. That is legal as a rare fraud tool; it is insolvent as “finish the void.”

**Fix.** ops.md: void is terminal settlement; unwind-from-`Voided` is only “void was the wrong fraud decision,” not t0 cleanup. Users with an open receivable cannot withdraw (Phase 6 can refuse `TxnKind::Withdrawal` while `balance(receivable[user]) > 0` without the full ban switch). E2e (b) already books the receivable — add (b2): that user’s next withdraw 409s. Cap house receivable outstanding or page finance at a named threshold.

### N6. [MAJOR] §3.1 / D28 — convoy-at-hidden−15 s is the right *side*, the wrong *duration*; closes_at is the wrong trade worst case

**Scenario.** D22 already froze the book at `tally_hidden_at`. A convoy at `closes_at` measures votes/resolve, not CPMM lock wait. hidden−15 s is the last-look trade moment I asked for in r1; I was wrong about 15 s being enough.

PlaceTrade holds `market_for_update`. Freeze does not drain waiters (unlike D25 pause fences). 100 serialized trades at the D19 300 ms budget need ~30 s of Live; 2_000 need minutes. Start everyone at −15 s and most nightly agents 423; p95 of the winners understates the lock the SLO is supposed to see. Spec §9: last-**minute** pile-in is the default worst case.

**Fix.** Trade convoy starts at `max(open, tally_hidden_at − 60 s)` (last minute of Live; on a 180 s/`FLASH_TALLY_HIDDEN_SECS=120` book that is t=60). Assert: (i) a configured fraction of trade-capable agents get **2xx before hidden**, (ii) p95 is computed on those 2xx plus in-window 423s separately, (iii) nightly still has a **vote** convoy / 1k-voter settlement at close — that is the other worst case, and it is not a PlaceTrade convoy.

### N7. [MAJOR] §3 — `FLASH_TALLY_HIDDEN_SECS=60` vs “hidden 60 s” is an off-by-one-window trap

**Scenario.** `seed.rs` `market_windows`: both env vars set ⇒ `tally_hidden_at = now + hidden.min(closes)`. So “hidden 60 s” as copied from every existing e2e script means **live=60, closing=120**, not hidden-window-length=60. D24 bound `hidden_window_secs ∈ [60, 900]` and Phase 5 flash default 300 s are lengths. Convoy `tally_hidden − 15 s`, burst `[closes_at − 60, closes_at)`, and “hidden 60 s” then describe three different clocks.

**Fix.** In `scripts/e2e_swarm_smoke.sh` pin `FLASH_CLOSES_SECS=180` and `FLASH_TALLY_HIDDEN_SECS=120` if the intent is a 60 s hidden **window** (and say so). Cross-check `hidden_window_secs < open_secs` against those same numbers. Do not rely on the word “hidden 60 s.”

### N8. [MAJOR] D25a / §3.3 — 409 `StaleConfig` on global next-trade drift breaks lexical confirm in an exploitable way

**Scenario.** Converse: preview → 2 min pending → lexical yes → `PlaceTrade(idempotency_key=pending_id)`. Plan: any generation change that includes a `next-trade` key (fee, **caps**, **flip window**) ⇒ 409; “converse re-previews and re-confirms”; “lexical confirm never fires on a stale quote.”

- Flip window and caps are not two-phase and have no max-delta. Finance (or whoever owns those keys) can bump generation every few seconds. Every in-flight yes 409s.
- If converse treats 409 as “re-preview and execute” (or re-uses the same pending), the user said yes to 100 bp / $25 and pays 120 bp / a new cap — the LLM-free corridor just auto-confirmed a different quote.
- If converse leaves the pending and the next yes retries the **same** `config_version`, the user is stuck until TTL. Confirm-fatigue / support DoS. Pin-a-quote then churn flip-window is the exploit; the steal is not a cheap fill (409 prevents that), it is a killed corridor during a window the operator chose.
- If 409 also fires when global fee changes but **this** pool’s `fee_bps` did not (D25a stamp), every live-book yes dies for a change that does not affect the quote. That is the §3.3 e2e as written.

**Fix.** 409 iff a preview-relevant field **for this market** changed: this pool’s `fee_bps` (only via `reprice-market`, if it survives N3), this user’s effective cap, flip-window if the sell would flip. A global next-market fee `SetConfig` must **not** 409. Converse contract: 409 → expire the pending, send a **new** preview, require a **new** lexical yes; never execute from the old pending; never loop 409 on the same version. E2e: (1) global fee change, existing-book place **200** at old pool fee; (2) `reprice-market` (or delete it), place 409, second yes after new preview 200; (3) flip-window churn does not 409 a non-flip buy.

### N9. [MINOR] D28 — wash-ring assertion is vacuous at published tier 0

**Scenario.** Covered under finding 1. Smoke never leaves tier 0; base fee is the only fee.

**Fix.** Staging override `rep_micro ≥ 400_000` on one wash-pair member (same arm as `created_at`), assert sell ledger fee = 100 bp not 90 bp. Optional: a control sell outside the 3600 s window (impossible on a 180 s market unless `last_buy_at` is also backdated) — if you cannot show the discount *and* its removal, do not claim anti-churn was hit.

---

## Scoring.md vs §3 sanity

| Plan §3 / D28 pin | vs scoring.md / shipped | Verdict |
|---|---|---|
| 100 bp base, 3600 s flip, caps $25–$500, quality floor $50 | matches scoring.md table | OK — **must actually seed these**; `RepConfig::default()` is still MAX/0/0/0 |
| fee bounds [10, 200] bp | min 10 bp published; 200 bp is plan-only (ok) | OK |
| `FLASH_CLOSES_SECS=180`, “hidden 60 s” | scoring last-10-min rule; Phase 5 flash 3600/300; `seed.rs` offset semantics | **mismatch** — N1, N7 |
| “published scoring defaults” **and** “hold low enough that the fat pot enters Resolving” | scoring hold **$500**; code default `i64::MAX` | **unnamed override**. If swarm seeds the $1_000 demo pot, published $500 already holds — pin `$500` and a fat seed, or name `PAYOUT_HOLD_THRESHOLD_MICRO` as smoke-only |
| “product floors for min_votes / OI” | flash `min_votes_floor=3`; OI floor **unpublished**, runtime 0 | **OI number missing**. 0 makes void-suppress trivial (any `N < min` voids). Pin a positive OI floor on the pad/suppress market and 0-or-low on the void market or D21’s two branches collapse |
| integrity profiles never zero age | 72 h / last 10 min | OK as policy; **incompatible with 180 s + young burst** (N1) |
| `SLO_ENFORCE=0`, hold delay excluded from p99 | D19 / spec §9 still claim 300 ms / 1 s / 100 ms as release numbers | OK for Phase 6 if the script prints the disclaimer; nightly “fail if p95 trade > 300 ms” is a real gate — only honest if N6’s convoy actually queues |

## What is sound (still not a pass)

- Generation-serialized snapshot patches + reconciler-as-wakeup (codex #1) is the right config bus.
- Class-4 pause fences above the class-3 LP breaker, votes not bound by trading pause, public pause frames.
- `InvariantReadTx` + six ledger identities + receivables in the contra set.
- simswarm as an outer crate, intent-schedule determinism, two-factor chaos, named crash barrier.
- Thin Playwright in the merge gate is the right call for the iOS window.
- Dual-control unwind with no `Paid` path is the right R3 shape — until N2/N3/N5 are closed.

## Build read

Round-1 BLOCKERs 2–3–4 are not fully closed (carry-in pause, reprice + two-phase identity, remedial credit). Finding 1’s flag proof is still theater at published ppm. Amend D25 / D25a / D26 / D28 / D30 / §3 for **N1–N3** (plan-text, with the inequalities and env pins). Land **N4–N8** in the same amend so 6.5 cannot go green by weakening thresholds or auto-confirming 409s. Task 6.0 may proceed only after 0008’s key catalog includes two-phase + distinct-principal constraints and does **not** grow an unbound `reprice-market` / finance-only credit route.
