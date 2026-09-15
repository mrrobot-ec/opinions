# Phase 6 plan — round 5 scoped delta verification

BLOCKED — D30 gives receivable write-off the required unwind-grade dual-control protocol, but D26's exhaustive dual-control endpoint list still omits receivable write-off: it lists only proposals, unwind, voting pause, and remedial credit.

1. **RESOLVED — D24 discount validator.** The plan now validates each discount entry against `[0, trade_fee_bps]`, monotonicity, and the 10bp/5min delta, while applying `min_fee_bps` only to the computed per-tier effective fee as `max(base - discount, min_fee)`. The published `[0,0,10,20,30]` seed is therefore valid.

2. **PARTIAL / BLOCKER — D30 write-off dual control.** D30 now specifies an immutable proposal, finance/superadmin distinct token ids, delay, reason, two atomic audits, idempotent confirmation, caps, linked movement/realization, and fake+Pg/RBAC proof. However, the promised addition to D26's dual-control endpoint list is absent, leaving D26 inconsistent with D30 and the round-4 disposition.

3. **RESOLVED — W1 converse version carrier.** W1 explicitly owns `services/converse/src/converse/{graph.py,core_client.py}` and their tests. The contract retains `config_version`, sends `expected_config_version`, and on 409 expires the pending action, re-previews, and requires a new lexical yes; `api_models.py` is regenerated.

4. **RESOLVED — D24 catalog attributes.** The round-4 gaps now have the stated attributes: rep thresholds have a 10% proposal delta; minimum-pot and payout-hold thresholds have 2x deltas; the daily seed budget has absolute bounds and a daily 2x delta; sweep delay has a 2x delta; cadence has a one-step delta; boolean flags mark max-delta not applicable; and faucet caps have a 2x delta.
