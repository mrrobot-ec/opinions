# Getting paid

Settlement is where a market stops being opinions and becomes money. It is a single
database transaction, and it either happens completely or not at all.

## The setup

The market closed. 41% voted yes. So, forever after:

- every **yes** share is worth exactly **41 cents**
- every **no** share is worth exactly **59 cents**

Sam holds 21.1726 yes shares → **$8.680766**. Other people hold the rest. The pool holds
some of both, because it is the counterparty to every trade.

## The transaction

```mermaid
sequenceDiagram
    autonumber
    participant Sched as Scheduler
    participant UC as ResolveMarket
    participant Dom as Domain settlement
    participant DB as PostgreSQL

    Sched->>UC: this market is due
    UC->>DB: BEGIN
    UC->>DB: claim the settlement key `resolve:<market>`
    UC->>DB: enumerate voters, sort, lock their reputation rows
    UC->>DB: enumerate referral-relevant users, sort, lock them
    UC->>DB: lock the market
    UC->>DB: is a curator decision required? is the sweep clean?
    UC->>DB: read the REAL escrow balance
    UC->>DB: count the votes; read the open interest
    UC->>DB: enumerate every holding, users and pool alike
    UC->>Dom: settle(holdings, 41%, escrow)
    Dom-->>UC: exact payout per holding + dust
    UC->>DB: one ledger transaction paying everyone
    Note over UC,DB: the crash point fires here, in chaos tests
    UC->>DB: stamp the collateral-at-close fact
    UC->>DB: zero every position, write a realisation per holder
    UC->>DB: record the pool's profit or loss
    UC->>DB: write both outcomes' redemption values
    UC->>DB: score every vote; update reputations and tiers
    UC->>DB: mark Resolved, then Paid
    UC->>DB: finalise promotional credit progress
    UC->>DB: grant qualifying referral bonuses
    UC->>DB: outbox events
    UC->>DB: COMMIT
```

## The details that matter

**The escrow balance is read, not assumed.** The payout is computed against what the
market's account *actually holds*, not a running total the code kept in its head. If those
two ever disagreed, assuming would pay out money that is not there.

**Every share is presented, including the pool's.** The settlement function does not just
accept whatever holdings it is handed. It adds up the yes side and the no side separately
and demands that **each equals the number of complete sets the escrow can back**. If the
caller forgot the pool's inventory, that is a hard error — not a quietly smaller payout with
the difference written off as dust. Forgetting a holder is a bug, and the arithmetic refuses
to hide it.

**Everyone is paid in one transaction.** A thousand holders is still one transaction. Not a
thousand small ones that could half-fail.

**Locks are taken in sorted order.** Settlement needs many user locks at once. It collects
the users, sorts them, and takes the locks in that order — before the market lock — which is
what stops it from deadlocking against a trade or a withdrawal happening at the same instant.

**The dust is assigned explicitly.** Dividing a pot among holders rarely comes out even. Each
payout is rounded down, and the accumulated remainder goes to the fee account as a line item.
The settlement is rejected outright unless the remainder is smaller than the number of
holdings — the tightest bound rounding down can possibly produce. That check is what proves
the leftover is rounding crumbs and not a missing payout.

**Nothing is created.** The sum of every payout plus the dust equals exactly what was in
escrow. The domain refuses to produce a settlement where that is not true, and the ledger
refuses to save one.

**Zero legs are omitted.** A holder whose payout rounds to zero gets no entry rather than a
zero one. In the degenerate case where every leg is zero, no ledger transaction is written at
all.

## What else happens in the same transaction

Settlement is the moment several other things become final, and it matters that they are all
inside the same commit:

- **Positions are zeroed** and a **realisation** is written for each holder recording exactly
  what they made or lost. This is the permanent record leaderboards and profiles read.
- **The pool's profit or loss** is computed as what its inventory redeemed for minus what the
  house seeded, and stamped on the market.
- **Every vote is scored**, and — provided the market cleared its participation minimum and
  its pot was big enough to matter — every voter's reputation and tier are updated.
- **Promotional credit progress is finalised.** Fee allocations move from provisional to
  final *only here*. A market that voids never finalises them.
- **Referral bonuses are granted**, if the referee's first paid market has just landed.

## The crash-safety property

Suppose the server dies halfway through.

Nothing was committed, so nothing happened: no payouts, no state change. On restart the
market is still due, the job runs again, claims the same settlement key, and completes.

If it dies *after* committing, the settlement key already exists — so the retry sees it,
reconstructs the receipt from the market's final state, and returns it instead of paying
twice.

This is not asserted, it is tested. There is an injected **crash point** with exactly one
call site: immediately after the payout ledger write and before any subsequent write — the
most dangerous instant in the whole operation. In a chaos run the process dies there, and the
restart must produce exactly one payout.

That crash point is inert in production. Arming it requires *both* `OPINIONS_ENV=staging` and
`CHAOS_ENABLED=1`; setting one without the other is a startup error, not a silently armed
fault.

## What Sam sees

A notification: *"Market resolved: 41% yes. You received $8.68."*

That notification came from an outbox row written inside the settlement transaction. It
cannot arrive for a settlement that did not commit, and it cannot be missing for one that
did. See [How live updates work](../build/live-updates.md).

## If the market is voided instead

The void path reuses `settle_market(holdings, 50%, escrow)` **verbatim** — the same function,
the same conservation checks, the same dust rules, just with 5,000 basis points instead of
the tally. Void is not a special case bolted on; it is the ordinary settlement path with a
different number.

Which means, plainly: a void is **not** a refund. Every share redeems at 50 cents. If you
bought yes at 70 cents you lose 20 cents a share; if you bought at 30 you gain 20.

Two things are deliberately *not* done on the void path:

- **Votes are not scored and reputation is not updated.** A voided market produced no truth
  to be measured against, and scoring it neutrally would mint free majority reputation for
  everyone who voted.
- **Promotional credit progress is not finalised.** A voided market's fee allocations stay
  provisional forever, so a wash-trader cannot void their way to a converted bonus.

## Where to go next

- [The ledger](../money/ledger.md)
- [Prices and fees](../money/fees-and-pricing.md) — where 41 cents comes from.
- [Rules that can never break](../money/invariants.md)
- [Tests and quality gates](../running/tests-and-gates.md)
