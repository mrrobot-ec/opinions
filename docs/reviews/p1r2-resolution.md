# Phase 1 plan — round 2 resolution

- **grok: FINAL VERDICT sound-to-build** ([grok-p1r2.md](grok-p1r2.md)) — all its R1 items verified; residuals applied same-day: `IdempotencyGuard` added to `VoteTx`/`DepositTx`, Task 1.5 owns the gate-consume rewiring (in scope by design).
- **codex: fix-first** ([codex-p1r2.md](codex-p1r2.md)) — 5 of its fixes + all grok engineering fixes verified; the amendment wave itself introduced issues, all applied now:

| P1R2 finding | Resolution |
|---|---|
| B1/M2 — contract factory signature uncompilable (`'_` in Future bound, E0637) | Suites are generic over `S: Store + ?Sized`, opening transactions through the store; concurrency suites open two txs from one store; role helpers take `&mut (dyn Role + '_)` |
| B2 — rules contradicted guard-first; impossible aborted-tx recovery retained; CastVote missing in-lock cutoff | Fixed sequence in PlaceTrade rule 1 and CastVote (serialize_key → replay-check → market_for_update → clock-under-lock); `DuplicateKey` demoted to invariant-violation error; `market()` naming corrected |
| B3 — bootstrap unimplementable (no bootstrap_tx, no pool owner/account, contradictory lifecycle) | `Store::bootstrap_tx`, `OwnerRef::MarketPool`, `MarketWriter::create_pool` (pools row + pool ledger account), SeedMarket leaves exactly `Scheduled`, seed.rs advances via `AdvanceMarket(GoLive)` |
| B4 — generic AdvanceMarket could bypass settlement | Event whitelist (`Approve, GoLive, EnterCloseWindow, Close, StartIntegritySweep`); financial events → `AppError::UseResolveMarket` |
| M1 — NULL-owner rows dodge the owned-account unique index | `ledger_accounts_owner_shape` CHECK (owned classes require owner_id; singletons/external forbid it) in 0003 |
| M4 — D21 extension ADR not recorded in decisions | ADR appended to D21 in docs/decisions.md (extension → Phase 5 curation; Phase 1 curator path = admin resolve/void) |
| M6 — generator pinned too late, bare invocation, no CI sync | Pin + `uv lock` move into Task 1.4; scripts invoke via `uv run --project`; CI installs uv + syncs before the two-artifact freshness diff |
| grok-verification #4 — coverage gates not wired into the pipeline | `just coverage` grows per-crate gates in 1.1 (application) and 1.3 (adapters); CI runs it with `DATABASE_URL` |

Applied fixes go to codex as a bounded verification addendum; build starts in parallel on the stabilized port design (Task 1.0 already green on main).
