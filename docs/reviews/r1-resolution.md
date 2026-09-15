# Review round 1 — resolution map

Reviewers: codex (gpt-5.6-sol xhigh) and grok, both verdict **fix-first**. Full reports: [codex-r1.md](codex-r1.md), [grok-r1.md](grok-r1.md). Every finding below was applied by the coordinator in the commit tagged `r1-fixes`; reference = where the fix landed.

| Finding | Severity | Resolution |
|---|---|---|
| grok B1 — LLM router could authorize execution via `intent=confirm` | Blocker | Confirm/cancel is now a **pre-router lexical gate** on raw normalized text (allowlists); anything else auto-cancels + re-routes. LLM output can at most produce a preview. spec §7.5, conversation-graph.mermaid, D16, PLAN Task 8 |
| grok B2 — votes (the oracle) were LLM-extracted and executed without confirm | Blocker | Votes moved inside the guarded corridor: preview → `pending_action(kind=vote)` → lexical confirm → `POST /votes`, run_id-chained. spec §7.5, graph, pending_actions DDL |
| grok B3 — integrity stack arrived Phase 3, after money write paths | Blocker | **D21**: minimum integrity bar (phone uniqueness, device fingerprint, vote velocity, young-account friction) + `min_votes_to_resolve` void rule are Phase-1/2 launch-blocking; vote-oracle threat model is a launch artifact. spec §3.4, §13, PLAN roadmap |
| grok B4 — "75/25 defuses vote-your-bag" overclaimed | Blocker | Honest framing: vote-before-trade ordering + immutability + 1/N influence means the real attack is sybil-shaped; the eligibility stack (not scoring weights) is the bag-defense, published as such. spec §3.4 |
| codex B1 — universal non-negativity made deposits unrepresentable | Blocker | **`External` contra account class** (may go negative; equals net system inflow); internal accounts stay non-negative; genesis = explicit External→House txn; no test backdoor. PLAN Task 2, spec §5.6, D13 |
| codex B2 — branch-coverage CI gate doesn't exist on stable cargo-llvm-cov | Blocker | Stable gate = `--fail-under-lines 100`; branch coverage on scheduled nightly-toolchain job (reviewed); mutation kill-rate ≥90% via checked-in `scripts/mutation_gate.py`. PLAN Task 0, spec §5.5, D12 |
| codex M1 — u128/i128 overflow on valid i64 inputs | Major | `MAX_RESERVE = MAX_AMOUNT = 10^15` micro enforced (`InputTooLarge`); checked u128 products/discriminants proven to fit; bound-edge property tests. PLAN Task 3 |
| codex M2 — settlement omitted pool/LP inventory → fake dust | Major | `settle_market` takes **all** outstanding holdings incl. pool; `HoldingsIncomplete` completeness error; dust contract `0 ≤ dust < holdings.len()`. PLAN Task 6 |
| codex M3 — missing schema keys/checks | Major | `outcomes unique(market_id,idx)`; trades gained `market_id` + `unique(market_id,seq)`; non-negativity checks on pool_reserves/positions. PLAN Task 7, ER |
| codex M4 / grok m2 — vote seq allocation under concurrency | Major | Locked allocation via `markets.vote_seq_counter` update-returning in the vote txn + `idempotency_key` on casts; Phase-1 blocking. PLAN Task 7 |
| codex M5 — app-only sum-zero once a shared schema exists | Major | Deferred constraint trigger is a **blocking precondition for any Phase-1 write adapter**; stated in DDL comment + roadmap. PLAN Task 7/roadmap |
| codex M6 — CI/local tool parity + missing audit/mutation jobs | Major | CI installs just/cargo-llvm-cov/cargo-mutants pinned via taiki-e action; audit job added; `just ci` mirrors CI; justfile syntax fixed (codex m1). PLAN Task 0 |
| codex M7 — dust bound was loose/unprincipled | Major | Escrow defined as `minted_sets × 1e6`; exact claims sum to escrow; `0 ≤ dust < holdings.len()` enforced in-domain. PLAN Task 6 |
| grok M1 — hidden tallies + open book = free last-look | Major | **D22**: buys freeze during the hidden window, sells stay open. spec §3.4 |
| grok M2 — LP seed drained by adverse selection | Major | **D23**: expected-loss model pre-real-money, dynamic seed config, automatic LP kill-switch as launch control. spec §2.3 |
| grok M3 / codex m4 — agent tables absent from ER; causal chain unqueryable; growth unmanaged | Major | ER extended 1:1 (agent_runs/agent_steps/pending_actions); trades/votes gained `run_id`/`pending_action_id`; `unique(channel,inbound_msg_id)` dedupe; indexes; partition/retention/redaction policy. ER, PLAN Task 7, spec §10.3 |
| grok M4 — "event-sourced" terminology wrong | Major | Renamed: state-primary + transactional outbox; outbox rows retained = append-only event log for projections only. spec §5.1/§5.6, D9 |
| grok M5 — no dual-currency modeling before counsel fork | Major | `usdc` + `usdc_credit` currencies + `credit_grant`/`credit_convert` txn kinds from day one; counsel spike weeks 1–2. spec §5.6/§12, Task 7 |
| grok M6 — MSB/money-transmission underweighted | Major | Money-transmitter/custody counsel = own day-zero track; mainnet custody provisional until answered. spec §12, decisions open items |
| grok M7 — roadmap ordering vs iOS clock + oracle trust | Major | Phase 2 pulls in minimum integrity bar + PWA shell; full stack Phase 3. spec §13, PLAN roadmap |
| grok M8 — pending races/concurrent messages unspecified | Major | Single-flight per thread_id; webhook dedupe; non-pure-confirm cancels; param change ⇒ new preview. spec §7.5, PLAN Task 8 |
| grok m1 — numeric score/rep columns | Minor | Integer bps (`accuracy_bp/majority_bp/score_bp`) + `rep_micro` fixed-point. ER |
| codex m2 — rounding "1 micro" overclaim | Minor | "<1 micro per operation, ≤2 per fill" everywhere. spec §2.5, PLAN constraints |
| codex m3 — monotonic proptest underspecified | Minor | Ordered-distance two-guess form, majority held constant. PLAN Task 5 |
| codex ckpt 7 — seam should be provider-agnostic + async | — | Shared `AgentNode` async protocol for every agent node. PLAN Task 8 |

Explicitly kept after review: both-sides-win tie rule at exactly 50.00% (both reviewers endorsed; publish the definition), fee-on-proceeds sell model, smaller-quadratic-root sell math, `c ≥ no` drain guard, Phase-0 pure-domain-first scope.
