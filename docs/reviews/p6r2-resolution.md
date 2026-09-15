# Phase 6 plan — review round 2 resolution map

Plan revision 3 dispositions for codex-p6r2 (8 new BLOCKERs + 9 PARTIAL / 2 UNRESOLVED carryovers) and grok-p6r2 (3 BLOCKER / 5 MAJOR / 1 MINOR new + partial carryovers). All ACCEPTED; "→" = plan section carrying the fix.

## codex-p6r2

| Finding | Disposition |
|---|---|
| r1#1 PARTIAL (impossible Pg test) | → D24 read path: test rewritten — (a) writer B blocks behind A on the generation row; (b) missed wake-up heals from authoritative generation. |
| r1#3 PARTIAL (catalog incomplete) | → D24 catalog: every §10.2 key enumerated in plan text with bounds/class/role/max-delta (rep thresholds, velocity, integrity ppm, cadence, flags included; withdrawal keys stay Phase 7). |
| r1#4 UNRESOLVED / NEW-2 (saga audit) | → D26: single-tx mutations audit in-tx via AuditWrite; multi-tx `publish_now` = atomic (idempotent publication command + audit) executed by the existing saga; `AdminContext` Machine/Admin actor at shared boundaries; Phase 0–5 suites stay green on Machine paths; every call site in the §2 matrix. |
| r1#5 PARTIAL (faucet arm) | → §0: faucet mounts only under `OPINIONS_ENV=staging` AND `STAGING_FAUCET=1`. |
| r1#6/#8 + NEW-3 (receivables vs identities) | → D27/D30: receivables are a **non-cash facts subledger**, never a ledger account class; reversal books the shortfall to a house leg (cash identities untouched); identity 7 reconciles the subledger; legs enumerated full/partial/zero/collection/write-off. |
| r1#7 PARTIAL (atomicity boundary) | → D30: ONE transaction (Phase 3's 2k-voter settlement precedent), namespaced reversal keys, saga option dropped. |
| NEW-1 (fence convoy drain) | → D25: three fence namespaces (trading-global/trading-market/voting-market); CastVote takes only the voting fence; shared fences acquired after row locks at the write point (queued writers hold no fence); one-way lock graph (pause writers never take downstream locks); exclusive fences acquired BEFORE the generation row; 2,000-writer convoy contract test. |
| NEW-4 (determinism contract) | → D28: versioned run manifest + trace schema; PlannedOpportunity (dry-run, no network) vs DecidedAction (live trace) split; canonical encoding, counted RNG, ring vector ordering, retry classification, WS gap→snapshot records; replay rejects version mismatch; exit criterion 2 re-scoped. |
| NEW-5 (crash-point seam) | → D29 + §2: `ResolutionCrashPoint` application port, Noop prod impl, single call site inserted by 6.0a in `resolve_market.rs`; W4 owns the staging adapter; main arm pre-wired in 6.0b; contracts named. |
| NEW-6 (file matrix) | → §2: exhaustive per-file ownership incl. all shared/existing files; 6.0 split into 6.0a (schema/ports/models/gates/skeleton) and 6.0b (RBAC/middleware/main/legacy-token removal), each exit-checked; unlisted shared-file need stops the wave. |
| NEW-7 (proposal authority) | → D24: `config_change_proposals` table (namespaced key, patch hash, base generation, proposer/confirmer token ids, expiry, status); confirm locks proposal → fences → generation, revalidates, applies once; concurrency/replay/expiry contracts. |
| NEW-8 (fee preview contradiction) | → D25a: fee is new-market-only; **no reprice-market in Phase 6**; 409 only for this-market/user-relevant drift via the `config_changes` log; e2e asserts existing-book 200 after a global fee change. |

## grok-p6r2

| Finding | Disposition |
|---|---|
| f1 PARTIAL / N1 / N9 (ring efficacy theater) | → D28 pinned inequalities: sybil = aged ring k≥31 + one device + one /24 + last-60s burst vs ≤15 honest votes (device ≥60% AND burst — two signals); near-close proof on a separate 720s market; wash member staging-rep-seeded to tier 2 (assert 100bp not 90bp); `DEVICE_HASH_SECRET`/`TRUSTED_PROXY_CIDRS`/hold=$500 pinned in §3; inequalities in plan text so thresholds cannot be weakened to go green. |
| f2 PARTIAL (voting-pause carry + dual control + fence coupling) | → D25: voting pause auto-expires at `tally_hidden_at` (no carry into the hidden window); two-phase distinct principals; CastVote takes only the voting fence; e2e asserts auto-expiry re-enables votes. |
| f4 PARTIAL / N3 (reprice-market back door) | → D25a: `reprice-market` DELETED from Phase 6; live pool fee immutable; repricing deferred to Phase 7 with real auth. |
| N2 (remedial credit = refund button) | → D30: unwind-grade controls (dual control distinct tokens, ≥T, reason, two audits), per-market + daily house caps (422 over), self/linked-principal credits refused, honesty paragraph, e2e. |
| N4 (two-phase deadlock / one token both roles) | → D24: proposals expire (15 min), either principal rejects (audited), `confirmer_token_id ≠ proposer_token_id` schema-enforced (same-token 403 e2e), `config_changes.old` enables a dual-controlled one-step revert; pause-as-config-hatch forbidden in ops.md. |
| N5 (receivable farm) | → D30: open receivable ⇒ withdrawal 409 (b2 e2e); house-receivable cap + finance page; ops.md: unwind-from-Voided = wrong-fraud-decision only, never t0 cleanup. |
| N6 (convoy duration) | → §3: trade convoy from `tally_hidden_at − 60s`, asserted fraction of 2xx-before-hidden, p95 on 2xx with in-window 423s separate; nightly vote convoy at close + 1k-voter settlement kept as the second worst case. |
| N7 (hidden-secs offset trap) | → §3: `FLASH_CLOSES_SECS=180` + `FLASH_TALLY_HIDDEN_SECS=120` pinned with the offset semantics stated in the script and plan. |
| N8 (409 breaks lexical confirm) | → D25a: market/user-relevant staleness only; converse 409 contract (expire pending, new preview, new yes, never auto-execute, never loop); flip-window churn does not 409 non-flip buys (e2e). |
| Sanity table (hold unnamed, OI floor, RepConfig seeds) | → §3 env pins: hold=$500 named, OI floors split across suppress (low) and pad (positive) markets, published scoring seeds mandatory. |

## Cross-review merges
- codex NEW-1 + grok f2: one fence redesign (namespaces + drain-safety + voting isolation + expiry).
- codex NEW-8 + grok N3/N8: one fee policy (new-market-only, no repricing, market-relevant staleness, converse contract).
- codex NEW-3 + grok N5: one receivables model (house-leg cash + non-cash facts + withdrawal guard + caps).
