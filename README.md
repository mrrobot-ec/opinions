# Opinions — real-money opinion market, built to win

Real-money opinion markets: YES/NO shares trade in cents against an AMM and redeem at the **final vote percentage** — you're not betting on what's true, you're betting on what the crowd believes. Built for speed, voter economy, trust & polish, video at scale, and mobile — with a unique distribution surface: trading over iMessage.

**📚 [Read the full documentation](https://mrrobot-ec.github.io/opinions/)** — a plain-language guide for learning the whole system from zero.

## Documents

| Doc | What it holds |
|---|---|
| [docs/spec.md](docs/spec.md) | **The master spec** — competitive teardown, full mechanism math, voter economy, architecture, engineering standards, distribution layer, agent observability, swarm testing, ops control plane, growth, compliance, build plan (§1–§13) |
| [docs/decisions.md](docs/decisions.md) | Locked decision log (D1–D20) with rationale + open items |
| [docs/research-notes.md](docs/research-notes.md) | External verification (2026-08-12): market research, Sendblue, legal landscape, sources |
| [docs/er-diagram.mermaid](docs/er-diagram.mermaid) | Full ER model, column-level |
| [docs/conversation-graph.mermaid](docs/conversation-graph.mermaid) | The iMessage conversation graph (LangGraph) |
| [PLAN.md](PLAN.md) | Plan index — per-phase execution plans in docs/plans/ (Phases 0–7 ✅ built) |
| **[docs-site/](docs-site/index.md)** | **Learn the whole system from zero** — a plain-language MkDocs guide for people with no software background: how the market works, how the code is organised, how the money is kept honest, and step-by-step walkthroughs. Serve it with `mkdocs serve` (config: [mkdocs.yml](mkdocs.yml)) |

## The system in one paragraph

A **Rust monolith** (Axum/Tokio/SQLx/Postgres/Redis/NATS) holds the money: an append-only **double-entry ledger** in integer micro-USDC (per-currency balanced, cash + bonus credits), a CPMM over complete sets, a typestate market lifecycle (incl. voided + full-unwind refunds), and sub-second vote-share resolution — state-primary with a transactional-outbox event log, clean architecture with the dependency rule enforced in CI, 100% line coverage on stable + nightly branch coverage + ≥90% mutation kill on the money core. Solana is a USDC rail only (custodial ledger; chain touched at deposit/withdraw). A **Python FastAPI + LangGraph** conversation service fronts it over iMessage (Sendblue sandbox now): six pinned-model agents for language, a deliberately **agent-free money corridor** (preview → explicit confirm → idempotent execute), and every graph run recorded to `agent_runs`/`agent_steps` with rendered prompts and a causal chain from message to ledger transaction. A 2,000-agent swarm on real sandbox rails (devnet USDC, sandbox cards) is the release gate.

## Status

- ✅ Spec, decisions, research — complete (this repo)
- ✅ Phase 1 core write path — built and exit-checked; the text-to-trade e2e is green from webhook preview and lexical confirmation through the ledger-backed position
- ✅ Phase 2 core loop — built and exit-checked (2026-08-12): outbox→WS, lifecycle scheduler + auto-resolve, D21 integrity bar, chart/tape, installable PWA; live-loop e2e prints `LIVE LOOP GREEN` (`snapshot → price+trade → lifecycle closing/closed/resolved`, Closing trades → 423, payout cascade ≤2s, escrow drained, vote_scores present); line coverage 100% on `domain` / `application` / `adapters`
- ✅ Phase 3 economy & integrity — built and exit-checked (2026-08-12): rep EWMA + tier fee/caps, integrity payout hold + two-signal flag + curator inbox, realization-fact leaderboards, LP kill-switch, web economy surfaces + published scoring copy; `scripts/e2e_economy.sh` prints `PHASE 3 E2E GREEN` (sections A–D); coverage 100% × 3 (domain 1,326 / application 6,639 / adapters 2,491 lines)
- ✅ Phase 4 social & notifications — built and exit-checked (2026-08-12): threaded comments + votes + brigade-braked reports, holders by committed capital, profiles, §7.1 notification fanout (second outbox consumer), WS user channel; web social surfaces + `docs/copy/notifications.md`; `scripts/e2e_social.sh` prints `PHASE 4 E2E GREEN` (sections 1–7); 301 Rust / 33 Python / 40 web tests; coverage 100% × 3 (domain 1,502 / application 8,858 / adapters 4,091 lines)
- ✅ Phase 5 content engine — built and exit-checked (2026-08-12): draft curation + reserved cadence slots, replay-safe publication saga, leased deterministic poster/video jobs, WS asset hot-swap + snapshot rehydration, inert real-PnL share cards, env-gated LLM draft/moderation adapters; `scripts/e2e_content.sh` prints `PHASE 5 E2E GREEN`; 455 Rust / 29 Python passed (+4 skipped) / 50 web tests; coverage 100% × 3 (domain 1,897 / application 13,368 / adapters 6,672 lines)
- ✅ Phase 6 simswarm, chaos & ops — built and exit-checked (2026-08-13): deterministic swarm/replay engine, live config fences and switches, leased replay/refanout plus continuous invariants, auditable RBAC/manual recovery, receivables, chaos seams, and mobile ops surfaces; `scripts/e2e_swarm_smoke.sh` prints `PHASE 6 E2E GREEN` (convoy 33/56 = 58.9%, lifecycle signals 1, fat-pot signals 3, exactly-once payout 1); 696 Rust / 36 Python / 58 web tests; coverage 100% × 4 (domain 1,897 / application 21,497 / adapters 8,826 / simswarm lib 2,990 lines)
- ✅ Phase 7 money hardening + compliance gate — built, integrated and exit-checked (2026-08-14): hold-first withdrawals with persisted-signed-bytes send lineage and a 2-of-3 archival non-landing predicate, deposit observation/admission machine (`DepositSuspense`) with source-locked refunds, bonus credits as grant lots converting on fees-paid-at-Paid against a segregated `BonusReserve`, KYC/sanctions/geo with a `Clear|Hit|Indeterminate` algebra, AML at-request + sweep, ban/shadow/self-exclusion with a no-stranded-funds egress split, dual control over one generic proposal authority, SLO gates on lib constants, signed reconciliation + durable incident outbox, live fee override; plan converged after **5 codex + 4 grok adversarial review rounds** (~70 findings); **1,027 Rust tests across 23 binaries**, clippy `-D warnings` clean, **100% line coverage, zero exclusions** (`domain` 1,934 / `application` 38,492 / `adapters` 14,111 / `simswarm` lib) gated on the lcov union
- ✅ Phase 8 learning site — [docs-site/](docs-site/index.md): 26 pages (~36k words, 28 mermaid diagrams) teaching the whole system to a reader with no software background, audited line-by-line against the source; `just docs`
- ⏳ External gates: counsel (real-money vote-resolved wagering; sweepstakes question), onramp + Apple MfB written approvals
- ⚠️ Clock: mobile market is competitive — the window is open but closing

## Ground rules for contributors (human or agent)

1. Money math is integer-only; no floats cross the ledger or AMM. Rounding favors the pool.
2. The dependency rule (domain ← application ← adapters ← main) is CI-enforced; domain has zero I/O.
3. Nothing merges below the coverage gates (spec §5.5): money core at 100% line coverage on stable CI, branch coverage reviewed nightly, ≥90% mutation kill.
4. LLMs never compute numbers, never move money, never send unguarded output (spec §7.5, D16).
5. Every schema/mechanism change updates docs/spec.md in the same PR.
