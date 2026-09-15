# grok-p7r1 — adversarial design review

**Plan:** `docs/plans/phase7-money-compliance.md` revision 1 (pre-review)
**Role:** independent of a correctness pass — economic exploits, abuse orderings, client-trusted inputs, and green-wrong gates
**Grounding:** spec §§9–13, D1–D30 + Phase 6 shipped code (`place_trade.rs`, `credit_deposit.rs`, `ops.rs` `WithdrawalEligibility`, `ledger.rs`, `invariant_sweep.rs`, `simswarm` `LatencyLog` / `main.rs` / `runner.rs`, `migrations/0001_init.sql`, `scripts/e2e_swarm_smoke.sh`)

## Verdict

**fix-first**

Hold-first withdrawals, confirmation-depth deposits, dual-control above a threshold, and “credits are not withdrawable” are the right shapes. Revision 1 still leaves several **self-serve cash extracts** and a **harness that can stamp the compliance/SLO gate green without exercising the attacks**. Do not dispatch 7.0a / W1–W4 until the blockers are plan-text.

The Phase 6 wash pair, receivable lien, and `SLO_ENFORCE=0` disclaimer are not closed by adding a withdrawal row and flipping a flag. They become the farm.

## Findings

### B1. [BLOCKER] Wager-through + double-sided referral is a self-serve mint

**Plan:** D32; §3.5; spec §11
**Shipped:** `PlaceTrade` is a CPMM against house escrow (`place_trade.rs` user/escrow/fees legs only — no counterparty). Flip-window sells already pay base fee (`fee_policy.rs`). `CreateUserCmd` has no referrer field. D21 phone OTP is still deferred (Phase 6 stand-in). `RingKind::WashPair` already buys then sells. Domain forbids mixing `usdc` and `usdc_credit` in one ledger txn (`ledger.rs` `cross_currency_two_leg_transaction_rejected`).

Spec §11 sells wager-through as “killing the deposit-bonus-withdraw fraud loop.” D32 does not define the quantity that kills it.

Unset, and therefore farmable:

| Knob | What the plan says | What an attacker does |
|---|---|---|
| `wagered_micro` | “settled trade notional” | Count buy **and** sell gross (`quote_legs` `gross` is collateral on buy, proceeds+fee on sell). One round-trip against the pool doubles progress. |
| Whose money | “cash before credits” (monotone) | Deposit or hold any cash; spend cash first; credits convert without ever being at risk. Credits-only path: spend credits → sell position for cash → withdraw. |
| `credit_convert_multiple` | unnamed config | Default/e2e can be 1. Round-trip cost is ~2× `fee_bps` + slippage ≈ 2%. Multiple must be **≳ 1 / round-trip cost** (~50 at 100 bp) before wash is unprofitable. 0 is instant convert. |
| Wash exclusion | none | Same user, or the Phase 6 wash pair, inside `discount_flip_window_secs`. Exit 5 is the honest one-direction path — it will go green. |
| Referral bind | “both legs on the referee’s first settled market” | Client-supplied code (the only place W3 can hang it — `CreateUserCmd` today). A refers B refers A: four grants / two people. Chain A→B→C… is +EV if grant ≫ dust trade + fee. “Settled” can be a public flagship they dust-trade, or a `Voided` 50¢ book if void counts. |
| Identity cost | none | No KYC on signup grant. No phone uniqueness (still deferred). No device / dest-wallet / funding-instrument bind between referrer and referee. `feature_referrals` is **ops-direct** (`ops.md` / `config.rs`). |

**Extract (lower bound).** Signup grant \(G_s\) + both-leg referral \(G_r\) each, times \(N\) sybils, minus ~2% wash cost if multiple is small. No deposit required if credits are spendable and sell proceeds are cash.

**Resolution (plan-text, pinned):**

1. **Notional** = buy collateral actually debited this user this txn (sells never increment). Exclude flip-window volume and any buy whose inventory is fully sold inside `discount_flip_window_secs` (reverse the increment on the sell, same txn or compensating fact).
2. Pin `credit_convert_multiple ≥ 50` (or equivalently: progress = **fees paid to `OwnerRef::Fees`**, convert when fees ≥ grant — the house is paid for the bonus). Fail-closed if the key is unset. Catalog: finance+2P, bounds `[50, 1000]`, not ops-direct.
3. Cash-before-credits stays; conversion may only move **remaining unspent** `UsdcCredit`. Spending the bonus does not mint it back as cash.
4. Referral: server-issued code, not a client `user_id`. Bind referrer≠referee on verified phone (D21, no longer deferrable for this grant), device HMAC, and first funding instrument / dest wallet. One grant per bind key. Both legs fire only if the referee’s first **Paid** market has referee buy-notional ≥ a pinned floor (not void, not dust). Daily/house **signup+referral mint cap** (same grade as remedial: 0008 already has `$500` / `$2,000`).
5. E2E must **fail** the farm: wash pair after signup grant does **not** convert; A↔B referral loop grants once; third account on the same device/phone gets \(G_s=0\).

### B2. [BLOCKER] `UserCredits → User` is the conversion the ledger already rejects; D31’s identity then lies

**Plan:** D32 “`credit_convert` books `UserCredits → User` cash atomically with the trade settlement”; D31 “identity 1 becomes deposits − settled withdrawals = user + pools + fees + **withheld**”; grant is `External → UserCredits`.
**Shipped:** `Balances::apply` rejects a two-leg `TxnKind::CreditConvert` that crosses `UsdcCredit` → `Usdc` (`ledger.rs` test name: `cross_currency_two_leg_transaction_rejected`; comment: an all-currency scalar sum would “readmit credit→cash conversion”). `0001_init.sql` same warning on the sum-zero trigger. Identity 3 is `−External = Σ(non-external)` (`invariant_sweep.rs`). Identity 4 pairs **deposit facts** with exactly one External cash leg — it does not know converts. Remedial `CreditGrant` already mints **USDC cash from House**, not credits (`remedial_credit.rs`).

If W3 copies the D32 sentence:

- One ledger txn is a domain error. “Atomic with the trade” is a **second** `ledger_apply` in the same DB tx, two currencies, or it does not compile against D13.
- `External → User` cash on convert is an unbudgeted mint. D31’s restated identity 1 is then false (convert is neither a deposit nor a withdrawal). Workers who implement that sentence as a sweep check will either (a) fail after the first convert or (b) drop the check to go green.
- The same restatement **omits house and escrow**. `SeedMarket` already moves House → Escrow. A literal D31 identity 1 fails on every live book, withheld or not.
- Identity 3 still holds if Withheld is a new non-external `OwnerType` (it falls into `!= External`). That is the identity to extend — not a deposit-minus-withdraw slogan.
- W3 “manual grants use the D30 template” will mint **cash** if they copy `remedial_credit.rs`. D32 said credits.

**Resolution:**

- Grant: `House UsdcCredit → User UsdcCredit` (house credit pool pre-funded by genesis / dual-control top-up, capped).
- Convert: **two** ledger txns in one DB tx — `User UsdcCredit → House UsdcCredit` and `House Usdc → User Usdc`. Never touch cash External. Identity 3 unchanged; identity 1 in deposit/withdraw terms stays true; identity 4 stays deposit/withdraw facts only.
- Withheld is a new `OwnerType` (ALTER `ledger_accounts.owner_type` CHECK + `ledger_accounts_singleton_uk` / owner-shape — “one per currency, non-negative”). Sweep identity 3 automatically includes it; do **not** replace it with D31’s four-term sum.
- Manual Phase-7 grants are `UsdcCredit`, not a second cash remedial.

### B3. [BLOCKER] Withdrawal request is not in the lock graph; the Phase 6 lien is the wrong caller; `dest_address` is trusted

**Plan:** D31 hold-first in the same tx as insert; gates include `WithdrawalEligibility`; dest is a column; decision pass is “an application service, not a route”; chaos is only `withdraw_send_crash`.
**Shipped:** `WithdrawalEligibilityView.eligible = open_receivables_micro == 0` (`unwind_tx.rs`, `fakes/ops.rs`) — any dust receivable blocks **all** cash, and the view is a **lock-free read**. Auto-collect runs only inside `CreditDeposit` (`receivable_collection.rs`) and that path **does not `lock_user`** (only `serialize_key` on the deposit idempotency key). `PlaceTrade` / `CastVote` / comments take class-2 `lock_user` first. `0001` already has `withdrawals(status in queued|risk_hold|sent|settled, txn_id, dest_address)` — one ledger txn, no deny/failed, no hold/release pair. D31’s “new table” in `0011_money.sql` collides.

Races / extracts:

1. **Two withdraws vs one balance.** Distinct idempotency keys. Without `lock_user` (or `SELECT FOR UPDATE` on the user USDC account) inside the hold tx, both read “available = cash”, both attempt `User → Withheld`. Non-negativity may save you with a 5xx; it is not a typed 409 and it is not specified. Deposit+withdraw use different serialize keys, so they do not serialize with each other today.
2. **Request vs deposit auto-collect (the named race).** User: $100 cash + $50 receivable.
   - If the caller is today’s boolean: withdraw 409s forever until they deposit enough to collect — dust $1 freezes $10k (the grok-p6r3 NEW-5 hole Phase 6 “fixed” only on deposit).
   - If W1 treats eligibility as “has cash” and forgets the lien: withdraw holds $100, deposit later collects from the next inbound. Receivable dodged; house short the $50.
   - Correct shape (already written in grok-p6r3, accepted then narrowed): **in the withdraw tx**, `lock_user` → auto-collect `min(cash, Σ open)` → hold only the remainder → 409 only if the requested amount still exceeds post-collect cash. The Phase 6 GET is not that caller.
3. **Request vs `PlaceTrade`.** Trade holds `lock_user` then market. Withdraw must take the **same** user lock before reading available / booking Withheld, or a $100 user can fill a $100 buy and a $100 withdraw. Ledger non-negativity is not a product contract.
4. **Decision-pass limbo.** Row enters `requested` with money already in Withheld. Crash before approve/hold and there is no reconciler for `requested` (only sent-but-unsettled). Funds stuck; retry can double-decide unless status is CAS’d.
5. **`dest_address` is client-supplied.** No Solana format check, no confirm-echo, no bind to `wallets.address` (already in `0001`), no “new dest” risk reason, no cluster-across-users. Sybil ring withdraws to one hot wallet; or withdraws to another user’s deposit address and the D32 watcher credits them (internal peel). Auto-approve `< withdraw_auto_approve_micro` never looks at dest novelty.
6. **Who pays D8 network cost?** Unspecified. If house pays gas, dust withdraws (per-tx min unnamed) drain the hot wallet. Spec 5.3 also wanted a **global** hot-wallet / cold-treasury daily cap — D31 is per-user only. \(N\) sybils × auto-approve = hot-wallet empty.

**Resolution:**

- ALTER `0001.withdrawals` (do not CREATE). Status machine: keep spec 5.6 `queued → risk_hold → sent → settled` and add `denied|failed`; `hold_tx_id` NOT NULL after queue; `release_tx_id` on deny/fail; `settle_tx_id` on settle. CAS every transition.
- Request tx: `serialize_key` → **`lock_user`** → pause/KYC/geo/sanctions/AML-at-request (B4) → auto-collect → available = user USDC (already net of prior Withheld) → book `User → Withheld` + insert. Same one-way lock graph as PlaceTrade (user before any money write; never take market locks).
- Dest: validate pubkey; require an explicit confirm field equal to dest; first-time dest and dest reused by ≥K users are `risk_reasons` and **never** auto-approve. Per-user **and** per-dest **and** global hot-wallet daily caps in the catalog. User pays network cost from the requested amount (min ≥ dust + gas).
- Decision pass in-process after the insert CAS, or a leased job that CAS `requested → approved|held`. Stuck-`requested` is a D35 page, not a comment.
- Chaos: also kill between hold and decision, and run concurrent withdraw+trade+deposit under the Pg contract.

### B4. [BLOCKER] Geo / KYC / sanctions / AML are ordered so money and the oracle move first

**Plan:** D33 deposits ≥ `deposit_kyc_tier`, withdraws ≥ `withdraw_kyc_tier`, geo on trade/deposit/withdraw; sanctions at KYC completion and every withdraw; D34 `aml_sweep` is a **scheduled** job; open flags only block **auto-approve**. Fail-closed on geo error for money paths. Webhooks “driven” / “dedup by `(provider, event_id)`.”
**Shipped:** `users.kyc_tier` is already `int not null default 0` (`0001`). Chain credits are `CreditDeposit` by `chain_sig` with **no KYC read**. `wallets(address unique)` already exist as deposit addresses (spec 5.3 embedded wallet). Vote metadata trusts `x-forwarded-for` only behind `TRUSTED_PROXY_CIDRS` (`routes/mod.rs` `vote_metadata`); a new `GeoResolver` that reads the header without that rule is client-supplied geo. Converse `POST /trades` is server-to-server (D16) — HTTP middleware sees the converse/Sendblue IP, not the user. CastVote is not in D33’s enforce list. Identity 4 / deposit watcher will credit whoever owns the wallet the USDC landed on.

Bypass orderings:

| Ordering | What happens |
|---|---|
| Signup → trade/vote → KYC | Credits convert (B1). Oracle votes from a blocked region. D2 was “USA-only; KYC rides the onramp.” |
| Chain send to `wallets.address` | Watcher credits at depth. `deposit_kyc_tier` never ran if it only wraps the onramp webhook. |
| iMessage corridor | Geo of the money path is the converse box. Entire channel is one region or fail-open. |
| `X-Forwarded-For` | Client-supplied region unless the vote_metadata trusted-proxy rule is reused verbatim. |
| Onramp / KYC webhook | `event_id` and completion payload are provider-supplied. No HMAC/signature, no destination-user bind in the plan. Replay or forge → credit / `kyc_tier=full` on an account we did not verify. Sandbox “KYC to full” (exit 2) must not exist as a public/staging-unarmed route. |
| Sanctions only at KYC + withdraw | Sanctioned user deposits (chain or onramp race), trades, votes, refers. Withdrawal `held`. Book and oracle already moved. |
| AML sweep after the fact | Auto-approve is `amount < withdraw_auto_approve_micro AND sanctions pass AND no **open** flag`. Structure \(N-1\) sub-threshold **withdrawals** (plan only structures **deposits**) to dest D, all auto-approve, then the sweep flags. Money is gone. Converted credits are not deposits, so deposit-velocity never sees the B1 farm. Per-user window only — 100 sybils × just-under-threshold to one dest is unstructured. Clearing a flag is **single** finance + reason (unwind-grade everywhere else). |
| `pause_deposits` | Spec §10.2. D32 watcher does not mention it. Pause the onramp, chain still credits. |

**Resolution:**

- KYC+geo+sanctions **fail-closed** on: signup grant, every `CreditDeposit` (watcher and onramp), PlaceTrade, CastVote, withdraw request. Converse must pass the **user’s** last-known region (channel country / onramp KYC country), not the box IP. Reuse `TRUSTED_PROXY_CIDRS` for `GeoResolver`.
- Watcher credits only if `users.kyc_tier ≥ deposit_kyc_tier` **in the credit tx**; otherwise `deposits.status='seen'` and do not `ledger_apply`. Same for `pause_deposits`.
- Webhooks: verified signature, destination user derived from **our** records (wallet / session), never from the JSON `user_id`. Dedup after auth.
- Sanctions: also at deposit credit and first trade; a hit freezes status to a dual-control compliance hold (not just withdraw `held`). Never auto-cleared; clear is D30-grade.
- AML **at request**, not only scheduled: per-user **and** per-dest sliding velocity + structuring on deposits **and** withdrawals, including converted-credit cash-outs. Open flag ⇒ no auto-approve **and** no send. Sweep is the backfill. Pin N, window, threshold in the e2e (not “a series”). AML clear = dual-control.
- Catalog every new key with D24’s four attributes (see M5). `deposit_kyc_tier` / `withdraw_kyc_tier` unset ⇒ fail-closed (already said) **and** not writable to `none` by a single ops token.

### B5. [BLOCKER] The SLO gate, as specified, will go green without measuring the SLOs

**Plan:** D35 `LatencyLog::slo_report()` “becomes a gate”; `--slo-gate`; targets constructor params **pinned in the script**; exit 9 = smoke swarm with the flag on.
**Shipped:**

- `slo_report()` **hardcodes** `slo_enforce: false` (`trace.rs`).
- `main.rs` always `eprintln!("SLO_ENFORCE=0 (measurement only)")`.
- `e2e_swarm_smoke.sh` **requires** that string (`grep -q 'SLO_ENFORCE=0'`).
- Runner records `trade-confirm` / `vote` / `no-op` only (`runner.rs`). `close-to-paid` and `ws-delivery` are written in a unit test, **never** in the live runner. `percentile` on an empty vec is `None`.
- Timer starts **after** `fetch_market_view` + `decide`; one `elapsed` is applied to every action in a ring batch.
- Smoke is 100 agents. Spec §9 / D19 release gate is the **2,000-agent** swarm at those numbers. Phase 6 already documented that a 1k-voter settlement vs p99 < 1s is in tension with `sweep_delay_secs=180`.

Gaming the harness (W4 owns the gate **and** the script):

1. Leave `close-to-paid` / `ws-delivery` empty → `None` → treat as pass.
2. Keep the `SLO_ENFORCE=0` grep; print both strings.
3. Pin constructor targets to 30s / 10s / 5s “in the script.”
4. Gate the 100-agent smoke, not the convoy samples, not the 2k nightly, not chaos (`CHAOS_RELAY_DELAY_MS=1500` is 15× the WS SLO — run it on a different leg).
5. One lucky 2xx makes p95 defined (`percentile` of `n=1` is that sample).

**Resolution:**

- `slo_report(enforce, targets)` — targets are **constants in `simswarm` lib** equal to spec §9 (300 / 1000 / 100), not script argv. `--slo-gate` only flips enforce. Env cannot change targets.
- Fail if any required series has `count < n_min` (pin `n_min`, include convoy 2xx in the trade series). Empty ≠ pass.
- Actually record `close-to-paid` (hidden→Paid, hold delay excluded as in D28) and `ws-delivery` (server stamp → client receipt) in the runner.
- Delete the `SLO_ENFORCE=0` grep. Exit 9 is: smoke with gate on **and** the nightly 2k profile with gate on (or the phase does not claim D19). Chaos WS delay is not in the gated sample set; say so.
- Timer is HTTP send → 2xx, per action, not post-view batch elapsed.

### B6. [BLOCKER] `reprice-market` reopens the live fee sandwich D25a existed to kill

**Plan:** D36 ops capability, per-market `fee_bps_override`, new trades only, “coherence rides `expected_config_version` + 409 StaleConfig”; “this drift IS market-relevant.”
**Shipped:** `trade_fee_bps` is finance+2P, **new-market-only**, Δ≤20 bp / 5 min (`ops.md`, `config.rs`). `preview_relevant_drift` explicitly **ignores** global `trade_fee_bps` on existing books. `PlaceTradeCmd.expected_config_version` is `Option<i64>` and the HTTP DTO is optional — **omit the field, skip the staleness check** (`place_trade.rs` `if let Some(previewed)`). Auth is still `ADMIN_TOKENS_JSON` bearers. No `MarketFeeChanged` public frame.

Sequence: friend (or stolen ops token) sizes a preview at 10 bp override → public still quoting 100 → flip to 200 after fill. In-flight previews 409 only if they sent a version **and** the override write actually appears in `fence_changes_since`. A separate override table that does not bump `config_generation` / write `config_changes` is a silent reprice. Raw API clients omit the version and execute at whatever is current — the sandwich.

This is grok-p6r1 finding 4 / D25a, weaker RBAC (single **ops**, not finance+2P), on a **live** book.

**Resolution:**

- Same control grade as `trade_fee_bps`: finance+2P, bounds `[10,200]`, Δ≤20 bp / 5 min, delay T. Ops cannot do it alone.
- Override write **is** a generation bump with key `fee_bps_override:{market}` so `preview_relevant_drift` is true for that market. Public `MarketFeeChanged` outbox/WS in the same tx (D25 pause precedent).
- `expected_config_version` **required** on PlaceTrade (422 if missing). Converse already has to carry it.
- `FeeContext.base_bps` reads the override, not only `pool.fee`. Preview too.
- E2E: omit version → 422; mid-live reprice → in-flight place 409, new preview shows new fee, same-token confirm 403, no fill at a fee the preview did not show.

---

### M1. [MAJOR] Shadow-limit is detectable with the surfaces Phase 4–6 already ship

**Plan:** D34 `shadow_limited` ⇒ trade/deposit caps, votes count, “no fanout of their comments.” Ban is overt 403.
**Shipped:** `AppError::PositionCapExceeded { cap_micro, tier }` Display is in the HTTP `{code,message}` envelope (`error.rs`). Comment listing already shows `Shadow` rows to the **author** and hides them from others (`fakes/social.rs`). Comment DTO sends `moderation_status: "shadow"`. Sequential vote numbers are public (D6).

Detection: compare 422 message cap to the published tier table; post a comment, read it as self, 404 as an alt; `GET /me` if it returns `status`. Votes still increment the public seq — that path does not leak, the others do.

A shadow program the user can prove they are in is just a worse ban.

**Resolution:** Homogeneous errors (same code/message as a normal cap, do not echo the numeric cap or a `ShadowLimited` code). Author-visible comments stay `visible` on self-read; no `CommentCreated` on the public market WS; no `status` on self profile. E2E: limited user cannot distinguish from a published-tier cap without a second principal; the second principal’s public feed is the only proof, and that is ops-only.

### M2. [MAJOR] `0011` “new withdrawals table” + `users.kyc_tier` enum rewrite the frozen Phase 0 schema

**Plan:** 7.0a owns `migrations/0011_money.sql` creating `withdrawals(...)` and `users.kyc_tier (none|basic|full)`.
**Shipped:** both already exist (`0001`: `withdrawals` with a different status CHECK and a single `txn_id`; `users.kyc_tier int default 0`). `OwnerType` / `ledger_accounts` CHECK has no `withheld`.

A CREATE will fail the empty-DB e2e. A silent second table splits authority. Workers who squeeze hold+release into one `txn_id` lose the deny reversal.

**Resolution:** 0011 is ALTER + new satellite tables (`withdrawal_events`, `aml_flags`, `kyc_events`, `sanction_screenings`, `credit_wager_progress`). Map `0|1|2` ↔ none|basic|full or add a text CHECK in place. Withheld ALTER as in B2.

### M3. [MAJOR] New money/compliance keys have no D24 attributes

7.0a “catalog seeds for every new config key” does not classify them. Phase 6 taught that an unclassified key is an extract button (`ops.md` table is the contract).

At least: `deposit_kyc_tier`, `withdraw_kyc_tier`, `withdraw_auto_approve_micro`, `withdraw_dual_control_micro`, `withdraw_daily_limit_micro`, `withdraw_min_micro`, `withdraw_max_micro`, `credit_convert_multiple`, `credit_signup_micro`, `credit_referral_referrer_micro`, `credit_referral_referee_micro`, `blocked_regions`, AML velocity/structuring knobs, shadow caps, `deposit_confirmations`, `pause_deposits`, `pause_withdrawals`, stuck-withdraw SLA, `fee_bps_override:{id}`.

Invariant: `withdraw_auto_approve_micro ≤ withdraw_dual_control_micro`. Convert multiple as in B1. KYC tiers not writable to `none` except superadmin+2P. `pause_*` = ops immediate, public frame.

Without this, B1–B4 are one `SetConfig` away from being turned off.

### M4. [MAJOR] Spec §12 software surfaces that this phase claims to be the compliance gate do not appear

Phase 7 charter: “every software surface those [counsel/onramp] gates need.” Spec §12 non-negotiables also include **self-exclusion** and **user-set deposit limits** (responsible gaming), plus MSB/custody as a track. Plan has ops shadow-limit (not self-serve), no self-exclusion, no user deposit-limit table, no return-to-source for a banned/sanctioned user (D34 ban 403s **every** authenticated mutation — including withdraw — so dual-control ban **strands** cash by construction). Convert-to-cash also pre-empts the still-`[open]` sweepstakes structure (D32 vs spec §12 / D13 dual-currency rationale).

**Resolution:** Self-exclusion use case (user-initiated, dual-control lift, blocks grants/trades/deposits/withdraws except a named return-to-source). User deposit limits. Ban/sanctions: mutations blocked **except** a compliance withdrawal to a screened dest (or forced return-to-source). Convert-to-cash behind an explicit `bonus_structure=real_money|sweeps` config; sweeps must not mint withdrawable USDC.

### M5. [MAJOR] Exit criteria do not contain the attacks this review is about

§3.5 is an honest convert. §3.6 is deposit structuring, unpinned. §3.7 is a cap, not a leak test. §3.9 is the vacuous SLO. No concurrent withdraw+deposit+trade. No dest-cluster. No wash-pair-after-grant. No A↔B referral. No omitted `expected_config_version`. No unsigned webhook. No chain-deposit-without-KYC.

W4 / e2e will be green and the farm will ship.

**Resolution:** Add pinned legs that must go **red** on the B1–B6 sequences, or do not claim the compliance gate.

### M6. [MAJOR] User-status propagation is not the config notifier

Exit 7: “banned user’s trade 403s mid-session (propagation without deploy).” Ban is `users.status`, not a `config_entries` key. PlaceTrade today reads `user_rep` under `lock_user` and never a status. Reusing the config watch will miss it or invent a shadow config key.

**Resolution:** Read `users.status` under the existing `lock_user` (authoritative, next request). No cache in the demo token. “Mid-session” = next HTTP request; in-flight PlaceTrade that already passed the fence completes (drain-safe, D25). Shadow/ban are not config.

---

### m1. [MINOR] `CreditDeposit` still has no `lock_user`

Any Phase-7 withdraw/collect/convert that shares cash with the deposit path must add it. Do not assume the deposit tx serializes against withdraw because both touch the user account — they take different advisory keys.

### m2. [MINOR] Daily withdraw window is unspecified

UTC midnight double-dip (max at 23:59 and 00:01) is the default if you only say “daily.” Pin TZ, include in-flight holds in the used amount, and put it in the AML-at-request math.

### m3. [MINOR] `feature_referrals` is an ops-direct boolean

Turning it on with B1 unfixed is the mint. Two-phase + require the bind keys in B1 before the flag can be true (prospective-snapshot check).

### m4. [MINOR] Shadow-limited users can still withdraw converted credits

D34 restricts trade/deposit/comment fanout. Cash-out is unrestricted. If shadow is a fraud holding pen, withdraw must be `held` at least.

### m5. [MINOR] Confirmation depth after credit is accepted house risk — say so

D32: reorg before depth = no credit. Post-credit deeper reorg is unhandled. Pin `deposit_confirmations` and write “after credit, reorg is house loss; we do not reverse.” Otherwise W3 will invent a reverse path that fights D13 non-negativity if the user already traded.

### m6. [MINOR] simswarm has no referral / bonus-wash ring

`RingKind` is AgedSybil / WashPair / VoidSuppress / ThresholdPad. B1’s e2e cannot be a bash loop of `curl` if the swarm is the abuse harness. Add `ReferralChain` + `BonusWash` or the pinned inequalities will not exist.

---

## Client-supplied values (checklist for the next revision)

| Input | Trusted by r1? | Required rule |
|---|---|---|
| `dest_address` | yes | Format + confirm-echo + novelty risk (B3) |
| `amount_micro` | yes (typed) | In-tx available; AML-at-request; min≥gas |
| `expected_config_version` | optional | Required (B6) |
| Referral code | implied | Server-issued, bind-checked (B1) |
| Onramp `event_id` / KYC webhook body | yes | Verify signature; user from our records (B4) |
| `GeoResolver` IP / `X-Forwarded-For` | unspecified | Same trusted-proxy rule as votes (B4) |
| Client-posted `chain_sig` | if a route exists | Watcher only; never credit from a body `user_id` |
| `credit_wager_progress` | should be derived | No client write |
| Sandbox “set KYC full” | exit 2 | Staging two-factor only, like the faucet |

## What is sound (do not reopen)

- Hold-first + Withheld so queued cash cannot be double-spent **once the lock graph exists**.
- Dual-control above a threshold, deny reverses the hold, rails idempotent by withdrawal id + `sent_at` CAS.
- Credits excluded from withdrawable cash; cash-before-credits as an *ordering* (not as a wash defense).
- Confirmation depth before first credit; onramp dedup **after** auth.
- Ban dual-control because it strands; shadow single-ops.
- Alerter on invariant breach / 1 µ drift / stuck sent.
- Chaos `withdraw_send_crash` as one of several crash points, not the only one.

## Build read

Revision 1 is **not** buildable as a money/compliance gate. 7.0a would freeze the wrong withdrawals DDL and an incomplete port set (no lock-graph notes, no webhook-auth contract, no required `expected_config_version`). W1–W4 would implement a farm, a vacuous SLO, and a live fee sandwich. Amend, then another adversarial round.
