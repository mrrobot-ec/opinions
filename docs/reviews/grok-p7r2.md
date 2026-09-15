# grok-p7r2 — adversarial re-review (economics / abuse)

**Plan:** `docs/plans/phase7-money-compliance.md` revision 2
**Disposition map:** `docs/reviews/p7r1-resolution.md` (all grok-p7r1 B/M/m + client checklist claimed ACCEPTED)
**Role:** same lens as r1 — verify the text actually closes the farm, then hunt what r2 introduced
**Grounding:** shipped `fee_policy.rs` / `apply_fee` (ceil), `amm.rs` `round_trip_never_profits`, `place_trade.rs` `FeeContext.base_bps = pool.fee`, `preview_relevant_drift` (no override key), `WithdrawalEligibility` still `open==0`, `CreditDeposit` still no `lock_user`, `RingKind` still four variants, `slo_report()` still hardcodes `slo_enforce: false`, `expected_config_version` still `Option`, D30 unwind still “fees reversed to users”, `0001` withdrawals + `OwnerType` still without Withheld/Suspense

## Verdict

**fix-first**

Revision 2 **does** kill the r1 self-serve wash mint: credits are not spendable, convert is two same-currency txns from a capped House reserve, and `fees_paid ≥ lot` is 1:1 with what `OwnerRef::Fees` actually received at every legal `fee_bps` and every published tier discount. Referral binds and red-team legs are plan-text. D31’s lock graph, ALTER-not-CREATE, and required `expected_config_version` are plan-text.

The rewrite also adds two **new egresses** that r1 never had — `DepositSuspense` refund and ban/sanctions **return-to-source** — without an XOR observation state or a dest that is forced to the inbound source. Those are extraction / OFAC-shaped holes. The SLO residual (`n_min` unnamed; 2k profile can drop the convoy / 1k-voter samples) and dest-warming (exit 2 blesses it) are still green-wrong. Do not dispatch 7.0a until the new blockers are plan-text.

## Fees-paid arithmetic (r1 B1, re-run)

D32: a lot converts iff cash trade fees the user **actually paid** to `OwnerRef::Fees` since `granted_at`, each `trade_id` once, oldest lot first, ≥ the lot amount. Credits cannot fund the trades.

Shipped fee identity (`apply_fee` ceil, both buy and sell pay, `effective_fee` = flip? `base` : `max(base − discount[tier], min_fee)`):

Let \(G\) = lot amount, \(f\) = effective fee as a fraction. A wash round-trip of gross \(X\) pays

\[
\text{fees} \approx X\,f + (X(1-f)-\text{slip})f = X f (2-f) - \text{slip}\,f
\]

so the \(X\) that produces \(G\) of fees is \(X \approx G / (f(2-f))\). Net attacker P&L once convert fires is \(+G - \text{fees} - \text{slip} \approx -\text{slip}\). `amm.rs` `round_trip_never_profits` already proves slip \(\ge 0\) even at **0 bp**. So wash is \(\le 0\) at every \(f\).

| Setting | \(f\) | \(X\) per $1 of grant | Attacker net after convert |
|---|---:|---:|---|
| Catalog min / live override floor | 10 bp | ~$500.3 | \(-slip\) |
| Published tier 0–1 (`[0,0,10,20,30]`, base 100) | 100 bp | ~$50.3 | \(-slip\) |
| Published tier 4 | 70 bp | ~$71.8 | \(-slip\) |
| Catalog max | 200 bp | ~$25.3 | \(-slip\) |
| Flip-window sell | always `base_bps` (not discounted) | *higher* fees / notional | still \(-slip\) (they pay more) |
| `min_fee_bps = 10` floor | cannot go below 10 bp | same as first row | \(-slip\) |

Discounts do **not** shrink the dollar cost of conversion. They only change working capital and slip. Position caps (`ops.md` `$25…$500` per market) make a large lot even slower (many books), not cheaper.

The mint is dead **if and only if** progress increments by the Trade txn’s Fees credit (`legs.fee.0`), not by `notional × posted bps`. If W3 computes `gross * pool.fee.0 / 10_000` while `FeeContext` paid 10 bp, a 10× inflate reopens r1. That pin is still missing (new m1).

Unwind is the remaining convert hole (new M1): D30 already reverses fees to users; r2 only reverses *progress*.

---

## Per-r1 finding

| ID | r1 | r2 text | Status |
|---|---|---|---|
| **B1** wash+referral mint | fees≥lot, credits unspendable, server codes, OTP+device+instrument+dest binds, Paid+min notional, mint caps, red-team legs, `ReferralChain`/`BonusWash` | D32 + §3.4 + W4 rings | **RESOLVED** on the mint (arithmetic above). **PARTIAL** on identity: “D21 OTP required for any grant” has no ceremony, no `verified_at`, and the swarm still has only synthetic phones (`ops.md`). A channel-link stand-in reopens A↔B. |
| **B2** illegal `UserCredits→User`; identity 1 lie | two same-currency txns; House reserve; External untouched; identity 3 extended not replaced; manual grants = credits | D32 + D31 Withheld paragraph | **RESOLVED** |
| **B3** lock graph / lien / dest / D8 / CREATE | pinned request protocol, `lock_user`, in-tx auto-collect, GET is not the caller, ALTER 0001, dest novelty+confirm-echo, per-user/dest/hot-wallet caps, user pays gas, chaos + concurrent contract | D31 | **RESOLVED** as a request protocol. Dest *warming* after first review is a new hole (M2). |
| **B4** geo/KYC/AML ordering | observation≠availability, fail-closed on grant/admission/trade/vote/withdraw, converse uses KYC region, signed inbox, AML at-request + per-dest, dual-control clear, `pause_deposits` gates admission | D32–D34 | **RESOLVED** for inbound/oracle. Refund / return-to-source is a new outbound bypass (B1/B2 below). |
| **B5** vacuous SLO | lib constants, `--slo-gate` flips enforce only, empty series fails, live `close-to-paid`/`ws-delivery`, timer = send→2xx, grep deleted, 2k is the release artifact, chaos-delay excluded | D35 + §3.9 | **PARTIAL** — mechanism stated; `n_min` has no number; 2k profile does not force convoy 2xx or a 1k-voter close into the gated series (new M3). Shipped `slo_report()` still hardcodes `slo_enforce: false`; that is expected until W4. |
| **B6** live fee sandwich | finance+2P, [10,200], Δ≤20 bp/5 min, generation bump, `MarketFeeChanged`, version **required** 422 | D36 + §3.11 | **PARTIAL** — RBAC/version are plan-text. Shipped `preview_relevant_drift` computes `effective_fee` from `ctx.pool_fee_bps` and only watches `min_fee_bps` / `fee_discount_bp_by_tier` / flip window. A `fee_bps_override:{market}` change row leaves `fee_before == fee_after` → **no 409**. `FeeContext.base_bps` is still `pool_row.pool.fee.0`. Neither file is in §2. (new M4) |
| **M1** shadow detectable | homogeneous errors, self-read `visible`, no public WS, no `/me.status`; shadow withdraw → `risk_hold` | D34 | **RESOLVED** in text. E2E §3.7 names the differential probe. |
| **M2** 0011 CREATE / kyc enum | ALTER withdrawals; kyc stays int 0/1/2; OwnerType ALTERs | D31, D33, 7.0a | **RESOLVED** |
| **M3** catalog attributes | “full D24 attributes” + cross-key `min ≤ auto ≤ dual ≤ max ≤ daily` | D24 paragraph | **PARTIAL** — obligation is written; the **table of numbers** is not (no auto_approve, no K, no grant sizes, no `n_min`, no dest/hot-wallet caps). `ops.md` still lists `feature_referrals` as ops-direct. Workers will invent seeds. |
| **M4** §12 surfaces / stranded / sweeps | self-exclusion, user deposit limits, ban except return-to-source, `bonus_structure` | D34 | **RESOLVED** as a surface list. Return-to-source dest is the new bypass (new B2). |
| **M5** exit criteria lack the attacks | §3.4 red-team list | §3.4 | **RESOLVED** for r1 attacks. Missing: dest-warmup dump, unsigned dest on refund, sanctions-RTS, unwind-after-convert, omit-override-key 409. |
| **M6** status ≠ config | `users.status` under `lock_user`; next-request; drain-safe | D31.4 / D34 | **RESOLVED** |
| **m1** deposit `lock_user` | `CreditDeposit` gains it; cash-creating paths collect | D31 | **RESOLVED** in text (code still unlocked — W3). |
| **m2** daily window | rolling 24h UTC; reserved by requested+held+sent | D31.6 | **RESOLVED** |
| **m3** `feature_referrals` | two-phase; refuses `true` until binds exist | D32 | **RESOLVED** in the plan; catalog/ops.md still contradict (M3 residual). |
| **m4** shadow can withdraw | shadow withdraw always `risk_hold` | D34 | **RESOLVED** |
| **m5** post-credit reorg | accepted house loss; never reversed; ops.md | D32 | **RESOLVED** |
| **m6** no bonus rings | W4 `ReferralChain` + `BonusWash` | §2 W4 | **RESOLVED** as a named ring. `RingKind` still has four variants until W4. |

### Client-input checklist

| Input | r2 | Status |
|---|---|---|
| `dest_address` | pubkey + confirm-echo + novelty in fingerprint | **RESOLVED** for *user* withdraw. Refund/RTS dest is unspecified (new B1/B2). |
| `amount_micro` | in-tx available, min≥dust+gas, daily reserved | **RESOLVED** |
| `expected_config_version` | required, 422 if absent | **RESOLVED** in text; DTO still `Option` until W3. |
| Referral code | server-issued, bind-checked | **RESOLVED** |
| Onramp / KYC webhook | signed inbox; user from our records; hash algebra | **RESOLVED** |
| Geo / `X-Forwarded-For` | `TRUSTED_PROXY_CIDRS` verbatim; converse = KYC region | **RESOLVED** |
| Client `chain_sig` | watcher; bound to user/address/mint/amount/slot | **RESOLVED** |
| `credit_wager_progress` | replaced by server-side grant lots | **RESOLVED** |
| Sandbox KYC-full | staging two-factor | **RESOLVED** |

---

## NEW findings

### B1. [BLOCKER] `DepositSuspense` refund is a second spend path with no XOR and no bound dest

**Plan:** D32 “every finalized inflow books `External → DepositSuspense` always; admission moves `DepositSuspense → User`; refusal leaves suspense + pages; **refund = return-to-source via the D31 send protocol**.”
**Shipped:** `deposits.status` is `seen|confirmed|credited` with one `txn_id` (`0001`). Identity 4 pairs that `txn_id` with exactly one External cash leg. There is no observation-state column. D31 request, used as-is, requires `kyc_tier ≥ withdraw_kyc_tier` and books `User → Withheld` — money in Suspense is **not** User cash.

r2 created a custodial bucket that must egress either to User **or** to chain, once. It specified neither the state machine nor the dest.

Extracts / failures:

1. **Admit + refund.** Two workers (or admit + compliance refund) both see `DepositSuspense > 0`. One books `Suspense → User`, the other runs D31 send and eventually `Withheld → External` (or worse `Suspense → External`). Without `observation_status observed|admitted|refunded` and a CAS, this is a double release against one External debit. Identity 3 breaks or the chain is short.
2. **Dest substitution.** “Return-to-source via D31” invites a D31 dest field. If that field is client- or ops-supplied, the no-KYC inbound is a **mixer**: send from X, fail admission, refund to Y. Source must be the **observation fact’s** sender/token-account, copied into the send-attempt bytes, not a request parameter.
3. **D31 KYC vs no-KYC refund.** A refused user cannot pass D31.4. Either refund is impossible (inbound stranded — ops invents a hatch) or refund skips KYC (the hatch). The hatch has to exist and must be dest-locked to source.
4. **Identity 4 / settle shape.** Observation already spent the External leg (`External → Suspense`). Refund cannot be a second `Withheld → External` unless the observation is first reversed (`Suspense → External` as the settle, **not** the D31 User-hold path). Reusing D31’s settle identity (d) double-counts External.

**Resolution:** Observation authority: `deposit_observations(status observed|admitted|refunded, source_address, chain_sig, suspense_tx_id, admit_tx_id, refund_tx_id)` with CHECK exactly one of admit/refund once terminal. Refund is **not** a user `POST /withdrawals`. It is a compliance send: dest **equals** `source_address`, bytes persisted before broadcast, settle = `DepositSuspense → External` keyed `deposit-refund:<id>`. No User credit, no Withheld. E2E: admit∥refund CAS — one wins; forged dest 422; identity 3/4 and recon stay green.

### B2. [BLOCKER] Return-to-source + “screened dest” is a sanctions / ban dest hatch

**Plan:** D34 ban blocks every mutation **except** “compliance return-to-source withdrawal to a **screened dest**.” D33 Hit ⇒ dual-control compliance hold. Self-exclusion also excepts return-to-source. D32 refused deposits refund via the same sentence.
**Shipped:** nothing; this is a new door.

“Screened dest” is not “source of our observation.” A banned or Hit user (or their friend on the finance token) picks any dest that currently `Clear`s — mule wallet, exchange deposit, mixer. That is dest substitution with a compliance badge.

Worse: **returning USDC to a Hit source is itself a sanctions send.** Fail-closed for Hit is freeze-in-place (Suspense or Withheld), not D31 broadcast.

Self-exclusion is the opposite product (user should get funds out) and must not share the sanctions path.

**Resolution:** Split the doors.

| Status | Egress |
|---|---|
| `banned` or sanctions **Hit** | No send. Funds stay User/Withheld/Suspense. Dual-control *license* only after counsel-shaped dest (not user-picked, not automatically the source if the source screens Hit). |
| Admission refused, sanctions **Clear** | B1 refund to observation `source_address` only. |
| Self-exclusion, Clear | Return-to-source to a dest this user has **already settled to**, or the observation source — still D31, still novelty/cluster/AML. Not a new dest. |

E2E: Hit user + Clear mule dest 403s; Hit source does not auto-refund; self-exclusion to a brand-new dest is `risk_hold`, not auto.

---

### M1. [MAJOR] Unwind reverses fees (and progress) but not the convert those fees bought

**Plan:** D32 “Unwound trades reverse their attributed fee progress via compensating fact.”
**Shipped:** D30 unwind “trade fees reversed to users”; `unwind_market.rs` proves Fees back to 0 and users back to t0 cash.

Sequence (dual-control, but a money invariant, not an honor system):

1. Grant \(G\), wash until `Fees += G`, convert `House → User` \(G\).
2. Unwind the book: user receives the fee cash **again**; progress compensating fact zeros `consumed_fee`; lot still has `converted_at`.
3. User keeps converted \(G\) + returned fees \(G\). House reserve is empty; Fees is empty. **2G extract**, admin-gated.

If the compensating fact also *reopens* the lot (`converted_at` cleared) without reversing the cash, they convert a third time on the next book.

**Resolution:** Unwind of a trade that funded a convert must, in the **same** unwind tx: reverse the convert (`User Usdc → House Usdc`, `House UsdcCredit → User UsdcCredit`) or refuse (409) if the user cannot cover (receivable for the convert cash, credits restored). Converted lots never accept new fees. E2E: grant → convert → void → unwind ⇒ credits back, bonus reserve restored, user is not +2G.

### M2. [MAJOR] Exit 2 teaches dest-warming; review fatigue cashes it out

**Plan:** D31 first-time dest and dest shared by ≥K users never auto-approve. §3.2: “novel-dest forces review first; **second withdrawal to the now-known dest auto-approves**.” K, dest daily cap, hot-wallet cap, auto_approve are unnamed (M3 residual).

Attack: \(N\) sybils (or one user, \(N\) wallets) each withdraw dust to a unique dest. Finance clears the queue (small, under dual-control — the named single-finance exception). Dest is now “known.” Second pull is `auto_approve − ε`. Unique dests **avoid** the ≥K cluster rule. Per-user daily cap is the only brake, and it is unpinned.

Return-to-source / a settled refund marking that source “known” is the same warmup for free.

**Resolution:** Dest is auto-approve-eligible only after **settled** (not approved) cumulative ≥ a pinned floor **and** age ≥ T. Dust does not warm. K pinned (suggest 2). Per-dest and hot-wallet daily caps named in the D24 table. Refund/RTS dests do not enter the warm set. Red-team: $1 then dump stays in `review_required`. Delete the “second withdrawal auto-approves” sentence or replace it with the floor.

### M3. [MAJOR] `n_min` and the 2k profile are still harness-shaped

**Plan:** D35 empty/thin series fail; `n_min` “pinned”; release = gated smoke **and** a 2k profile with “10% money path, close spike, chaos-healed.”
**Shipped:** `percentile` of `n=1` is that sample; runner still does not record `close-to-paid` / `ws-delivery`; Phase 6 already knows a 1k-voter settlement vs p99 < 1s is in tension with `sweep_delay_secs=180`.

W4 owns the gate **and** the script. Unnamed `n_min` becomes 1. The 2k profile can:

- put close-spike / 1k-voter / money-path withdraws **outside** the gated series (“chaos-healed” / “quiet segment”);
- populate `trade-confirm` from the non-convoy 90%;
- never open a fat book, so p99 close-to-paid is a 3-voter flash.

That is r1 B5 with the flag on.

**Resolution:** Pin numbers in the lib, not the script: `N_MIN_TRADE` ≥ convoy 2xx count (reuse the ≥50% of trade-capable agents rule), `N_MIN_CLOSE_TO_PAID` ≥ paid markets in the profile including one ≥1k-voter book (p99 **excludes** configured hold, as D28 — say so), `N_MIN_WS` ≥ a real tick count during the spike. Gated 2k series **includes** the convoy 2xx. Chaos-delay stays excluded. Fat settlement either meets 1s after hold exclusion or the profile fails honestly.

### M4. [MAJOR] “Current policy at decision/send” loosens held rows; drift does not see the override

Two related holes.

**Loosen.** D24: “Held rows use **current** policy at decision/send.” Fail-closed on a KYC downgrade is right. The same sentence auto-approves a row that was `review_required` once someone raises `withdraw_auto_approve_micro`, warms the dest (M2), or clears a flag. Decision may only **tighten** vs the snapshotted `risk_reasons` / policy_version in the fingerprint.

**Silent reprice.** D36 claims the override bump makes `preview_relevant_drift` fire. The shipped function never reads a `fee_bps_override:*` key and always uses `ctx.pool_fee_bps` as base (`config.rs` ~998–1007). `place_trade.rs` ~140–146 sets `base_bps: pool_row.pool.fee.0`. Required `expected_config_version` 409s only if drift returns true. As written, live override + a client that **does** send the old version still **fills at the new fee**.

`ops/config.rs` and the `FeeContext.base_bps` line are not in §2 (7.0a “validator extension” is not this predicate; W3 owns `place_trade.rs` but not `preview_relevant_drift`; W4 owns the op).

**Resolution:** Snapshot risk at request; send/decide = union(snapshot, current) toward more review, never less. Name `preview_relevant_drift`: `fee_bps_override:{this market}` changes `base_bps` (override replaces pool stamp). `FeeContext.base_bps` reads the same. Owner = W3 (sole `place_trade` owner) + 7.0a signature freeze on the predicate, or stop the wave and remanifest `ops/config.rs`. E2E: override with a preview stamped at the old generation → 409 even when min_fee/discount did not move.

---

### m1. [MINOR] Convert/collect order and the progress increment are still prose

Cash-creating paths “invoke the same collection routine” — not **after** the credit. Convert-then-collect in the same `lock_user` tx is the only order that cannot leave converted cash spendable against an open receivable for a tick. Progress += **that Trade’s Fees credit**, leftover rolls to the next lot; never `notional × bps`.

### m2. [MINOR] Grant OTP is a word; channel-link will become the stand-in

`PhoneVerificationRequired` today is `user_has_channel("imessage")` (Phase 2). Phase 6 still defers possession-proof. r2 puts OTP “in scope for any grant” with no vendor, no `user_channels.verified_at`, no staging two-factor for the swarm. W3 will accept a linked channel; `ReferralChain` goes green; A↔B is a pair of `POST /users` with two synthetic phones.

**Resolution:** `verified_at` required for grant issuance. Swarm OTP is the same two-factor staging arm as the faucet (audited, unmountable in prod). E2E: linked-but-unverified channel ⇒ grant 0.

### m3. [MINOR] House reserve vs Fees: “made whole” is a consolidation story

Convert pays from the **bonus reserve**, not from `OwnerRef::Fees`. Fees stay \(+G\), reserve \(-G\). Platform PnL nets if you consolidate; the reserve can still 422 while Fees is fat. That is fine if the reserve is topped by dual-control (same grade as remedial). Say so, so W3 does not “helpfully” convert from the Fees account (mixes fee revenue with growth spend and breaks D8 / reporting).

### m4. [MINOR] Observation External legs vs identity 4

Identity 4 is still “credited deposit ↔ one External leg.” Observation books External before admission. If W3 reuses `TxnKind::Deposit` + `deposits.txn_id` for the suspense txn, admission must **not** write a second External leg. If it uses a new kind, identity 4 / recon (D35) must name observation facts. Pin it next to D31 (a)–(e).

---

## What is closed (do not reopen)

- Fees-paid 1:1 + unspendable credits **as a wash mint** (arithmetic table). Discounts change capital, not EV.
- Two-txn convert from House; External cash untouched; identity 3 auto-includes new OwnerTypes.
- Withdrawal ALTER + `lock_user` + in-tx collect + dest confirm-echo + user-pays-gas, as a *request* protocol.
- Fail-closed KYC/geo/sanctions on grant / admission / trade / vote / withdraw; signed webhooks; AML at-request.
- Required `expected_config_version` (once drift actually sees the override).
- Shadow homogeneity; status under `lock_user`; self-exclusion surface; `bonus_structure` gate.
- Red-team legs for the **r1** farm (wash-after-grant, A↔B once, third-device zero, unsigned webhook, no-KYC chain → suspense).

## Build read

Revision 2 is a real pivot, not a sticker pass. The r1 mint is dead in the text. The new suspense/RTS egresses, dest-warming, unwind-after-convert, and still-gameable SLO/drift predicates are not. Amend those, then r3. 7.0a should not freeze `0011` until observation XOR + dest-bind and the `preview_relevant_drift` signature are in the plan.
