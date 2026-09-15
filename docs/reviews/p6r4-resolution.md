# Phase 6 plan — review round 4 resolution map

**grok-p6r4: APPROVED** — every r3 closer verified against shipped code (`fee_policy.rs` effective_fee, `integrity.rs` verdict thresholds, `seed.rs` window semantics, `TxnKind`); zero new findings meeting the bar. Non-blocking notes honored: watermark-409 is a conservative extra re-preview (hot history sized so §3.3's one-generation-later place still hits); §3.7(b) "withdrawn-seller" implemented as cash-already-spent fixture, never a `POST /withdrawals`; converse payload growth is W1-owned plan-text.

**codex-p6r4: BLOCKED on four narrow items — all applied to the plan same-day (revision 4.1):**

| Finding | Disposition |
|---|---|
| NEW-1 (discount validator rejects its own seed) | → D24: entry-vs-floor constraint deleted; `min_fee_bps` floors the computed effective fee per tier (the shipped `effective_fee` formula); discount entries get their own bounds ([0, base], monotone, Δ≤10bp/5min); published seed validates. Grok r4 independently mandated the same reading. |
| NEW-2 (write-off = single-principal transfer) | → D30: write-off carries the full unwind-grade protocol (immutable proposal, distinct token ids, ≥T, reason, two audits, idempotent confirm, per-write-off + daily caps); added to D26's dual-control list; fake+Pg + RBAC e2e prove one principal cannot forgive. |
| r1#13/NEW-6 carryover (converse carrier unowned) | → §2 W1: `services/converse/src/converse/{graph.py,core_client.py}` + tests owned (pending retains config_version; execute sends expected_config_version; 409 ⇒ expire pending, new preview, new lexical yes; `api_models.py` regenerated). Deposit path named as `application/src/credit_deposit.rs` (W2); fake ownership transfer 6.0a-placeholders → W2-implementations made explicit. |
| r1#3 carryover (catalog attribute gaps) | → D24: every key now carries bounds + apply-to + role + max-delta (rep thresholds Δ≤10%, min-pot Δ≤2×, hold Δ≤2×, seed budget absolute [10^6,10^11], sweep Δ≤2×, cadence one-step, flags n/a-boolean, faucet caps Δ≤2×). |

**Gate status:** grok APPROVED + codex's four fixes applied ⇒ Task 6.0a/6.0b proceed now (both reviewers' build reads allow it); W1–W4 dispatch after a scoped codex delta-verification (round 5, limited to the four fixes) returns APPROVED.

**Round 5 (scoped codex delta, `codex-p6r5.md`):** 3/4 RESOLVED; the single remaining item — D26's dual-control endpoint list omitting receivable write-off (the protocol itself was already in D30) — was a one-sentence omission, fixed immediately (write-off added to the D26 list). With grok-p6r4 APPROVED and every codex-p6r5 item closed, **the Phase 6 plan review cycle is complete: waves W1–W4 are cleared to dispatch.**
