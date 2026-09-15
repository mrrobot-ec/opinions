# Phase 7 coordinator integration TODO (pre-barrier pass)

Owner: coordinator. Executed as ONE integration pass once W1 + W3 send worker_done
(W4 main-composition items already landed). Sources: docs/reviews/w2-integration-specs.md,
W1/W3 worker_done notes when they arrive.

1. **Mount W2 routers** (spec §2): extend `adapters::http` with a
   `Phase7Routers { public: Vec<Router>, admin: Vec<Router> }` pass-through (frozen
   core.rs edit): public → top-level merge (`phone::router`, `kyc_webhook::router`,
   `compliance_admin::public_router`); admin → nested INSIDE the D26 RBAC-wrapped
   subtree, replacing the `phase7_admin_stubs` rows (`compliance_admin::admin_router`).
   main.rs builds the states: `PgComplianceStore::from_store`, clock, alerter,
   `SandboxPhone`, hmac secret/key-version, sandbox_kyc two-factor arm, policy_version,
   demo token. Path convention: propose carries subject id, confirm carries proposal id;
   ban/unban bodies `{reason, epoch}`.
2. **Wire `SandboxCompliance`** (spec §2b): `from_env(clock, region_allowset_value,
   region_allowset_version, region)` → into W1's `WithdrawServices { geo, sanctions }`
   and the W3 deposit-admission screening slot, gated by the staging two-factor arm.
3. **fakes/market.rs MoneyProposalIo delegation** (spec §2c): AFTER W3 adds
   `proposals: Vec<MoneyProposal>` to `Phase7Committed/Phase7Pending` in fakes/state.rs
   (+ apply_phase7 merge; W3 was instructed 2026-08-14): land the four-method
   delegation in frozen fakes/market.rs mirroring the Pg CAS semantics
   (`proposal_confirm` CASes `status='pending'`; second confirmer reads back the first).
4. **Swap the alert-store composition line in main.rs** from `MemoryAlertStore` to
   W4's Pg AlertStore when W4 statuses that it exists (0011 columns are in place).
5. **W2 CastVote-read note** (spec §1): the straight-line snapshot reader does the
   deposit-side reads on every mutation; if the 2k profile burns the 300ms budget,
   re-branch by kind (W2 offered; W2 is released — becomes a barrier fix if needed).
6. **Real-vendor caveat** (spec §5): phone route digests `SANDBOX_CODE`; a real
   vendor adapter must hand its issued code back through `PhoneVerification` before
   the route can point at it. ops.md-worthy at barrier time.
