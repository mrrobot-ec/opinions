# Phase 2 plan — round 2 resolution

- **grok: FINAL VERDICT sound-to-build** ([grok-p2r2.md](grok-p2r2.md)); its residuals applied: vote-receipt `seq` suppressed during the hidden window (a late receipt's "#N" would disclose the hidden vote count), OTP possession-proof named as the Phase 3 deferral, curator wording tightened.
- **codex: fix-first** ([codex-p2r2.md](codex-p2r2.md)) — 12 of 15 verified; the five remaining items applied:

| P2R2 finding | Resolution |
|---|---|
| B1 — user advisory lock had no port role and shared a collision namespace with idempotency locks | `UserLockGuard::lock_user` role on `VoteTx`, called replay-lookup → user → market (fixed order); advisory locks move to the two-key namespaced form (class 1 idempotency / class 2 user); deadlock contract test |
| B2 — Tasks 2.0–2.3 overlap on main/routes/ports/fakes — unsafe parallel without VCS | Topology re-sequenced: one worker owns the Rust chain 2.0→2.1→2.2→2.3; only `web/` (2.4) runs parallel |
| B3 — WS frames lacked declared query/projection sources; ambiguous `seq`; countdown-less snapshot | `MarketQueries::market_snapshot` port (shared by WS + REST detail); outbox payloads enriched at the source (TradePlaced carries handle/side/action/amount/trade_seq/created_at; MarketResolved carries redemptions); `outbox_seq` vs `trade_seq` named distinctly; snapshot/detail ship `closes_at` + `tally_hidden_at` |
| B4 — settlement paid ledger only; positions never showed payouts | Settlement now projects into positions in the same transaction (`realized_pnl += payout − cost`, zero shares/cost), idempotent under the resolve key; winner/loser/replay/conservation tests; e2e assertion has a real source. Curator flagging got an atomic winner predicate (`WHERE curator_flagged_at IS NULL RETURNING`) |
| M1 — scaffold could prompt and **git-init** `web/`; probe had no `ws` dep | `create-next-app@15 --yes --disable-git` (mandatory in a no-VCS repo), unsupported flag dropped, exact pnpm via corepack, `scripts/package.json` pins `ws` for the probe |
| grok-fix regressions — `user_has_channel` channel-agnostic; snapshot missing deadlines | Port takes the channel type (`"imessage"`); deadlines shipped in snapshot/detail (B3) |

Build topology: grok → 2.4 (web, parallel); codex → bounded addendum on these six deltas, then the Rust chain 2.0→2.3; exit check 2.5 last.
