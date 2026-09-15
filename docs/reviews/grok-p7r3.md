# grok-p7r3 — adversarial re-review (economics / abuse)

**Plan:** `docs/plans/phase7-money-compliance.md` revision 3
**Maps:** `docs/reviews/p7r2-resolution.md` (all grok-p7r2 B/M/m claimed ACCEPTED), `p7r1-resolution.md`
**Prior:** `docs/reviews/grok-p7r2.md`
**Role:** verify r2 closures; re-run convert arithmetic under provisional-until-Paid; attack the **seeded numbers**; APPROVED or FIX-FIRST with a minimal delta
**Grounding:** `fee_policy.rs` / `apply_fee` ceil, `amm.rs` `round_trip_never_profits`, `ops.md` published caps `$25…$500`, D30 “never from Paid” (`phase6-simswarm-ops.md`), `users.kyc_tier` int, `0001` deposits/withdrawals

## Verdict

**FIX-FIRST** — one catalog line, one shadow seed, one AML predicate, and one e2e/D30 sentence. The r2 egress/convert/SLO holes are closed in the text. 7.0a should not freeze `0011` until the seed snapshot actually validates.

No new self-serve mint. `auto_approve $50` vs `dest_warm_floor $100` is the *right* inequality (auto cannot warm a dest). Do not “fix” that by raising auto.

---

## Per-r2 finding

| r2 | Claimed | r3 evidence | Status |
|---|---|---|---|
| **B1** suspense refund XOR / dest | ALTER `deposits` machine; `admit_tx_id` XOR `refund_tx_id`; dest **=** chain `source_address`; settle `DepositSuspense → External` via `outbound_payments(subject=deposit_refund)`; not a user withdraw; admit∥refund CAS; omnibus review | D32 | **RESOLVED** |
| **B2** RTS sanctions hatch | Egress split: Hit/banned freeze, no send, counsel license dest never user-picked, Hit source never auto-refunds; refused+Clear = source-locked refund; self-ex+Clear = settled dest or source, new dest `risk_hold` | D34 + §3.4 | **RESOLVED** |
| **M1** unwind-after-convert 2G | Fees provisional until `Paid`; convert uses finalized allocations only; Paid-unwind would reverse convert | D32 | **RESOLVED economically** by provisional+D30-never-Paid (void never finalizes, so never converts). **PARTIAL as written:** D32 still invites unwind-from-`Paid` (contradicts D30); §3.5 `grant→convert→void→unwind` is an impossible lifecycle and will go green without testing 2G (new M3). |
| **M2** dest-warming | Warmth = **settled** ≥ `dest_warm_floor_micro` **and** age ≥ `dest_warm_age_hours`; dust never warms; K=2; refund/RTS dests excluded; $1-then-dump stays review | D31 + seeds $100 / 72h + §3.4 | **RESOLVED** |
| **M3** n_min / 2k sampling | Lib `N_MIN_*` tied to convoy ≥50%, ≥1k-voter book, spike ticks; convoy 2xx **in** gated trade series; `close-to-paid` = Closed→Paid minus named hold, never `tally_hidden_at` | D35 + §3.9 | **RESOLVED** |
| **M4** tighten-only + drift | Snapshot ∪ current toward more review; `preview_relevant_drift` + `FeeContext.base_bps` read `fee_bps_override:{market}`; W3 owns `ops/config.rs`; old-gen preview 409 | D31, D36, §2 W3, §3.4 | **RESOLVED** |
| **m1** collect order / `legs.fee` | convert-then-collect in the same `lock_user` tx; increment = Trade Fees credit, never notional×bps | D32 | **RESOLVED** |
| **m2** OTP is a word | `PhoneVerification` port + `phone_verifications.verified_at`; staging two-factor; linked-but-unverified ⇒ grant 0 | D32 + W2 + §3.4 | **RESOLVED** |
| **m3** Fees paying convert | `BonusReserve` segregated; only dual-control `House → BonusReserve` funds it; only convert debits; coverage is an invariant not a 422 | D32 | **RESOLVED** |
| **m4** identity 4 vs observation | Identity-4 extension names observation External legs; suspense Σ = non-terminal liabilities | D32 | **RESOLVED** |

r1 leftovers that r2 left partial (B1 OTP, B5 SLO, B6 drift, M3 catalog-as-prose) are plan-text in r3.

---

## Provisional fees at Paid — arithmetic + timing

r2 table still holds: convert pays \(G\) cash after \(G\) of **finalized** fees; wash net \(\approx -\mathrm{slip}\) at every \(f \in [10,200]\) bp and every published discount. Paid-finalization does **not** reopen the mint. It adds a clock.

**Can an attacker time convert vs void?**

No. Allocations finalize only at `Paid`. `Voided` never finalizes, so it never converts. Choosing a book they expect to void **burns** their own progress. The profitable play is to put wash flow on books that will `Paid` — i.e. honest-looking fat markets — and still finish \(-slip\).

**Does Paid-finalization grief honest users?**

Yes, as **delay / forfeiture**, not as a mint:

| Event | Honest user | Attacker |
|---|---|---|
| Fees on a Live/Closing book | Progress provisional; grant still locked | Same |
| Book `Paid` | Allocations finalize; convert may fire | Same, still \(-slip\) |
| Book `Voided` (D21 thin) | Provisional rows never finalize. Fees stay in `OwnerRef::Fees` unless a rare D30 unwind refunds them. Bonus progress from that book is **gone**. | Same. A suppress ring can *deny* other people’s conversion on a thin book they themselves did not need. |
| Flash vs daily | Convert waits hours–a day per book. A $5 signup at 100 bp needs ~$250 notional of **Paid** fees; tier-0 cap is $25/`ops.md`, so many books, all must settle. | Same capital + time tax. |

That is an acceptable growth-program cost if ops.md says it: **voided-market fees do not count and are not refunded unless unwind**. Do not let W3 “help” by finalizing on `Voided` (that reopens convert-then-unwind 2G).

**Do not put every crossing convert inside `resolve_market`’s Paid commit.** Phase 3 already treats a 1k-voter payout as the p99-1s budget. Finalizing allocation rows in that tx is fine; debiting `BonusReserve` for every qualifier in the same tx is how the 2k profile fails D35. Finalize in resolve; convert lazily on that user’s next `lock_user` (still convert-then-collect). Residual, not a blocker (new m1).

---

## Catalog numbers

Seeds as USDC: min $5, auto $50, dual $500, max $1 000, user-daily $2 000, dest-daily $1 000, hot-wallet $10 000, warm-floor $100 / 72 h, signup/referral $5, referral-min-notional $10, mint-daily $500, AML velocity $5 000 / 24 h, structuring **N=4 / 24 h**, shadow caps $10, confirmations 32.

### What is coherent

**`$50` auto vs `$100` warm floor is correct and must stay strict `<`.** A dest is cold until humans have settled ≥ $100 to it *and* 72 h have passed. Auto-approve cannot contribute the warming dollar (`$50 < $100`). Dust ($5 min) cannot warm. After warmth, only sub-$50 repeats skip review. Add to the validator:

`withdraw_auto_approve_micro < dest_warm_floor_micro`

Do not raise auto to meet the floor.

Hot-wallet $10 k/day is a real backstop: ~200 warmed dests × $50 auto ≈ the cap. Warming those dests first costs finance review of ≥ $100 × N and 72 h. Review fatigue exists; it is no longer free.

Signup $5 / mint $500 ⇒ ≤100 grant identities/day before OTP+device+instrument+dest binds. Convert still costs $5 of Paid fees. Referral $5+$5 on $10 notional mints credits, not cash.

### What is broken

**B1.** Written cross-key invariant vs published seeds.

D24: `min ≤ auto ≤ dual ≤ max ≤ daily ≤ dest_daily ≤ hot_wallet`

Seeds: daily `2e9` ($2 000), dest_daily `1e9` ($1 000). \(2 \times 10^9 \le 10^9\) is false. 7.0a’s snapshot validator (the thing Phase 6 actually runs) **refuses the seed**. That is an unimplementable catalog, not a taste issue.

Economically dest_daily **should** be ≤ user daily (one dest cannot exceed the user cap). The **inequality is backwards**. Flip it; do not raise dest_daily to $2 000 (that weakens the per-dest brake).

**M1.** `shadow_*_cap_micro = 1e7` ($10) vs published tier-0 cap $25 (`ops.md` `position_cap_micro_by_tier[0] = 25e6`).

r2 M1 / D34 require shadow to be indistinguishable from a published tier cap. A probe buy of $15: honest tier-0 **200**, shadow **422**. The user does not need a second principal. Seed shadow caps to the published tier-0 $25 (trade) and the same number they would otherwise see for deposits, or stop claiming the differential probe.

**M2.** `aml_structuring_n = 4` / 24 h with **no** “sub-threshold” amount.

Workers will invent the cutoff.

- If cutoff = auto $50: four $25 onramp legs in a day — normal card/onramp behavior — pages AML. Three $49.99 auto-approved pulls after warmth = $150/day forever, never 4 in a window (slide at 24 h+ε).
- If cutoff = dual $500: three $499 single-finance approvals = $1 497/day unstructured.
- If cutoff = velocity $5 000: almost every honest deposit is “structuring.”

Pin `aml_structuring_threshold_micro` (suggest $500, finance+2P) and apply N to **that** band only. $5–$25 onramp dust must not count. N=4 is then “four just-under-$500 in 24 h,” which is the actual pattern. State the 3-per-window game as accepted (velocity + dest-daily + warm floor still bind).

**M3.** D32 vs D30, and a vacuous 2G e2e.

Phase 6 D30: unwind is legal only from `Voided` or flagged-`Resolving` pre-payout; **never from `Paid`**. Provisional-until-Paid + that rule **already** kills 2G: a voided book never finalized, so it never converted; unwind only refunds fees.

D32 nevertheless says “Unwind of a **Paid** market reverses its finalized allocations and any convert they funded.” That reopens the r1 Paid-unwind refund button. §3.5 `grant → convert → void → unwind` cannot happen on one market (convert requires Paid; Paid is not Voided). Implemented as grant→void→unwind it goes **green with no convert to reverse**.

Minimal close: delete Paid-unwind from D32 (D30 stands). Rewrite §3.5 to two legs: (a) grant → Paid fees → convert (happy); (b) grant → fees → Voided → unwind ⇒ no convert, fees returned, reserve untouched, not +G. If you *insist* on testing convert reverse, that is a D30 ADR + dual-control Paid-unwind, not a sneaky sentence.

---

## NEW findings (only the delta)

### B1. [BLOCKER] Catalog seeds cannot pass the catalog’s own invariant

Evidence above. **Resolution:** validator order is

`min ≤ auto < dest_warm_floor` and `auto ≤ dual ≤ max ≤ dest_daily ≤ daily ≤ hot_wallet`

Keep the seeds ($50 / $100 / $1 000 / $2 000 / $10 000). Add a 7.0a unit test that `validate_patch` of the **seed snapshot** is `Ok`.

### M1. [MAJOR] Shadow seed $10 is a self-tell against the $25 published floor

**Resolution:** `shadow_trade_cap_micro = 25_000_000` (and deposit shadow = whatever honest basic-KYC deposit limit is, once that exists). Probe e2e must use amounts that a tier-0 published cap would also reject.

### M2. [MAJOR] Structuring N=4 is undefined and either deaf or noisy

**Resolution:** add `aml_structuring_threshold_micro` seed `5e8` ($500), count only legs in `(0, threshold)` per user **and** per dest, window 24 h, N=4. E2E: four $499 withdraws flag; four $25 deposits do **not**; three $499 do **not**.

### M3. [MAJOR] Paid-unwind sentence + convert→void e2e will stamp the 2G close green-wrong

**Resolution:** as above — do not reopen D30; split the e2e.

---

### m1. [MINOR] Finalize allocations in resolve; convert on the user’s next `lock_user`

Keeps p99 Closed→Paid honest under a 1k-voter book (D35). Same convert-then-collect rule.

### m2. [MINOR] Say voided fees are forfeited (or refunded only via D30 unwind)

Stops W3 from finalizing on `Voided` “to be nice.” Suppress-ring grief of *other people’s* bonus progress is then a named D21 residual, not a surprise.

### m3. [MINOR] Platform-absorbed SOL gas is a grief budget, not a USDC mint

After warmth, ~200 × auto $50 hits the USDC hot-wallet cap first. SOL treasury still pays ~200 sigs/day at the cap. Name a SOL daily attempt cap next to `hot_wallet_daily_limit_micro` or accept it in ops.md as house opex. Not a convert hole.

---

## What is closed — do not reopen

- Fees≥lot wash mint, with or without Paid-finalization. Discounts change capital, not EV.
- Credits unspendable; two-txn convert from `BonusReserve`; External cash untouched.
- Deposit machine XOR + source-locked refund + generalized outbound attempts.
- Egress split (Hit freeze / source refund / settled-dest self-ex).
- Dest warmth = settled floor + age; auto $50 cannot warm a $100 floor.
- Tighten-only decisions; drift reads the override; version required.
- OTP as a durable `verified_at` fact; grant 0 if missing.
- SLO lib constants, convoy-in-series, Closed→Paid timer, 1k-voter book in the profile.
- Actual D24 table and command matrix (once B1’s inequality matches the seeds).

## Minimal delta for APPROVED

1. Flip the dest/user daily inequality; keep seeds; seed-snapshot unit test.
2. Shadow caps = published tier-0.
3. Add `aml_structuring_threshold_micro` and the three e2e counts.
4. Delete Paid-unwind from D32; split §3.5 into Paid-convert vs Void-unwind.

That is a table-and-sentence pass, not another architecture pivot. After it, r3 can APPROVE. Do not dispatch 7.0a on the invariant-breaking snapshot.
