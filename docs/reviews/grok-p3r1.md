VERDICT: fix-first

Reviewed `docs/plans/phase3-economy-integrity.md` against spec §§2.3 / 3.2–3.4, decisions D4/D5/D6/D21–D23, current `cast_vote.rs` / `resolve_market.rs` (Phase 2 integrity + settlement shape), and `web/` structure (`LeaderboardSlot` empty stub, `how-it-works` placeholder copy). Phase 3’s *direction* is right: EWMA rep with real gates, payout-hold sweep, LP kill-switch, leaderboards filling designed slots. The plan under-specifies adversarial economics (fee-discount wash, EWMA farming) and ops surfaces (curator inbox); a few definitions would ship misleading honesty claims in reports and leaderboards.

## FINDINGS

[B1] **Fee discounts lower the cost of wash / churn trading.**  
Tier fee discounts (`fee_discount_bp_by_tier`) make round-trip market churn cheaper exactly for accounts that already have capital and high tier. Combined with a trader leaderboard ranked on **realized-in-window** PnL/volume-adjacent activity and a public tape (D6), a high-tier account can manufacture tape + fee-favored churn more cheaply than tier-0. The plan never names wash-trading as an attack surface or pairs discounts with a min-hold / max-churn / same-market flip penalty.  
**FIX:** State explicitly that discounts apply only to **net economic** exposure (or document acceptance); at minimum add (a) discount does not apply to self-crossing / flip within T minutes, or (b) leaderboard and fee-discount eligibility exclude trades closed within a short window; pin a test that a buy+sell same market within window does not earn the full discount benefit. Do not ship discounts as pure volume subsidy.

[B2] **Position cap binds on gross cost, but stack amplifies sybil capital efficiency.**  
Cap is `position.cost + gross ≤ cap[tier]` (buys only) — good: discount does not inflate the cap. But climbing tiers via EWMA (Task 3.0–3.1) **simultaneously** raises position cap *and* cuts fees. Spec §3.3 wants new accounts low; the attack path is many small, easy-score markets → tier climb → larger bags with cheaper churn. No “markets counted toward EWMA must pass a quality floor” (min OI, min unique voters, min pot).  
**FIX:** (1) Only update rep when the scored market had `votes ≥ min_votes_to_resolve` and `open_interest ≥ floor` (voided markets already skip scores — keep that); (2) optionally require `escrow ≥ rep_score_min_pot` before `update_rep`; (3) document that tier climb is *slow* under honest H=20 and that farming thin markets is intentionally unrewarded.

[B3] **Integrity sweep false-positive posture is under-specified for viral flash markets.**  
The four checks (burst, young share, subnet, device) are a reasonable *minimum* relative to §3.4’s “anomaly job,” but a legit viral flash market looks exactly like `vote_burst` near close. A **flag freezes payout for all holders** until a curator acts — false positives are not “soft”; they are capital lockups. Plan has `min_votes_for_rules` and ppm thresholds but no dual-baseline (e.g. compare final window to *same-hour historical* markets), no auto-pass on multi-signal absence, and no SLA for curator response.  
**FIX:** (1) Require **≥2 independent flags** before `verdict=flag` (or severity tiers: soft-hold vs hard-hold); (2) document false-positive policy in `integrity_reports.checks` and on the scoring page; (3) add curator inbox (see B4) so flags don’t die in the outbox.

[B4] **Curators cannot find flagged markets.**  
Flag path reuses `FlagCuratorNeeded` + `CuratorNeeded` outbox event, but Phase 3 routes only add leaderboards, fee summary, and seed force — **no** `GET /admin/markets?under_review=1` / curator queue. Spec §10.2 and D21 assume a human decision path; without a list, `under_review` is a dead-end in the product.  
**FIX:** Add to Task 3.2 or 3.3: admin list of markets with `curator_flagged_at IS NOT NULL` or `integrity_reports.verdict='flag'`, plus the existing resolve/void body. Web admin can stay minimal (curl-ok) but the API must exist.

[M1] **Weekly voter leaderboard by score *sum* rewards volume over accuracy.**  
`top_voters` = sum of `vote_scores.score_bp` in window. A user voting 50 mediocre markets outranks one accurate vote on a hard market. Spec §3.2–3.3 sell “understanding the crowd,” not “vote a lot.” Tier eligibility for leaderboards (spec) is not enforced in the query either.  
**FIX:** Prefer `avg(score_bp)` (or sum with **min markets scored ≥ N** and display avg + n), and filter `tier ≥ 1` if you want to keep zero-rep noise off the board; document the formula in utoipa + how-it-works.

[M2] **Trader leaderboard SQL can double-count or over-claim “skill.”**  
“Payout ledger legs + sell-trade realized in window” is honest only if sell path does not also create payout-like legs and if buy→hold→payout isn’t double-counted with intermediate sells. Realized-in-window also **ignores open mark-to-market** — fine if labeled, toxic if the UI says “Top Traders” without “settled PnL only.”  
**FIX:** Write the exact SQL in the plan with a fixture that (a) sell-then-repurchase, (b) hold-to-payout only, (c) void market — and pin expected rows; utoipa description must say **not mark-to-market**. Confirm sell realized is only the position-accounting delta already on `positions.realized_pnl` at sell time, not re-derived from gross.

[M3] **LP kill-switch is seed-time-only — acceptable if stated louder; window sum is timing-gameable.**  
D23 wants auto-pause on realized LP PnL threshold. Seed-only means **live markets keep trading into adverse selection** after the switch trips — plan says this once in checkpoint 7 but not in Task 3.3 body. Cumulative `lp_pnl_sum(window_days) ≤ threshold` can be gamed by (a) concentrating losses just outside the window, (b) seeding many small markets before a known bad week.  
**FIX:** (1) Put seed-time-only + “live markets unaffected” in the Task 3.3 behavior contract and admin fee/LP dashboard copy; (2) define threshold as **sum of lp_pnl_micro over markets resolved in window** (not calendar accrual ambiguity); (3) optional: also block `SeedMarket` when *trailing* LP PnL per-tier breaches, not only global sum.

[M4] **Sybil-signal honesty is mostly good; overclaim risk is in the verdict UX.**  
`TRUST_PROXY` default-off, converse null IP/device, skip device/subnet when coverage &lt; half — all correct. Risk: `integrity_reports.verdict='flag'` and web “fairness review” copy can read as **proven fraud** when the signal is a weak /24 or spoofable `localStorage` id. Spec still lists “device fingerprinting” as launch-blocking; plan correctly ships a weak id — good if reports label confidence.  
**FIX:** Each check object in `checks` jsonb must include `{name, value, threshold, coverage, strength: "weak"|"medium", note}`; web under-review copy must say “automated heuristics, not a guilt finding.” Do not call localStorage “fingerprint” in user-facing prose.

[M5] **Published scoring / integrity page content is not specified enough.**  
Task 3.4 lists topics but does not pin the **formula** (75/25, accuracy kernel width 25pp, majority at exact 50% both sides — D4/§3.2 already implemented in `domain::scoring`), the **hold rule** (threshold, expected 2–5 min, curator path), or the **Phase 2 bar** (phone channel, velocity, young-account near close). Current `web/app/how-it-works/page.tsx` is aspirational fluff.  
**FIX:** Task 3.4 must include a content checklist with the exact formulas and rules (or a `docs/copy/scoring.md` source of truth the page renders). Without numbers, we fail the “publish the formula / publish the rule” superiority claim.

[M6] **Rep / tier visibility vs D6.**  
Plan puts `{rep_micro, tier}` on profile/positions, not on the public trade tape — coherent with “everything public” only if profiles are public (D6 activity tape + profile pages). Leaderboards show handle + dollar PnL / score — that *is* D6. Missing: whether **rep tier appears next to tape handles** (plan doesn’t; keep it off the tape to avoid targeting high-rep wallets).  
**FIX:** Explicit non-goal: no tier badge on public tape in Phase 3; profiles and leaderboards only.

[M7] **Resolve path + 1k-voter rep updates.**  
Checkpoint 2 is real: per-voter `rep_for_update` inside settlement after scores is O(voters) row locks in an already heavy tx. Phase 3 scale may be fine; not stated. Also: current `ResolveMarket` already walks Closed→Resolving→Resolved→Paid in one execute; Task 3.2 changes Closed→ either settle or hold at Resolving — ensure **scheduler** owns Resolving→settle and that HTTP `POST /admin/.../resolve` doesn’t short-circuit a flagged pot without curator override.  
**FIX:** Document max voters budget or batch rep updates after commit via outbox worker if &gt;N; add test: flagged market rejects non-curator resolve; curator path still settles once.

[m1] **Notifications for `RepUpdated` / `CuratorNeeded`.**  
Spec §7.1 lists `rep_tier_change` and ops need curator alerts. Plan correctly keeps WS lifecycle for Resolving but defers notification fanout. Acceptable for Phase 3 **if** admin list exists (B4); otherwise curators rely on log grepping.  
**FIX:** Defer push/WS user notifications explicitly; do not defer **admin discoverability**.

## CHECKPOINTS (plan’s 8)

1. **EWMA integer math:** Design is sound (K_PPM, floor divide, no floats). ±1 ulp half-life tests are honest **if** vectors are precomputed in integer — require golden vectors in tests, not “approximately half.” Negative rep/score rejected — good. **No issue if implemented as written; watch K rounding direction (round half-up stated).**

2. **Rep in the resolve tx:** Lock order must stay serialize_key → market → (accounts) → per-user rep locks in **stable user-id order** to avoid deadlocks when many voters. 1k-voter blow-up is a real risk at flash-viral scale — plan should state “acceptable for Phase 3” or batch (M7). Replay via `resolve:<market>` must not re-apply EWMA — test is mandatory.

3. **Fee-discount coherence:** Preview/execute/ledger same figure is correctly required. Converse preview path must use the same core `/trades/preview` (it does today) — still pin an e2e that a tiered user sees discounted fee in converse text. **B1** is the larger economic issue than path drift.

4. **Sweep semantics:** Hold uses locked escrow — good. Report unique + lifecycle key make double-settle hard. **Flagged market must not auto-settle** — stated; add negative test for scheduler loops. Passed sweep double-settle: only if resolve key missing — existing resolve key should prevent. **False-positive cost (B3) is the product risk.**

5. **Sybil-signal honesty:** TRUST_PROXY default-off is correct. /24 is a proxy for ASN — deferred list says so; force that into `checks[].note`. Device localStorage is weak — good if not overclaimed (M4). Converse null metadata skip — correct.

6. **Trader-leaderboard SQL:** Definition needs fixture-level honesty (M2). Indexes (`vote_scores_time_idx`, `ledger_entries_txn_idx`) help; ensure payout join uses `kind='payout'` and time on txn created_at. O(window) ok at Phase 3 volume.

7. **Kill-switch placement:** Seed-time-only is acceptable for Phase 3 **iff** stated in product/admin copy and Task body (M3). Live markets continuing is an explicit tradeoff with D23 “optionally pause the market” — Phase 3 chooses weaker lever; name it.

8. **No-VCS + no-polling:** Plan is clean. Leaderboards REST-on-load (no intervals) does **not** violate D11. Keep it that way — no `setInterval` refresh in 3.4.

## WHAT CHECKS OUT

- Architecture split (pure EWMA/anomaly in domain, ports, fake+Pg contracts) matches standing dependency rule and coverage bar.
- Payout hold only above threshold preserves sub-second resolve for small pots (spec §4.3 / §3.4).
- Voided markets not scoring (already in `resolve_market.rs`) correctly prevents free majority rep — keep when adding EWMA.
- D5 points-only voting + D6 public tape remain coherent; leaderboards are the right place for public PnL, not a soft contradiction.
- Deferral list (OTP, true ASN, strong fingerprint, dynamic config) is scope-honest **if** B4/M4/M5 land.

## BUILD READ

Do not start 3.1–3.2 implementation until **B1–B4** plan text is amended (discount abuse posture, EWMA market quality floor, sweep multi-signal / false-positive policy, curator list API). M1–M2 (leaderboard definitions) should land in the same amend so SQL and web copy don’t encode volume-farming by default. Task 3.4 can stub UI against empty leaderboards in parallel once M5 content checklist exists.
