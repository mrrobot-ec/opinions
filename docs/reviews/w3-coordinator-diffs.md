# W3 coordinator diffs (frozen files)

W3 did not edit `resolve_market.rs` / `unwind_market.rs`. The coordinator
applied these hooks once, inside the existing lock set.

## resolve_market.rs — finalize allocations and grant referrals at Paid only

After the market is committed `Paid` and trade rows are visible:

```rust
// NEVER call this on Voided.
tx.finalize_market_fee_allocations(cmd.market).await?;
tx.grant_referrals_on_paid(cmd.market).await?;
```

`ResolveTx` owns the set-based implementation. It serializes every live
allocation on the market and appends exactly one `finalized` terminal child.

Do **not** convert here. Convert fires lazily on the user's next
`lock_user` (`convert_then_collect`).

The referral hook examines the Paid market's qualifying first-trade users,
uses the user locks already acquired by resolution, and mints each referral
pair at most once. It is not called for `Voided`.

## unwind_market.rs — reverse provisionals; never finalize

On a D30 unwind (Voided only — Paid is illegal):

```rust
tx.reverse_market_fee_allocations(cmd.market).await?;
```

This writes `reversed` children of every live `allocated` fact on the market.
It does not touch BonusReserve and does not convert. Fees return via the
existing unwind reversal; bonus progress is forfeited.

## Named Pg race W3 owns

payout vs trade/deposit/convert: convert-then-collect runs under
`lock_user` on PlaceTrade / CreditDeposit / CastVote. Payout-recipient
algorithm (sorted user locks) races these; conservation is the existing
sorted-lock set plus BonusReserve coverage.

## HTTP core route registration — coordinator action pending

W3 owns `crates/adapters/src/http/routes/deposit_admin.rs`, but did not edit
the coordinator-owned route aggregation. Apply this exact mechanical delta:

- add `pub mod deposit_admin;` in `http/routes/mod.rs`;
- merge `deposit_admin::router::<S>()` inside the RBAC-protected admin router;
- remove only the four deposit admit/refund paths from `phase7_admin_stubs`;
- add `deposit_admin::{admit_propose, admit_confirm, refund_propose,
  refund_confirm}` to `ApiDoc` paths; and
- add `deposit_admin::{DepositAdmissionDto, DepositRefundDto}` to `ApiDoc`
  schemas.

The durable coordinator request is orchestration message
`msg_9808b7f2072e`. W3 independently compiled and tested the unmounted module
through `tests/deposit_admin_compile.rs`; after mounting it, the coordinator
should rerun OpenAPI/client generation and the normal adapter gates.
