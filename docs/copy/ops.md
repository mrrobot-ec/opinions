# Operations controls

These controls are operational guardrails, not a way to rewrite a live market. Values are validated as one prospective snapshot; an invalid patch is rejected without a partial change. “Two-phase” means one authorized principal proposes and a different token id confirms.

## Configuration catalog

| Key | Published value | Bounds | Applies to | Writer | Maximum change | User-visible meaning |
|---|---:|---|---|---|---|---|
| `trade_fee_bps` | `100` | 10–200 bp | New markets only | Finance, two-phase | 20 bp per apply | Fee stamped on newly created pools; open books never reprice. |
| `min_fee_bps` | `10` | 0–50 bp | Next trade | Finance, two-phase | 10 bp per apply | Floor under the effective fee after a tier discount. |
| `discount_flip_window_secs` | `3600` | 600–86,400 s | Next trade | Finance, two-phase | Bounded value | Window in which a side flip pays the base fee. |
| `fee_discount_bp_by_tier` | `[0,0,10,20,30]` | Five monotone entries, each 0–`trade_fee_bps` | Next trade | Finance, two-phase | 10 bp per entry per apply | Reputation-tier discount; the effective fee still respects `min_fee_bps`. |
| `position_cap_micro_by_tier` | `[25000000,50000000,100000000,250000000,500000000]` | Five monotone entries, never below the published table | Next trade | Finance, two-phase | Published floors | Maximum committed position by reputation tier. |
| `rep_tier_thresholds_micro` | `[0,200000,400000,600000,800000]` | Five monotone entries | Next trade | Finance, two-phase | 10% per entry per proposal | Reputation needed to enter each tier. |
| `rep_score_min_pot_micro` | `50000000` | 0–1,000,000,000 micro | Next trade/scoring | Finance, two-phase | At most 2× or ½× prior | Minimum market pot that can affect reputation. |
| `seed_micro_daily` | `500000000` | Positive and at least the published daily tier floor | New markets only | Finance, two-phase | At most 2× or ½× prior | Complete-set seed for a new daily market. |
| `seed_micro_flash` | `100000000` | Positive and at least the published flash tier floor | New markets only | Finance, two-phase | At most 2× or ½× prior | Complete-set seed for a new flash market. |
| `daily_seed_budget_micro` | `1000000000` | 1,000,000–100,000,000,000 micro | New markets only | Finance, two-phase | At most 2× or ½× prior per day | Total house seed admitted each day. |
| `hidden_window_secs` | `300` | 60–900 s and less than the market open time | Live-immutable stamp | Finance, two-phase | Bounded value | Time before close when the tally becomes hidden; an existing market’s timestamp never moves. |
| `min_votes_to_resolve_floor` | `3` | At least 3 | Live-immutable stamp | Finance, two-phase | May not cross the published floor | Minimum participation stamped on new markets. |
| `oi_floor_micro` | `0` | 0–1,000,000,000 micro | Live-immutable stamp | Finance, two-phase | Bounded value | Open-interest threshold separating low-participation void from curator review. |
| `payout_hold_threshold_micro` | `500000000` | 1,000,000–10,000,000,000 micro | Live-immutable stamp | Finance, two-phase | At most 2× or ½× prior | Pot size at which an integrity flag holds payout for review. |
| `sweep_delay_secs` | `180` | 60–3,600 s | Immediate global | Ops, direct | At most 2× or ½× prior | Delay before the integrity sweep runs after close. |
| `max_votes_per_window` | `30` | 15–45 | Immediate global | Ops, two-phase | Fixed ±50% band around the published value | Vote-velocity threshold. |
| `vote_window_secs` | `3600` | 1,800–5,400 s | Immediate global | Ops, two-phase | Fixed ±50% band around the published value | Window used by the vote-velocity threshold. |
| `integrity_burst_multiplier_ppm` | `2000000` | 1,600,000–2,400,000 ppm | Immediate global | Superadmin, two-phase | Fixed ±20% band around shipped default | Flags a close-window vote burst relative to prior windows. |
| `integrity_young_share_max_ppm` | `500000` | 400,000–600,000 ppm | Immediate global | Superadmin, two-phase | Fixed ±20% band around shipped default | Maximum young-account share before the signal fires. |
| `integrity_device_share_max_ppm` | `600000` | 480,000–720,000 ppm | Immediate global | Superadmin, two-phase | Fixed ±20% band around shipped default | Maximum shared-device share before the signal fires. |
| `integrity_subnet_share_max_ppm` | `600000` | 480,000–720,000 ppm | Immediate global | Superadmin, two-phase | Fixed ±20% band around shipped default | Maximum shared-subnet share before the signal fires. |
| `integrity_min_metadata_coverage_ppm` | `500000` | 400,000–600,000 ppm | Immediate global | Superadmin, two-phase | Fixed ±20% band around shipped default | Metadata coverage required before device/network signals are trusted. |
| `flash_cadence_secs` | `3600` | 900–86,400 s | Immediate global | Curator, direct | One cadence step | Target interval between flash publications. |
| `daily_slots` | `2` | 1–4 | Immediate global | Curator, direct | One slot per apply | Number of daily publication slots. |
| `feature_flash_markets` | `true` | Boolean | Immediate global | Ops, direct | Not applicable | Shows or hides the flash-market feature. |
| `feature_comments` | `true` | Boolean | Immediate global | Ops, direct | Not applicable | Shows or hides comments. |
| `feature_referrals` | `false` | Boolean | Immediate global | Finance, two-phase (refuses `true` until phone/bind tables exist) | Not applicable | Shows or hides referrals. |
| `trading_paused` | `false` | Boolean | Immediate global | Ops, direct | Not applicable | Stops new trades globally at the authoritative write fence. |
| `market_paused:{market_uuid}` | unset | Boolean with a valid market UUID suffix | Immediate market | Ops, direct | Not applicable | Stops new trades on one market. |
| `voting_paused:{market_uuid}` | unset | Boolean with a valid market UUID suffix | Immediate market, auto-expires at hidden start | Superadmin, two-phase | Not applicable | Temporarily stops votes and trades on one live market. |
| `faucet_per_call_cap_micro` | `1000000000` | 0–1,000,000,000 micro | Immediate staging-only | Finance, direct | At most 2× or ½× prior | Maximum one-call test credit; the faucet is absent outside the two-factor staging arm. |
| `remedial_credit_market_cap_micro` | `500000000` | 0–500,000,000 micro | Immediate control limit | Finance, two-phase | Bounded value | Maximum remedial credit associated with one market. |
| `remedial_credit_daily_cap_micro` | `2000000000` | 0–2,000,000,000 micro | Immediate control limit | Finance, two-phase | Bounded value | Maximum total house remedial credits per day. |
| `receivable_outstanding_cap_micro` | `10000000000` | 0–1,000,000,000,000 micro | Immediate control limit | Finance, two-phase | Bounded value | Stops new unwind confirmations when house receivables are too large. |
| `writeoff_per_item_cap_micro` | `500000000` | 0–500,000,000 micro | Immediate control limit | Finance, two-phase | Bounded value | Maximum amount forgiven in one receivable write-off. |
| `writeoff_daily_cap_micro` | `2000000000` | 0–2,000,000,000 micro | Immediate control limit | Finance, two-phase | Bounded value | Maximum receivable write-offs per day. |
| `proposal_ttl_secs` | `900` | 60–3,600 s | Immediate control policy | Superadmin, two-phase | Bounded value | Time before an unconfirmed proposal expires. |
| `dual_control_delay_secs` | `60` | 0–86,400 s | Immediate control policy | Superadmin, two-phase | Bounded value | Minimum delay between proposal and confirmation for delayed dual-control actions. |

## Operator honesty

An operator pause is not D22’s full-freeze window and is not a config hatch. It cannot move a live market’s close, hidden-tally timestamp, fee, participation threshold, or other stamped economics. Trading pause stops trades but not votes; voting pause is separately fenced, stops both while active, cannot be written inside the hidden window, and expires automatically at `tally_hidden_at`.

A void is a terminal neutral settlement. Unwind from `Voided` means the void was the wrong fraud decision; it is never a t0 cleanup mechanism. Unwind cannot be used after `Paid`, reverses the enumerated economic facts, and may create a non-cash receivable when a seller no longer has enough cash.

A remedial credit is a capped, audited house grant. It is not a refund and does not promise to make a user’s P&L whole. Proposal and confirmation require distinct token ids, and neither principal may be linked to the credited account.

A receivable write-off is an economic transfer, not bookkeeping cleanup. It requires the same distinct-principal dual control, delay, reason, caps, and two atomic audits as the other unwind-grade actions.

Principal linkage is deliberately narrow in Phase 6: an account is linked to an admin principal only when the user handle exactly equals that principal’s stored digest (`handle == digest`). This bound is explicit; it is not a general identity graph.

Public signup accepts no age or reputation override. Backdating and rep-seeding are available only through the separately audited two-factor staging arm (`OPINIONS_ENV=staging` and `STAGING_FAUCET=1`); a production mis-set alone cannot backdate or seed a real user. Synthetic phone addresses used by the swarm are not verified OTP identities.

Platform-absorbed SOL gas on withdrawals is accepted house opex. The USDC hot-wallet daily cap divided by the per-tx minimum bounds the attempt count.

## Withdrawal combination table (D31 / 7.0a)

Authority for `0011` CHECKs and for W1. Three persisted dimensions on the ALTERed `withdrawals` row: coarse `status`, `review_state`, `send_state`. Every accepted row has `hold_tx_id` NOT NULL (`User → Withheld`). `release_tx_id` and `settle_tx_id` are never both set. Terminal rows have exactly one of them.

### Legal combinations

`H` = `hold_tx_id` set. `R` / `S` = release / settle set. `OP` = `outbound_payments` row for this withdrawal. Attempts: 0, or at most one in `{prepared,broadcast,unknown}`, and at most one `finalized` ever.

| ID | status | review_state | send_state | H | R | S | OP | attempts | Meaning |
|---|---|---|---|---|---|---|---|---|---|
| W1 | queued | screening | unsent | Y | — | — | — | 0 | Insert after request tx. Decision not yet applied. |
| W2 | queued | approved | unsent | Y | — | — | — | 0 | Auto-approved, or human/dual-control approved, not yet claimed for send. |
| W3 | queued | approved | sending | Y | — | — | Y | 1 prepared | Send claimed; signed bytes persisted; not broadcast. |
| W4 | risk_hold | review_required | unsent | Y | — | — | — | 0 | Decision: not auto-approved. Waiting finance. |
| W5 | risk_hold | approval_proposed | unsent | Y | — | — | — | 0 | Dual-control proposal open (`money_command_proposals`). |
| W6 | sent | approved | broadcast | Y | — | — | Y | 1 broadcast | Bytes on cluster; not finalized. |
| W7 | sent | approved | unknown | Y | — | — | Y | 1 unknown | Landing unproven. Funds stay Withheld. Pages. |
| W8 | sent | approved | sending | Y | — | — | Y | 1 prepared (replaces expired) | Blockhash-expiry replacement after W7; same dest/amount. |
| W9 | sent | approved | finalized | Y | — | — | Y | 1 finalized | Receipt verified; settle not committed (crash window). |
| W10 | settled | approved | finalized | Y | — | Y | Y | 1 finalized | Terminal success. `Withheld → External`. |
| W11 | denied | screening | unsent | Y | Y | — | — | 0 | Deny or unwind-cancel during screening. |
| W12 | denied | review_required | unsent | Y | Y | — | — | 0 | Deny from review. |
| W13 | denied | approval_proposed | unsent | Y | Y | — | — | 0 | Proposal rejected or deny while proposed. |
| W14 | denied | approved | unsent | Y | Y | — | — | 0 | Unwind-cancel of approved-unsent, or deny after approve before send-claim. |
| W15 | failed | approved | definitive_failed | Y | Y | — | Y | last = definitive_failed; 0 finalized | Proven non-landing. Hold released. |

Any other triple is illegal. In particular: send_state ≠ `unsent` requires `review_state = approved`; `risk_hold` is only `review_required` or `approval_proposed` with `unsent`; `settled`/`failed`/`sent` require `approved`; `denied` is only `unsent`; both `R` and `S` set is illegal.

CHECK equivalent (0011 must implement this allowlist plus the H/R/S nullability above):

```
(status, review_state, send_state) IN (
  ('queued','screening','unsent'),
  ('queued','approved','unsent'),
  ('queued','approved','sending'),
  ('risk_hold','review_required','unsent'),
  ('risk_hold','approval_proposed','unsent'),
  ('sent','approved','broadcast'),
  ('sent','approved','unknown'),
  ('sent','approved','sending'),
  ('sent','approved','finalized'),
  ('settled','approved','finalized'),
  ('denied','screening','unsent'),
  ('denied','review_required','unsent'),
  ('denied','approval_proposed','unsent'),
  ('denied','approved','unsent'),
  ('failed','approved','definitive_failed')
)
AND hold_tx_id IS NOT NULL
AND NOT (release_tx_id IS NOT NULL AND settle_tx_id IS NOT NULL)
AND (status = 'settled') = (settle_tx_id IS NOT NULL)
AND (status IN ('denied','failed')) = (release_tx_id IS NOT NULL)
```

### CAS transitions (source → target)

Every transition is a single-row CAS on `(id, status, review_state, send_state)` and writes `withdrawal_events` + audit (when an Admin actor) + outbox in the same tx as any ledger leg.

| CAS | From | To | Authority | Coupled ledger |
|---|---|---|---|---|
| insert | (none) | W1 | `withdraw_request` after `lock_user` | `User → Withheld` (`hold_tx_id`) |
| auto-approve | W1 | W2 | `withdraw_decide` (machine; dest warm + under auto + Clear + no AML) | none |
| hold-for-review | W1 | W4 | `withdraw_decide` | none |
| propose-dual | W4 | W5 | `withdraw_decide` + `money_command_proposals` insert | none |
| confirm-dual | W5 | W2 | `withdraw_decide` confirm (distinct token, delay elapsed) | none |
| finance-approve | W4 | W2 | `withdraw_decide` single-finance named exception (amount < dual) | none |
| expire-proposal | W5 | W4 | machine, when `money_command_proposals.expires_at` has elapsed | none |
| deny | W1/W2/W4/W5 | W11/W14/W12/W13 | `withdraw_decide` deny (reason) | `Withheld → User` (`release_tx_id`) |
| unwind-cancel | W1/W2/W4/W5 | matching denied | unwind under sorted user locks | `Withheld → User` then collect |
| send-claim | W2 | W3 | `withdraw_send` (lien recheck; lease attempt; persist signed bytes **before** broadcast) | none |
| broadcast | W3 | W6 | `withdraw_send` after RPC submit | none |
| mark-unknown | W6 or W3 (timeout) | W7 | `withdraw_reconcile` | none |
| replace-expired | W7 | W8 | `withdraw_send` only with proof the expired blockhash did not land | none |
| rebroadcast | W7 | W7 | `withdraw_send` — **same** persisted bytes only | none |
| observe-finalized | W6/W7/W8 | W9 | `withdraw_reconcile` (receipt: sig, mint, source, dest, exact delta, finalized) | none |
| settle | W9 | W10 | `withdraw_settle` | `Withheld → External` (`settle_tx_id`) |
| definitive-fail | W7 (or W8 after proof) | W15 | `withdraw_reconcile` — 2-of-3 archival RPCs: height > `last_valid` AND signature absent; prune/disagree/timeout ⇒ stay W7 | `Withheld → User` (`release_tx_id`) |

### Failure / retry edges

- Crash between persist-bytes and broadcast: row stays W3. On lease expiry FIRST do a signature lookup — hit ⇒ CAS to W6 (it was broadcast); miss with blockhash still valid ⇒ reclaim and rebroadcast the SAME persisted bytes; miss with blockhash expired ⇒ CAS to W7 and let the 2-of-3 predicate decide. Never a second prepared attempt while the first is still live.
- Crash between broadcast and CAS to W6: reconciler looks up signature; hit ⇒ W6/W9; miss ⇒ W7.
- Crash between W9 and settle: reconciler re-runs settle under `withdraw-settle:<id>` idempotency; identity (c) holds.
- `unknown` never becomes `failed` without the 2-of-3 predicate. Pruned history ⇒ `unknown`.
- Fingerprint hit on `(user, amount_micro, canonical dest)` returns the original receipt (including a persisted refusal) and writes nothing.
- Unwind meeting W3/W6/W7/W8/W9 does **not** cancel (sent-unsettled is outside the lien); shortfall may open a receivable.

### W1 implementation notes

- Request protocol order is lock-free fingerprint lookup → remote geo/sanctions → `serialize_key` → `lock_user` → revalidate → auto-collect → settled-counting daily/dest/hot windows → `User → Withheld` hold as W1.
- Machine decide applies dest-warmth (settled floor + first-settle age; dust/shared/refund dests never warm) and tighten-only `union(snapshot, current)`.
- Dual-control lives in `money_command_proposals`; only the machine actor may run the W1 decision pass or expire-proposal W5→W4 after `expires_at`. Single-finance approve is the named exception below `withdraw_dual_control_micro`.
- Every follow-up CAS resolves the lock-free owner, takes `lock_user`, then row-locks and revalidates the owner. Send/recovery also revalidate immutable outbound subject, destination, amount, rail fingerprint, attempt state, and lease shape before any rebroadcast or state advance.
- Send persists signed bytes before broadcast. W3 lease-lookup does signature presence first; W7 rebroadcast accepts only the same persisted unknown attempt. The subject-agnostic `OutboundIo` lineage is shared with `deposit_refund`, and its rail cache rejects byte/signature drift for an existing payment id. Settle is `Withheld → External` keyed `withdraw-settle:<id>` plus `withdrawal_settled`.
- Non-landing is the 2-of-3 archival predicate; prune/disagree/timeout stay W7.

### Legacy `0001` mappings (0011 backfill)

The 0001 table has `status ∈ {queued,risk_hold,sent,settled}`, nullable `txn_id`, nullable `chain_sig`. Phase 7 has no Withheld history for those rows.

| Legacy | Action |
|---|---|
| `queued`/`risk_hold`/`sent`/`settled` with `txn_id` that is a two-leg `User ↔ External` (no Withheld) | **Quarantine** (`withdrawals_legacy_quarantine`): do not invent Withheld legs; do not set `hold_tx_id`. Operator-only. |
| `queued`/`risk_hold` with `txn_id` IS NULL and no ledger debit | **Quarantine**. |
| No pre-existing rows (empty table, the expected Phase-6 shape) | No backfill. New rows use this table only. |

`chain_sig` on a quarantined or future-migrated row becomes a nullable reference to a finalized attempt id when one exists; it is not a send authority.

New columns default so a CHECK-passing insert from Phase 7 always writes an explicit triple. There is no implicit `review_state`/`send_state` default that would admit an illegal triple.
