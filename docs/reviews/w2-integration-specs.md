# W2 → coordinator integration specs (Phase 7, D33/D34)

Everything below is W2-owned code that another owner has to *call* or *mount*.
W2 changed no file outside its bullet; each item names the exact seam.

## 1. Enforcement hook — W3 call sites (PlaceTrade / CastVote / credit_deposit)

`application/src/money/enforcement.rs` keeps the signature W3 already calls:

```rust
enforce_money_mutation(tx: &mut (dyn CreditIo + '_), user, kind, amount_micro, now) -> Result<(), AppError>
```

**No call-site edit is required.** The function is now the composition of two
public pieces, so W3 can use either half directly if it wants the facts without
the policy (for a preview, say):

| Item | Shape | Notes |
|---|---|---|
| `read_gate_snapshot(tx, user, now)` | `-> Result<MoneyGateSnapshot, StoreError>` | Straight-line reads, **no branches**. Runs all reads for every mutation kind. |
| `decide_money_mutation(&snap, kind, amount_micro)` | `-> Result<(), AppError>` | Pure. Every refusal is unit-provable without a store. |

**One behavioural note for W3:** the snapshot reads are now unconditional, so a
`CastVote` now performs the deposit-side reads too (`deposit_limit_micro`,
`pause_deposits`, both shadow caps) — seven extra indexed point lookups inside
the transaction that is already open under `lock_user`. If the 2k profile shows
this on the 300ms trade budget, tell W2 and the reader can be re-branched by
`kind`; it was written straight-line so the policy has exactly one home.

Refusal vocabulary is unchanged:

- banned → `MoneyForbidden("user banned")` — **except `DepositAdmit`**, which
  still evaluates, because a banned user's inbound leg books to suspense and
  only *egress* is frozen (D34 egress split).
- self-excluded → `MoneyForbidden("self-excluded")` for every kind.
- `DepositAdmit` → `DepositsPaused` / `ComplianceHold{kyc|geo|sanctions|deposit_limit}`.
- `PlaceTrade` → `MoneyForbidden(kyc|geo|sanctions)`.
- `CastVote` → `MoneyForbidden("geo")` only.
- shadow cap breach → `PositionCapExceeded { cap_micro, tier: 0 }` — the
  published tier-0 shape, no `ShadowLimited` code, nothing echoed that a probe
  could use to distinguish a shadowed account (D34).

## 2. Routers to mount (coordinator, `crates/main/src/main.rs`)

All three are state-erased `Router`s with their own state struct, so `core.rs`
and `routes/mod.rs` stay frozen. `core.rs::phase7_admin_stubs` currently answers
the compliance admin paths with `Unavailable`; mounting the real admin router
replaces those rows.

| Router | State | Mount |
|---|---|---|
| `http::routes::phone::router(PhoneApiState { store, clock, phone, hmac_secret, hmac_key_version, demo_token })` | needs `S: ComplianceAdminStore + Clone` | public, top level |
| `http::routes::kyc_webhook::router(KycWebhookState { store, clock, alerter, secret })` | same bound | public, top level |
| `http::routes::compliance_admin::admin_router(ComplianceAdminState { .. })` | same bound | **inside** the D26 RBAC sub-router — every path has a capability row in `http/middleware.rs`; merging it into the unguarded router would publish ban/unban |
| `http::routes::compliance_admin::public_router(same state)` | same bound | public, top level; mounts `/sandbox/kyc/complete` only when `sandbox_kyc` is armed |

`ComplianceAdminState` fields: `store`, `clock`, `sandbox_kyc` (two-factor arm,
`OPINIONS_ENV=staging` AND `STAGING_FAUCET=1`, identical to the faucet),
`policy_version` (stamped on sandbox-completed KYC facts), `demo_token`.

The store to pass is `adapters::pg::PgComplianceStore::from_store(&pg_store)`
(or `from_pool`). It is a pool wrapper, so `pg/store.rs` stays untouched.

Path convention, matching the capability matrix: `…/propose` carries the
**subject** id (user, flag, exclusion), `…/confirm` carries the **proposal**
id — the matrix's replay key for a confirm is the proposal id.

Ban/unban propose bodies take `{ reason, epoch }`; `epoch` is the replay
dimension from the matrix (`user id + epoch`). Same epoch + same target replays
the original proposal instead of opening a second one.

## 2b. Sandbox screening providers (coordinator ruling, option a)

`adapters/src/compliance/sandbox.rs` — `SandboxCompliance` implements all three
screening roles behind the faucet's two-factor arm. Unarmed, every call is
`StoreError::Unavailable("phase7:kyc" | "phase7:sanctions" | "phase7:geo")`, so
a production process that mounts it by accident denies instead of clearing.

```rust
SandboxCompliance::from_env(clock, region_allowset_value, region_allowset_version, region)
```

- `KycProvider::start_verification` → `sandbox-kyc:<user id>`, the stable
  reference `remember_provider_user` maps back through OUR records.
- `SanctionsScreen::screen` → `Clear{ttl 24h, policy_version "sandbox"}`, or
  `Hit` for any user passed to `plant_hit(user)` (that is the hook for the
  red-team frozen-funds / no-auto-refund legs).
- `GeoResolver::resolve` → runs the **real** D33 predicate
  (`evaluate_ip_geo` + `Allowset`), so an absent `region_allowset` is still
  deny-all and an out-of-allowset state is still a `Hit`, not an
  `Indeterminate`.

Wire it into `WithdrawServices { geo, sanctions }` (W1's extension) and into
whatever slot the deposit-admission path reads.

## 2c. Frozen diff for the coordinator: `MoneyProposalIo` on the fake

The Pg side is done — `pg/resolve_tx.rs` now delegates all four methods to the
one shared `money_command_proposals` authority in `pg/compliance_tx.rs`
(`proposal_insert` / `proposal_by_replay` / `proposal_by_id` /
`proposal_confirm`), proven by
`compliance_contract::the_money_effect_transaction_shares_one_proposal_authority_with_the_admin_surface`.
Note `proposal_confirm` is a CAS on `status = 'pending'`, so a second confirmer
moves nothing and reads back the first confirmation.

The **fake** side is still the empty default impl at
`crates/application/src/fakes/market.rs:485` (`impl MoneyProposalIo for InMemTx {}`).
W2 did not touch it: `fakes/market.rs` is frozen and the storage it needs is a
new `proposals: Vec<MoneyProposal>` on `Phase7Committed`/`Phase7Pending` in
`fakes/state.rs`, which plan §2 transfers to **W3**. Sequence it as: W3 adds the
field + `apply_phase7` merge, then the coordinator lands the four-method
delegation in the frozen `fakes/market.rs`. Until then any use case that
composes `MoneyProposalIo` through `InMemoryStore` fails closed with
`Unavailable("phase7:money-proposal")` — correct, but it will block a
fake-backed dual-control test.

## 3. Port change other lanes compile against

`ComplianceAdminTx` gained one method (implemented in both the Pg adapter and
the fake — no other lane implements this trait):

```rust
async fn refresh_phone_challenge(&mut self, id: Uuid, challenge: &str, expires_at: OffsetDateTime)
    -> Result<PhoneVerificationRow, StoreError>;
```

It fixes a real lockout: `phone_verifications` is unique on
`(number_hmac, hmac_key_version)`, so once a user's code expired, a second
`start_challenge` for the same number hit the unique index and the number was
dead forever. The same account's own unverified row is now re-armed in place
(new code, new horizon, attempts reset). A *different* account still gets
`Conflict("phone number already bound")` — the two-accounts-one-number race is
unchanged and still proven.

## 4. Defects found and fixed inside W2-owned code

1. **Per-dest AML reads counted unrelated conversions.** `PgComplianceTx::collect_legs`
   filtered deposits and withdrawals by dest but returned *every* converted lot
   in the window regardless of dest. Since a conversion's dest is the synthetic
   `convert:<lot>`, N conversions by anyone pushed every dest toward the
   structuring N. Conversions are now dest-filtered like every other leg.
2. **Expired phone code locked the number out** — see §3.
3. **Dead unreachable branch in `money::admin::enforce`.** The `FirstTrade` arm
   re-checked `allows_progress(sanctions)` after the same predicate had already
   refused above it. D33's "sanctions additionally at first trade" is satisfied
   by the unconditional check; the duplicate is removed.
4. **`start_self_exclusion` had an unreachable liveness re-check.** The store
   only returns unlifted rows, and an unlifted exclusion is in force whether or
   not its cooling-off elapsed (expiry only makes a *lift* proposable), so the
   guard is now `active_self_exclusion(..).is_some()`.
5. **Inbox conflict could be reported as success if the pager failed** — the
   page is now a side effect of the conflict path, and the typed
   `ProposalConflict` is what reaches the caller when paging succeeds.

## 4b. Seen in other lanes while verifying (not W2's to fix)

- `crates/adapters/tests/pg_contract.rs::pg_convert_vs_unwind_race_never_converts_provisional_progress`
  failed once on `Backend("deadlock detected")` in a full-suite run and passed
  on a targeted rerun. A race test that deadlocks rather than losing cleanly is
  a lock-order finding for W3, not flake to retry away.
- `crates/adapters/tests/pg_contract.rs::market_queries_and_trade_roles_round_trip`
  failed once with `StaleConfig { preview_generation: 1, current_generation: 4 }`
  in a full-suite run and passes in isolation — it pins `expected_config_version: Some(1)`
  while an earlier binary in the same database leaves the generation advanced.
  Cross-binary state, W3's to make order-independent.
- Whole-workspace `clippy -D warnings` is still red on W1/W3 files (per your
  ruling those are their exit passes). Every W2 file listed in §6 is clean.

## 5. Still owner-gated (not built, by plan §0)

- Real KYC / sanctions / phone vendors. The only phone provider is
  `adapters::phone::sandbox::SandboxPhone`, whose single code is
  `SANDBOX_CODE`. `http/routes/phone.rs` hashes that constant as the expected
  challenge; **a real vendor adapter must hand its issued code back through
  `PhoneVerification` before that route can be pointed at it** — otherwise the
  route's stored digest and the vendor's delivered code will never match.
- `region_allowset` ships absent = deny-all (D24/D33). Staging must apply the
  pinned non-production array before any money-path test; production content is
  the counsel-supplied proposal.

## 6. W2 file inventory (all fmt + clippy clean, 100% line coverage)

`application/src/money/`: `kyc.rs`, `sanctions.rs`, `geo.rs`, `aml.rs`,
`statuses.rs`, `self_exclusion.rs`, `phone_verification.rs`, `admin.rs`,
`enforcement.rs`; `application/src/fakes/compliance.rs`.
`adapters/src/`: `pg/compliance_tx.rs`, `inbox.rs`, `phone/sandbox.rs`,
`compliance/mod.rs`, `compliance/sandbox.rs`,
`http/routes/{phone,kyc_webhook,compliance_admin}.rs`,
`http/dto/compliance.rs`; plus the `MoneyProposalIo` delegation in
`pg/resolve_tx.rs`. Tests: `adapters/tests/compliance_contract.rs`.
