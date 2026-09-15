# Grok — Phase 2 plan Round 2 VERIFY

Read `docs/reviews/p2r1-resolution.md` and re-read the amended `docs/plans/phase2-core-loop.md` (no VCS). Checked remaining leak paths against D22 “hidden tallies and vote counts” and D21 phone uniqueness.

## R1 FINDING VERIFICATION (mine)

| ID | Status | One line |
|---|---|---|
| **B1** phone uniqueness / D21 claim | **VERIFIED** | CastVote rule 0 `user_has_channel` → `PhoneVerificationRequired` 403; uniqueness structural via `unique(channel,address)`; goal text still claims the min bar with fingerprint deferred honestly. Residual: linkage ≠ OTP verification (see m1). |
| **B2** tally suppression | **VERIFIED** | `tally` frames only while `Live && now < tally_hidden_at`; snapshot null in Closing; REST detail same rule; boundary tests both sides; chart/tape are trade-derived (frozen during D22 — no new price moves from trades). Residual: vote **receipt `seq`** still reveals running total to casters during freeze (m2) — not a public WS/REST tally feed. |
| **Velocity semantics** | **VERIFIED** | Global per-user window = Phase 2 control; per-market 1-vote structural; market-level arrival anomaly deferred Phase 3 (stated); user advisory lock in fixed order + cross-market concurrent test. |
| **Snapshot / reconnect / server_now** | **VERIFIED** | Subscribe → immediate snapshot with `server_now` + state + prices + conditional tally; reconnect = resubscribe + snapshot; countdowns from server_now + monotonic delta. |
| **At-least-once relay** | **VERIFIED** | Stated; frames carry outbox `seq`; client dedupe `(seq, type)`; zero-receiver = success; double-broadcast test with client dedupe. |
| **E2E deadline-polling** | **VERIFIED** | No fixed sleeps; poll with `boundary+10s` deadlines; cascade in one tick; payout latency from `lifecycle: closed` frame; `min_votes_to_resolve=1`; `ws_probe.mjs`. |
| **Post-resolve UX** | **VERIFIED** | Detail page first-class Resolved/Voided screens (final %, redemptions, user payout / void reason). |

## CODEX FIXES (mechanism read)

| Item | Status | One line |
|---|---|---|
| **Event name mapping** | **VERIFIED** | Frames keyed to real emitters (`TradePlaced`, etc.). |
| **`lifecycle_commands` durable keys** | **VERIFIED** | Insert in same tx as transition; key-exists → receipt replay; failure rolls key back; concurrent + retry tests — fixes IllegalTransition loser. |
| **Curator `resolve_at_tally` / `void`** | **VERIFIED with residual** | Flag-only override (422 if unflagged); both clear `curator_flagged_at` in settlement commit; `FlagCuratorNeeded` atomic. Residual: `resolve_at_tally` is intentional trust in curator to bless a thin electorate — not a new automated exploit if admin is trusted (m3). |
| **Singleton deployment honesty** | **VERIFIED** | Process-local broadcast; SKIP LOCKED = overlapping-pump safety not multi-node fanout; NATS deferred. |
| **Cascade sweep + 423 freeze mapping** | **VERIFIED** | Overdue flash markets close in one tick; PlaceTrade Closing → `TradingFrozen` 423. |
| **Chart numeric/bucket/index** | **VERIFIED** | Async Result ports; numeric SQL; bucket 1..=86400; trades_market_time_idx. |
| **PWA pin + token honesty** | **VERIFIED** | Pinned create-next-app@15; SW registration + icons; localStorage demo token labeled DEV exposure. |

## NEW FINDINGS

[m1] docs/plans/phase2-core-loop.md §2.2 rule 0: `user_has_channel` accepts **any** channel link, not a verified phone OTP — CreateUser/`link_channel("imessage", …)` without a verification ceremony still satisfies the rule; D21’s “verified” word is only partially met. FIX: document Phase 2 as “linked channel required; OTP verification deferred with fingerprint to Phase 3,” or require a `verified_at` on `user_channels` before vote.

[m2] docs/plans/phase2-core-loop.md §2.0/2.2: public vote receipt still returns **monotonic `seq`** during Closing — a sybil fleet casting through freeze learns the **running vote count** (not the YES/NO split) from successive `#N` values. FIX: either accept as  and state “hidden counts mean public UI/WS, not personal seq,” or return an opaque receipt id during freeze and reveal public seq only after `Closed`.

[m3] docs/plans/phase2-core-loop.md §2.1 `resolve_at_tally`: curator can settle a thin market at the live tally — correct product escape hatch, but it **re-opens the thin-electorate risk under a compromised admin**. FIX: one sentence in the plan: override is RBAC’d admin-only, audited, and is the deliberate trust boundary (not an automated path).

No new mechanism holes that invalidate freeze, relay, or scheduler design. Tally suppression on WS/REST detail/snapshot is complete for **public** vote-derived aggregates; only personal seq (m2) remains as a count side-channel.

## BUILD READ

| Slice | Ready? |
|---|---|
| 2.0 relay + WS | Yes |
| 2.1 scheduler + curator clear path | Yes |
| 2.2 CastVote integrity + freeze HTTP map | Yes |
| 2.3 chart/tape | Yes |
| 2.4 PWA | Yes |
| 2.5 e2e live-loop | Yes |

FINAL VERDICT: sound-to-build
