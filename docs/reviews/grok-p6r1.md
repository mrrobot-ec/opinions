BLOCKED

Adversarial review (round 1, grok) of `docs/plans/phase6-simswarm-ops.md` against spec §§9, 10, 13, `PLAN.md`, D1–D23, and published `docs/copy/scoring.md` + `docs/copy/curation.md`. Direction is right (public-API swarm, versioned config, pause above the class-3 LP breaker, unwind as a distinct fraud tool, chaos on existing idempotency). Economics, game theory, and ops realism are not. Independent Poisson personas cannot express the attacks Phase 3 was built to see; D25 pauses the oracle that D22 and `CastVote` explicitly leave open; D30 reopens the R3 historical-replay hole; the 100/5 min merge gate never closes a default-seeded market. Do not dispatch the 6.1–6.4 wave until the BLOCKERs below are plan-text.

## Findings

1. **[BLOCKER]** Independent D28 personas cannot stress the market they claim to test.
   **Plan section:** D28; §0 swarm; §4.1 smoke.
   **Failure/abuse:** Personas are weighted action tables with pure `(persona, market_view, rng) → Action` and one Tokio task per agent. That produces uncorrelated flow. It does **not** produce the attacks spec §3.4 names as the real insolvency path, nor the Phase 3 signals that exist to catch them.

   | Listed persona | What it actually hits | What it misses |
   |---|---|---|
   | whale | Large CPMM buys if faucet + `RepConfig` caps stay `i64::MAX` | Informed adverse selection (D23). A whale that cannot see / steer final `v` is variance, not the LP drain model. |
   | dabbler | Dust, pool-favor rounding, conservation | Wash / self-cross (buy-leg discount still on; flip window default is **0**). |
   | voter-only | Participation count | Threshold **padding** and **suppression** (D21). Random extra votes move `N`, they do not sit on `min_votes_to_resolve ± 1`. |
   | close-sniper | Last-second `PlaceTrade` before `tally_hidden_at` | Hidden-tally last-look (book is frozen). Young-account near-close (D21 / scoring.md 72h / last 10 min) — every swarm user is brand-new. |
   | panic-seller | Uninformed dumps into house LP | Coordinated dump **into** a pause or **out of** a fee flip. |
   | noise | Load | Nothing integrity-shaped. |

   Missing **coordinated** rings (one scheduler, shared RNG stream, shared `x-device-id` / `/24` / channel-prefix):

   - **Wash pair / self-crosser** — A buys, B sells the same complete-set inventory (or one account flips inside `discount_flip_window_secs`) to print tape and harvest remaining buy-leg discounts. Phase 3 anti-churn is sell-side only (`docs/copy/scoring.md`); the swarm never exercises it if `DISCOUNT_FLIP_WINDOW_SECS` stays at the hardcoded 0 default.
   - **Sybil cluster vs Phase 3 sweep** — N fresh accounts, one device HMAC, one subnet, burst in `[closes_at − burst_window, closes_at)`. That is the two-signal flag path (`vote_burst` + `device_concentration` / `young_account_share`). Independent agents with unique devices never trip `integrity_reports`.
   - **Void-hunter / padder (D21)** — Losers suppress votes so `votes < min_votes` **and** OI stays under `OI_FLOOR_MICRO` (runtime default **0**, so any thin book auto-voids). Winners pad just over `min_votes_to_resolve` to force a tally. Spec + scoring.md treat this as first-class; D28 has no persona on the threshold.
   - **Brigade voters + comment last-look** — Phase 4 already accepted free-text “I voted YES” during the hidden window as a residual. No agent posts comments, so the known oracle side-channel is untested. Report-brigade economics (age/tier floor) are also untouched.
   - **EWMA farmer** — Accurate guesses on many easy, above-floor markets → tier climb → published caps $25→$500 and fee discounts. Half-life is 20 markets. A 5-minute / 1–2 market smoke cannot move rep; `RepConfig::default()` also sets `rep_score_min_pot_micro = 0` and `fee_discount_bp_by_tier = [0;5]`, so even a long run does not see published-rules posture.
   - **Self-referral ring** — Honest miss: referrals are not built (Phase 5 deferred codes). Name it as a Phase 7+ persona when §11 credits land; do not pretend D28 covers growth fraud.

   Close-window spike of independent snipers **does** convoy the `market_for_update` row lock. That is load, not game theory. It does not stress D21, D22 (they cannot trade once frozen), D23 informed flow, or Phase 3 EWMA.

   **Fix:** Add a `Ring` abstraction in `simswarm/src/domain/`: `k` agents, shared `device_id` / source IP / referrer, a single decision fn `(ring, market_view, rng) → Vec<(agent, Action)>`. Ship at least four rings in `--profile smoke` and `full`: wash pair, sybil voter cluster (same device + young), void-suppress (holders on the losing side stop voting / recruiting), threshold-pad. Seed a slice of agents with backdated `created_at` (or inject `Clock` on the server) so the 72h near-close rule is **hit**, not total (all-new + 300s market + `near_close_secs=600` ⇒ **zero votes**, accidental void). Seed config from **published** scoring.md numbers (caps, 3600s flip window, $50 quality floor, 100 bp base), not `RepConfig::default()`. Pin `x-device-id` + forwarded-for behavior on the HTTP client. E2E must assert: sybil ring produces a `flag` report on a fat pot; wash pair pays base fee on the sell leg; pad/suppress sit on the D21 branch (void vs `NeedsCuratorDecision`).

2. **[BLOCKER]** D25 pauses `CastVote` — that is an oracle kill switch, not a trading halt, and it breaks D22.
   **Plan section:** D25; §4.4 kill-switch e2e.
   **Failure/abuse:** D22 and `cast_vote.rs` are explicit: during the hidden window **trading** freezes; **voting continues until `closes_at`** (`tally_hidden_at` is deliberately not consulted). D25 puts `trading_paused` / `market_paused:{id}` on PreviewTrade / PlaceTrade / **CastVote**, after `serialize_key`, 423 `TradingPaused`.

   Abuse:

   - Pause **during Closing**. Public copy (scoring.md) says you may still vote. Ops (or a stolen ops token) freezes the tally wherever it currently sits. Admin surfaces are not the hidden-tally audience — ops can still read counts the book cannot. That is the last-look D22 exists to kill, restored as a privileged button.
   - Pause votes when the operator (or a friend) is losing the live tally and OI is under the floor → force D21 auto-void (neutral 50¢) instead of a losing redemption. Inverse: unpause a padder ring in the last seconds after raising `min_votes_to_resolve`.
   - E2E §4.4 only checks **trades** return 423. It will go green while the oracle-pause bug ships.

   Snapshot lag (<2s) makes this worse: some `CastVote`s land, then the fold arrives, then they stop. The last voters are whoever’s in-flight requests won the race — including the operator’s own.

   **Fix:** Split keys. `trading_paused` / `market_paused:{id}` bind **only** PreviewTrade + PlaceTrade (and any future cancel). Add `voting_paused` (default false) as a **superadmin + reason + public `MarketVotingPaused` outbox/WS frame**, dual-controlled, forbidden once `now ≥ tally_hidden_at` unless the market is also being voided. D25 corridor text must quote the `CastVote` comment. E2E: pause trading in Closing → votes still 200, trades 423; `voting_paused` during hidden window is 403/409, not 423-on-vote.

3. **[BLOCKER]** D30 unwind-after-resolution is a captured-superadmin refund button and reopens R3.
   **Plan section:** D30; §4.7 RBAC e2e; spec §4.2 / D21 R3.
   **Failure/abuse:** Spec already learned this: historical replay of a market “can debit users who already withdrew and are not non-negativity-safe.” Void is therefore **current-state 50¢ from escrow**. Unwind is the leftover “admin fraud tool whose clawbacks may queue as receivables.” D30 instead says: superadmin, Resolved **or** Voided, compensating entries, **restore pre-market balances**, conservation by construction.

   Who gains:

   - **Losers / house after an informed drain.** Resolve (or void), then unwind. Anyone who still has cash is clawed back to t0; anyone who **withdrew** cannot be clawed without a negative user account (forbidden by D13) or a silent skip (breaks “restore pre-market”). Fast winners keep money; slow winners pay. System is short the withdrawn residual.
   - **Rogue or phished superadmin.** `ADMIN_TOKENS_JSON` is a bearer map (real auth is Phase 7). One token, one POST, flash market already `Paid` in <1s. Audit + “finance cannot unwind” deters a sloppy teammate. It does not deter a stolen token and it does not require a second principal.
   - **Fee / realization / rep mismatch.** Plan reverses “trades’ net cash, payouts.” Unstated: fee-account keep vs refund; `realizations` (trader leaderboard); `vote_scores` / EWMA (voided markets must not mint majority rep — scoring.md; unwind of a **resolved** market would leave scores for a cash-rollback). Leaderboard and bankroll then disagree.

   E2E §4.7 unwinds a **voided** market and checks conservation. That is the easy case (no one should have withdrawn a 50¢ void in the smoke). It never withdraws a winner and then unwinds a **Resolved** market.

   **Fix:** Rewrite D30 against R3, not against a clean t0:

   1. Legal only from `Voided` **or** `Resolving`+flagged **before** payout, **or** `Paid` only when no deposit/withdraw of that market’s proceeds has occurred (query realizations + subsequent `TxnKind::Withdrawal`). Otherwise refuse.
   2. Clawback path: if `user` cash < clawback, write a `receivable` ledger class (new, or `house` receivable + user memo), **never** a negative user row, **never** a skipped leg. Invariant sweep must know receivables.
   3. Dual-control: superadmin **proposes**, finance **confirms** after `T` seconds, two audit rows, reason required. Single token cannot settle.
   4. Same tx: reversing `realizations` facts; **no** EWMA rewrite on a previously resolved market (scores stay; copy says formulas do not change). Fees: pick keep-fees (documented tax) or reverse-to-users; test both conservation and “no free unwind-wash.”
   5. E2E: (a) voided, no withdraw — balances match t0; (b) resolved + winner withdrew — 409/receivable, no negative, conservation holds; (c) finance alone 403; (d) double POST same unwind key is a replay.

4. **[BLOCKER]** D24 tunables have no per-key bounds, no live-market immutability class, and no role matrix — ops (or a stolen ops token) can extract value.
   **Plan section:** D24; D26; §4.3 fee-flip e2e; spec §10.2 list.
   **Failure/abuse:** Spec’s configure-everything list is the attack surface: `trade_fee_bps`, per-tier `seed_micro`, rep thresholds + position caps, hidden-window length, integrity thresholds, vote velocity, cadence, payout-hold, (later) withdrawal limits, feature flags. Plan: typed write validation, lock-free snapshot reads, audit+outbox in the same tx. E2E flips `trade_fee_bps` and checks preview. That is not a bound.

   Concrete extraction / sabotage (all are single writes today):

   | Key | Abuse |
   |---|---|
   | `trade_fee_bps` | Set 0, friend (or ops sock) size in, set 1_000. Preview is lock-free and **already** allowed to go stale vs `PlaceTrade`. Mid-window fee flip is a private rebate. `Pool.fee` is today’s canonical base; hot-swapping it off the row onto a snapshot is a behavior change Phase 3 did not authorize. |
   | `position_cap_micro_by_tier` / tier thresholds | Caps default `i64::MAX`. Published table is $25–$500. Ops can raise caps or drop thresholds so a fresh faucet account is a whale, then dump into the house seed (D23). |
   | `*_SEED_MICRO` / `daily_seed_budget_micro` | Next `SeedMarket` / approve over-seeds; sock cluster harvests adverse selection. Phase 5 floors exist on **approve**, not on a later config write that those floors read. |
   | `min_votes_to_resolve` / `OI_FLOOR_MICRO` | Lower both → force resolve of a thin, steered tally. Raise min_votes + keep OI floor 0 → force void. This **is** the D21 game, with an admin button. |
   | `PAYOUT_HOLD_THRESHOLD_MICRO` / sweep ppm | `i64::MAX` hold default already disables review. Ops can skip review on a fat manipulated pot, or drop hold to $1 and lock everyone’s capital for `sweep_delay_secs`. |
   | `DISCOUNT_FLIP_WINDOW_SECS` / `MIN_FEE_BPS` | Set 0 / 0 and wash is cheap again. |
   | `MAX_VOTES_PER_WINDOW` | Raise → brigade; lower → silence opponents. |
   | `hidden_window_secs` | If it rewrites **live** `tally_hidden_at`, ops can reopen the book while tallies are hidden (exact D22 failure) or extend freeze after friends flattened. |
   | `trading_paused` | See findings 2 and 5. |

   D26 is an env bearer map and a route table that is not written down. E2E only proves finance ⇏ unwind. Curator vs ops vs finance vs superadmin on **fee, seed, integrity, pause, replay, refanout** is unspecified. Audit is after-the-fact; there is no max-delta, no cool-down, no second key.

   **Fix:** In D24 / `docs/copy/ops.md` / 0008 seed, classify every key:

   - **Bounds** at write (not at read): e.g. fee `∈ [min_fee_bps, 200]` and ≥ published min 10 bp; seeds ≥ Phase 5 `seed_floor_micro`; hidden window `∈ [60, 900]` and `< open_secs`; `min_votes ≥ min_votes_floor`; ppm fields already have constructors — reuse them.
   - **Apply-to** class: `live-immutable` (hidden window, min_votes, OI floor, hold threshold **for markets already Live/Closing** — stored on the market row at go-live, config only affects the next seed/publish); `next-trade` (fee, caps, flip window — pin the snapshot **version** at `serialize_key` so preview/execute can disagree only with an explicit `config_version` on the preview); `immediate-global` (pauses, feature flags).
   - **Role matrix** in the route table: curator = drafts/flags/resolve-void inbox (already); ops = pause trading, replay job, refanout, read invariants; finance = fee/seed/caps/hold **proposals**; superadmin = voting pause + unwind confirm. Fee/seed/integrity/min_votes = finance+superadmin dual-write or two-phase.
   - **Max delta / rate:** e.g. fee move ≤ 20 bp per 5 min; seed ≤ 2× prior.
   - E2E: out-of-range 422; ops cannot flip fee; fee change does not rewrite a live pool’s stored fee unless `next-trade` is chosen and then a preview taken **after** the fold matches execute; changing `hidden_window_secs` does not move `tally_hidden_at` on a Live market.

5. **[MAJOR]** Mid-live pause traps positions and creates an information-asymmetric close the D22 schedule was designed to avoid.
   **Plan section:** D25; D22; Phase 3 leftover “mid-market pause is Phase 6.”
   **Failure/abuse:** D22 freeze is **scheduled, public, both sides, voting still open**. A surprise `trading_paused` is none of those. Sequence: friend flattens (or never entered) → ops pauses → everyone else is stuck through hidden window into resolution. That is informed dumping **by the house**, not by a sniper. Propagation target <2s: in-flight `PlaceTrade`s that already passed the snapshot check still fill; everyone after 423s. The operator who wrote the key knows the fold is coming; the tape does not, unless a public frame exists (it is not in the plan). Pausing **inside** the hidden window is a no-op on trades (already `TradingFrozen`) but, with finding 2, a vote freeze. Unpausing at `closes_at − ε` creates a private last-look auction for whoever is retrying.

   **Fix:** Emit `TradingPaused` / `TradingResumed` on the public market WS in the **same** config transaction (existing outbox). Pin pause-check to the snapshot version captured at `serialize_key` (Preview has no `serialize_key` today — use a lock-free read of that same version; do not pretend preview takes the advisory lock). Disallow unpause inside `(tally_hidden_at, closes_at]` (resume only after resolve/void, or not at all). Require reason + ops role; global pause pages like an SLO. Document in `docs/copy/ops.md`: pause is not D22 and is not a fairness window.

6. **[MAJOR]** 100 agents / 5 min is theater against D19 SLOs and does not close the paths that break first.
   **Plan section:** §0; §4.1–4.2; D19; spec §9 gates.
   **Failure/abuse:** D19 / spec §9: smoke every merge; release gate p95 trade < 300 ms, p99 resolve-to-paid < 1 s, WS < 100 ms. Plan: **measure** p50/p95, **enforce** in Phase 7; smoke is `--agents 100 --duration 300s --seed 42`; full 2_000 is nightly. Default seed windows (`crates/main/src/seed.rs`) are **2 h open / 10 min hidden** unless `FLASH_CLOSES_SECS` is set. Content-tier defaults are 24 h / 60 min hidden (daily) and 1 h / 5 min hidden (flash). A 300 s run against those seeds never reaches `tally_hidden_at`, never convoys the close, never settles, never pays. Invariant-between-cycles is then “one live book still open.” Replay determinism is **dry-run action logs**, not a live failing run (spec §9: seed + append-only log replays the bug). HTTP retries, WS drops, and `market_view` (server `now`, remaining window) re-enter wall time — D28’s “no wall-clock in decisions” is true only if `market_view` is also recorded and replayed, which live HTTP does not give you.

   What actually breaks first (none of these are the smoke shape):

   1. **CPMM / trade path:** `PlaceTrade` serializes on `market_for_update` + `pool_for_update`. The honest worst case is already in spec §9: everyone piles the same flash book in the last seconds. 2_000 tasks × one row lock. p95 300 ms dies on lock wait, not on `quote_buy` math. Independent Poisson over 300 s on a 2 h book never makes that convoy.
   2. **Payout path:** Phase 3 already budgets ~2_000 voters in one settlement tx (canonical user locks → bulk scores/reps) and treats **1_000 voters < 2 s** as a hard Pg fixture. That **already exceeds** p99 resolve-to-paid < 1 s. Add `sweep_delay_secs` (default 180) whenever escrow ≥ hold threshold and D19’s 1 s number is a small-pot number only. Smoke with `MIN_VOTES_TO_RESOLVE=1` and hold default `i64::MAX` never sees hold **or** a fat settlement.
   3. **WS / outbox:** `CHAOS_RELAY_DELAY_MS=1500` in §4.6 is already 15× the 100 ms WS SLO. Ordered-late is tested; SLO is not.
   4. **Invariant sweep** during the spike: `GET /admin/invariants` is a full-ledger aggregate. Calling it “per cycle” on a 2_000-agent close is a self-DoS that can move p95 more than the trades.

   Chaos §4.5–4.6 runs **after** smoke, sequentially, on one market. It does not run **under** the close-spike. Kill-mid-payout with 3 voters is not kill-mid-payout with 2_000 row locks.

   **Fix:** Pin smoke env in `scripts/e2e_swarm_smoke.sh`: `FLASH_CLOSES_SECS=180`, `FLASH_TALLY_HIDDEN_SECS=60`, published scoring defaults, `MIN_VOTES_TO_RESOLVE` and `OI_FLOOR_MICRO` at product floors, hold threshold low enough that the seeded pot **does** enter `Resolving`. Require the 300 s profile to observe Live → Closing → Closed → (Resolving|) → Paid/Voided. Add a `convoy` action mode: at `tally_hidden_at − 15s`, all trade-capable agents hit the same market (this is the CPMM-breaker). Report p95 trade, p99 close→paid **excluding** configured hold delay, p95 WS. Phase 6 may stay measurement-only **if** the script prints `SLO_ENFORCE=0` and the numbers are stored; do not claim D19. Nightly 2_000 must include one 1k-voter settlement and fail the night if p95 trade > 300 ms. Live replay: persist `(seed, agent, tick, market_view_hash, action)` and allow `--replay-log` against a recorded view, or inject a test clock into `main` so a failing live run is actually reproducible.

7. **[MAJOR]** D29 chaos is process/WS/webhook-one-shot. Real ops fails on clock, disk, Postgres, and storms.
   **Plan section:** D29; §4.5–4.6; spec §9 chaos list.
   **Failure/abuse:** Plan seams: SIGKILL mid-payout, one duplicate converse webhook, `CHAOS_RELAY_DELAY_MS`, `CHAOS_WS_DROP_EVERY_N`. Spec also wanted onramp-webhook duplicates and indexer lag (honestly Phase 7 with rails). Still missing, and they are what staging actually sees:

   - **Clock skew / jump.** `AdvanceDue` compares `clock.now()` to stored `tally_hidden_at` / `closes_at`. An NTP step across `tally_hidden_at` skips the visible freeze (Live → Closed in one tick) or, backward, re-opens a window the book thought was dead. Swarm decisions use a tick clock; the server uses wall time — this is also the determinism hole in finding 6.
   - **Partial Postgres failure.** Restart / failover mid-`PlaceTrade` or mid-`ResolveMarket` (advisory locks drop with the xact). Connection-pool exhaustion from 2_000 agent tasks + invariant sweep + WS. WAL / disk-full on the data volume: writes 500, reads lie.
   - **Disk-full during render.** Phase 5 `render_dir` write_atomic for posters/share cards. ENOSPC mid-rename → job lease reclaim loops, or a truncated SVG served as a share card. Swarm that never hits `/users/{id}/share_card` will not see it; production will, on the first viral resolve.
   - **Webhook storms.** Converse dedupes `(channel, msg_id)` and single-flights a thread. One duplicate (§4.6) is the happy at-least-once case. Sendblue retry storms are **N** distinct `msg_id`s on one thread plus N agents (plan’s 5% converse slice). That is queue depth + LangGraph cost, not a unique-key hit.
   - **Config consumer death.** D24 missed-event safety is “version check on **load**.” A dead cursor leaves a stale snapshot: pause does not apply, fee flip does not apply, for the life of the process. No chaos asserts “kill the config consumer → next boot or version gap heals.”
   - **Simultaneous daily + flash close** (cadence machine). Two `resolve:` keys, two user-lock sets, one pool of connections.

   **Fix:** Extend D29 / §4 with a short, named list. Minimum Phase 6: (1) inject `Clock` jump `+hidden_window` and assert no trade fills after the jumped `tally_hidden_at`; (2) SIGKILL **Postgres clients** (or `pg_terminate_backend` on a `PlaceTrade` backend) and assert idempotent replay; (3) `chmod a-w` / fill a tiny `render_dir` tmpfs and assert job `failed` + no partial public asset; (4) 100 webhooks / 10 threads in 1 s, one pending per `(channel, msg_id)`, no double execute; (5) kill config consumer, flip pause, assert either 423 within heal-or-restart bound or a loud invariant/metric. Indexer/onramp storms stay Phase 7 with rails.

8. **[MAJOR]** Deferring **all** Playwright to Phase 7 is wrong for the  mobile timer; deferring **rails** is right.
   **Plan section:** §0 Out; spec §9 Playwright + sandbox rails; spec §13.7; `docs/research-notes.md`; PLAN.md phases 6–7.
   **Failure/abuse:** Spec §9 wants sandbox rails **and** a human Playwright path (signup → KYC → test-card → video → vote → trade → close → payout → push → withdraw). Spec §13 puts the **full** 2_000-agent SLO swarm in item 7 (money hardening). PLAN.md already split that: Phase 6 = API swarm + ops, Phase 7 = rails + compliance. Faucet `POST /admin/deposits` + `POST /users` as the current “real signup” is an honest stand-in for **ledger** load. Circle/onramp/indexer chaos cannot be real until those adapters exist — keep them in Phase 7.

   Playwright is not a rail. Phase 2 already shipped an installable PWA; Phase 5 shipped poster-first flash and share cards. Research notes (2026-08-12):  **iOS is in development**; “the mobile window is real but closing.” A 100-agent REST swarm will never see hidden-tally UX, vote+crowd-guess panel, PWA install, or mobile layout. That is the surface the incumbent is about to own. Phase 6 web work is **admin** pages + vitest only.

   Also: plan header cites “spec §13 roadmap item 6” — in `docs/spec.md` item 6 is the **conversation-graph** workstream. Swarm+ops is PLAN.md phase 6 / spec §§9–10 / spec item 7’s ops half. Small honesty bug; do not use it to claim spec-complete swarm.

   **Fix:** Keep rails, SLO **enforcement**, OTel→Grafana, and production continuous-checker in Phase 7. Pull a **thin** Playwright smoke into 6.5 / W4: mobile viewport, existing demo token, open live market, vote + guess, preview+trade, wait through the pinned short window, assert resolve/payout + share-card `<img>` (not inline SVG). No KYC, no card, no withdraw. State in §0: “API swarm is not mobile parity; PWA golden path is in the merge gate because the iOS window is closing.” Converse slice can stay 5% (iMessage path); do not advertise it as the spec’s 10% **rails** slice.

9. **[MAJOR]** Young-account + phone + device posture will either disable D21 in the swarm or void every smoke market — the plan never chooses.
   **Plan section:** D28 stand-ins; scoring.md integrity bar; `VoteIntegrityConfig::default`.
   **Failure/abuse:** Defaults: linked `imessage` required, 30 votes / 3600 s, accounts < 72 h cannot vote in the last 600 s. Swarm: `POST /users` now, act for 300 s. If smoke markets are shortened to actually close (finding 6), the **entire** market is near-close → **no** new agent can vote → D21 void (especially with `OI_FLOOR_MICRO=0`). If they set `VOTE_MIN_ACCOUNT_AGE_SECS=0` like `e2e_economy.sh`, the rule is off and the swarm does not stress it. Device HMAC / trusted-proxy IP: unless the client sends `x-device-id` and a peer address, Phase 3 coverage skips (`min_metadata_coverage_ppm`) and converse votes already carry null metadata. Phone link: they will attach channels so votes work — that is the **minimum** bar, not a sybil cost (numbers are synthetic).

   **Fix:** State the swarm identity policy. Suggested: 80% agents backdated `created_at = now − 80d` with unique channels + unique device ids (honest flow); 20% young + shared device (D21 + sweep). Never set `VOTE_MIN_ACCOUNT_AGE_SECS=0` in the profile that claims to test integrity. Document that synthetic phones do not satisfy “verified OTP” (still deferred) — do not claim D21 launch-complete because the swarm voted.

10. **[MINOR]** Config-propagation race vs the corridor is only half-specified.
    **Plan section:** D24–D25; review protocol prompt.
    **Failure/abuse:** Pause/fee check “after `serialize_key`, before economics.” `serialize_key` is the **idempotency** advisory lock, not the market row. Preview never takes it. A fold can land between key lock and `market_for_update`, or between preview and place. Conservation holds; users still see a 1% quote and pay 0% or 10%, or get 423 after a green preview (converse lexical confirm then dies).
    **Fix:** Stamp `config_version` on `TradePreview`. `PlaceTrade` rejects (409 stale-config) if the snapshot version ≠ stamped, **or** re-quotes under the new snapshot and requires a new confirm. Pause: capture snapshot at the start of the guarded section and use that capture through commit.

11. **[MINOR]** Ban / shadow-limit / deposit-withdraw switches are in spec §10.2 and missing; deposit faucet is a new money door.
    **Plan section:** §0 In/Out; D26; spec §10.2 manual ops.
    **Failure/abuse:** Plan reserves deposit/withdraw flags for Phase 7 (correct) but adds `POST /admin/deposits` as the swarm faucet under “admin authority.” Role unspecified. An ops token that can faucet is an infinite mint; conservation still holds (external contra) but LP/PnL and leaderboards do not. Ban/shadow-limit is listed in spec 10.2 and absent here — acceptable deferral only if said.
    **Fix:** Faucet = finance/superadmin, amount cap per key, same audit row as everything else, never ops. One sentence Deferred: account ban/shadow-limit + deposit/withdraw switches ride Phase 7 rails. Do not give the smoke the production admin token.

12. **[MINOR]** `docs/copy/ops.md` must publish pause ≠ D22 and unwind ≠ void, same honesty standard as scoring.md.
    **Plan section:** §2 inventory; scoring.md / curation.md posture.
    **Failure/abuse:** Published rules are the product’s fairness claim (scoring formula, anti-churn, hidden-window freeze, void = 50¢, heuristics ≠ guilt). An unpublished pause/unwind is a shadow rule the tape cannot price. Curation copy already refuses to invent markets; ops copy should refuse to invent silent clawbacks.
    **Fix:** Before 6.5, ops.md tables: every key, bounds, apply-to class, role, and a one-line “what a user sees.” Hidden-window paragraph must match scoring.md (“all trading freezes, voting continues”). Unwind paragraph must say “fraud clawback, not a refund button, may become a receivable.”

## What is sound (not a pass)

- Hexagonal simswarm (`Client` trait, Fake+HTTP contract, ChaCha8 per agent) matches standing crate discipline.
- Config write + audit + outbox in one tx, lock-free reads, is the right shape versus the trade corridor — **if** findings 2–4 land.
- Keeping LP class-3 seed breaker and putting operator pause **above** it is the Phase 3 promised leftover.
- Invariants as a use case (D27) and kill-mid-payout proving existing `resolve:<id>` idempotency are the right chaos philosophy.
- Rails/OTel-export/SLO-enforcement in Phase 7 matches PLAN.md and spec §13.7; only Playwright-mobile should move up (finding 8).
- 100% line on `simswarm` lib + no coverage-allowlist exclusions is consistent with the ratchet.

## Build read

Do not start W1–W4 until plan text amends **1–4** (rings + published defaults + aged accounts; vote-pause split; D30 receivables + dual-control; per-key bounds/apply-to/roles). Land **5–8** in the same amend so 6.5’s smoke is a real close+payout+convoy, chaos includes clock/disk/PG, and a thin PWA Playwright is in the gate. Task 6.0 (migration + stubs + freeze) can proceed **after** 0008 seed rows list the bounded key catalog from finding 4 — not before.
