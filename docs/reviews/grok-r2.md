VERDICT: fix-first

Re-read `docs/reviews/r1-resolution.md` and `git diff HEAD~1` on PLAN.md, docs/spec.md, docs/decisions.md, docs/er-diagram.mermaid, docs/conversation-graph.mermaid. Verified each R1 fix in my domain plus codex External/settlement as mechanism-adjacent. Pure domain Tasks 0–6 are unblocked; remaining gaps are (a) incomplete landings that would bake wrong shapes into Task 7–8, and (b) new mechanism holes introduced by the R1 fixes themselves.

## R1 FIX VERIFICATION:

- **grok B1 — lexical pre-router confirm:** **INCOMPLETE** — Correct and airtight in `conversation-graph.mermaid`, D16, and spec §7.5 design rules (raw text → allowlist before any LLM; anything else auto-cancels). **But PLAN Task 8 Step 3 still specifies `load_session → router → …` with no pending-gate node**, while only the Task 8 intro claims the gate is in the skeleton; `Intent.CONFIRM`/`CANCEL` remain on the router enum, inviting a re-wire of LLM confirm. Gate tests are named but the graph topology in the executable plan does not match the locked design.
- **grok B2 — vote corridor:** **VERIFIED** — Votes are preview → `pending_action(kind=vote)` → lexical confirm → `POST /votes` with run_id chaining in graph + §7.5 + DDL kind check. Agent path no longer one-shots the oracle.
- **grok B3 — integrity bar timing:** **VERIFIED** — D21 + §3.4 + §13 Phase 2 / PLAN Phase 2 pull min bar (phone, device, velocity, young-account friction, `min_votes_to_resolve` void) before real-money resolution; full sybil stays Phase 3.
- **grok B4 — bag-defense framing:** **INCOMPLETE** — §3.4 correctly states eligibility stack is the bag-defense and 75/25 is not. **§3.2 still claims accuracy weighting “defuses the vote-to-trade coupling”** — direct contradiction that will ship into product copy if not scrubbed.
- **grok M1 — buy-freeze window (D22):** **VERIFIED as specified** — buys frozen / sells open during hidden tallies is consistent in §3.4, D22, ER `tally_hidden_at` note. Residual sell-side adverse selection is a *new* finding (below), not a missed landing of the stated fix.
- **grok M2 — LP kill-switch (D23):** **VERIFIED** — §2.3 + D23: expected-loss model, dynamic seed, auto kill-switch as launch control.
- **grok M3 — causal chain in ER:** **INCOMPLETE** — `agent_runs`/`agent_steps`/`pending_actions` present; trades have `run_id` + `pending_action_id`. **Votes have `run_id` only — no `pending_action_id`**, despite ER relationship `PENDING_ACTIONS |o--o| VOTES` and the vote corridor now producing a pending row; chain is asymmetric vs trades.
- **grok M4 — event-sourcing language:** **INCOMPLETE** — §5.1 body + D9 correctly say state-primary + transactional outbox. **Residuals remain:** spec header line 6 (“event-sourced core”), §5.5 patterns (“event sourcing with the outbox pattern”), §9 swarm paragraph, README one-paragraph system blurb.
- **grok M5 — dual currency:** **VERIFIED** — `usdc` + `usdc_credit`, `credit_grant`/`credit_convert` kinds in ER/PLAN/spec §5.6; counsel spike weeks 1–2 called out.
- **grok M6 — MSB counsel:** **VERIFIED** — §12 + open items: money-transmitter/custody as day-zero track; mainnet custody provisional.
- **grok M7 — roadmap vs iOS/oracle:** **VERIFIED** — Phase 2 = min integrity + PWA; Phase 3 = full integrity/economy.
- **grok M8 — single-flight pending:** **VERIFIED** — per-`thread_id` lock, webhook dedupe `(channel, inbound_msg_id)`, non-pure-confirm cancels, param change ⇒ new preview (§7.5 + graph).
- **codex B1 — External contra account:** **VERIFIED** — `OwnerType::External` may go negative; internals non-negative; deposits/genesis as External→…; conservation `internal_total == -external` in PLAN Task 2 + D13 + ER.
- **codex M2 — settlement completeness:** **VERIFIED** — `settle_market` takes all holdings incl. pool; `HoldingsIncomplete`; dust `0 ≤ dust < holdings.len()`; escrow = minted_sets × 1e6.

## NEW FINDINGS:

[M1] docs/spec.md §3.4 + D21 `min_votes_to_resolve` void+refund: a losing whale (or colluding holders) can **suppress late votes / keep participation under threshold** to force void+full refund and escape a mark-to-market loss; winners can pad votes to force resolution — the void rule invents a new gameable binary outcome. FIX: define anti-void rules now (e.g. void only if participation < min *and* open interest below a floor; or no void once OI/volume exceeds X; or void requires curator ack above OI floor; publish that suppression/padding is anomaly-swept).

[M2] docs/spec.md §3.4 + D22 buy-freeze: freezing buys while sells stay open still gives **informed/manipulative sellers a last-look exit into the house LP** during the hidden window (they dump the losing side; uninformed cannot enter the other side). FIX: either freeze *all* trading in Closing, or auto-widen fee/impact during the window, or mark LP inventory at a frozen mid and halt LP fills — pick one and put it in D22.

[M3] docs/spec.md §7.5 lexical allowlist + graph: gate is fail-closed (good) but **normalization is unspecified** — `"yes!"`, `"Yes."`, `"do it please"`, emoji-only, and **iMessage tapbacks/reactions** (often empty or non-text payloads) will miss allowlists, auto-cancel a valid pending trade/vote, and re-route — silent user harm and support load, not a money leak. FIX: specify `normalize(text)` (lowercase, strip punctuation/whitespace, collapse spaces); on reaction/empty/near-miss respond with re-prompt “Reply yes or cancel” **without** clearing pending; only clear on explicit cancel or TTL.

[M4] PLAN.md Task 8 Step 3 + Intent enum: skeleton graph topology still **omits the pending gate** and keeps `Intent.CONFIRM`/`CANCEL` on the router — R1 B1 is not executable-plan-complete and can regress at implementation. FIX: Step 3 must be `load_session → pending_gate → (execute | cancel | router) → …`; drop CONFIRM/CANCEL from router Intent (or map them to no-ops that never execute); tests must prove router cannot reach EX.

[M5] docs/er-diagram.mermaid VOTES vs PENDING_ACTIONS: vote causal chain lacks `votes.pending_action_id` while trades have it — incident query “which preview became this vote?” is broken for the oracle path. FIX: add nullable `pending_action_id FK` on votes (mirror trades) in ER + Task 7 DDL.

[m1] docs/spec.md §3.2 still says 75/25 “defuses” bag voting — contradicts honest §3.4/B4 fix. FIX: rewrite §3.2 to “rewards crowd-reading; bag-defense is the eligibility stack (§3.4).”

[m2] Residual “event-sourced” language in spec header, §5.5, §9, and README — will mis-steer Phase 1 application design. FIX: global replace to “state-primary + transactional outbox” in those four sites.

[m3] Checks out for Phase 0 money core: External account + settlement-with-pool + MAX_RESERVE overflow bounds + integer bps scores/rep + fee-on-proceeds + smaller-root sell math remain coherent; Tasks 0–6 can start once doc nits above are queued — **do not freeze Task 7–8 or integrity void/freeze semantics without M1–M5.**

## BUILD-START READ:

| Slice | Ready? |
|---|---|
| Tasks 0–6 (domain math) | Yes — sound |
| Task 7 (schema v1) | After M5 (+ dual-currency already OK) |
| Task 8 (converse skeleton) | After M3–M4 |
| Phase 1+ money/oracle | After M1–M2 mechanism lock |
