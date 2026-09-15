# Glossary

Plain-English definitions. Terms are grouped by where you will meet them.

## The product

**Market**
: One question people vote and trade on, with an opening time and a closing time. Markets
come in two tiers: **daily** (long-running, curated) and **flash** (short, frequent).

**Vote**
: Your free, one-per-market statement, in two parts: which side you believe, and your
guess at what percentage of everyone else will say yes. Votes decide the final answer;
the guess decides your score.

**Share**
: A claim on the final percentage. A **yes** share pays the final yes-percentage in
cents; a **no** share pays the rest. A matching pair is always worth exactly $1.

**Complete set**
: One yes plus one no, created together when money enters a market. The reason the books
can always balance.

**Pool** (or *liquidity pool*)
: The pile of yes and no shares you trade against. There is no human on the other side of
your trade — the house seeded the pool and carries its profit and loss.

**Slippage**
: The gap between the price displayed before your trade and the average price you actually
paid, caused by your own trade moving the price as it fills.

**Resolution**
: The moment after closing when votes are counted and shares become fixed amounts of
money.

**Void**
: A market settled *neutrally* — every share, yes and no, redeems at 50 cents — because
too few people voted or a human decided the result could not be trusted. It is not a
refund: someone who bought at 70 cents loses on a void.

**Hidden tally**
: The window before closing when the running percentage is concealed and trading is
frozen on both sides, so late arrivals cannot trade on near-certainty. Voting continues.

**Open interest**
: How much money is actually at stake in a market. Used as the second condition on
automatic voiding: a thinly-voted market with real money in it goes to a human instead.

**Reputation** and **tier**
: A number between 0 and 1 built from your vote scores, mapped to a tier from 0 to 4. See
[Scoring and reputation](scoring-and-reputation.md).

## Money

**Micro-dollar**
: One millionth of a dollar. All money is stored as whole numbers of micro-dollars, never
as decimals, because decimals drift and lose cents. $1.50 is stored as `1500000`.

**Basis point** (bp)
: One hundredth of one percent. The published trading fee of 1% is 100 bp.

**Ledger**
: The permanent list of every money movement. Nothing is ever edited or deleted; mistakes
are corrected by adding a reversing entry, like an accountant would.

**Double-entry**
: The rule that every movement lists where money left *and* where it arrived, and the two
must cancel out to zero. A transaction that does not sum to zero is rejected by the
database itself.

**Account class**
: What kind of thing an account belongs to. There are nine: user, pool, escrow, fees,
house, external, withheld, deposit suspense, and bonus reserve.

**Escrow**
: A market's own pot, holding the money backing its shares until it settles.

**External**
: The contra account standing for the outside world. It is the only account allowed to go
negative, and its negative balance is exactly the amount of money currently inside the
system.

**Withheld**
: A holding account for money in a withdrawal that has been requested but not yet sent.
Money here cannot be spent.

**Deposit suspense**
: A holding account for money that has arrived on-chain but has not yet been approved for
the user's spendable balance.

**Bonus reserve**
: A pre-funded pot that backs promotional credits. A credit cannot be granted unless this
reserve already covers it, so a promotion can never write a cheque the house cannot cash.

**Credits**
: Promotional balance held in a separate currency that cannot be withdrawn. It converts to
real money only after you have paid at least as much in trading fees, counting only fees
on markets that have finished paying out.

**Dust**
: The sub-cent remainder left over when a pot is divided among holders by rounding each
share down. It is assigned explicitly to the fee account, never silently dropped.

**Receivable**
: A non-cash record of money someone owes the house, created when reversing a market would
otherwise push a user's balance negative. It is tracked, collected, or explicitly written
off — never quietly forgotten.

## Safety and rules

**Invariant**
: A statement that must be true at all times, checked continuously. Example: "the outside
world's negative balance is exactly the sum of every internal account." If an invariant
breaks by even one micro-dollar, the sweep reports a violation.

**Idempotency**
: Doing the same thing twice has the same effect as doing it once. If your phone sends a
trade twice, you get one trade. Every money operation carries a key so repeats are
recognised and replayed rather than re-executed.

**Fail closed**
: When the software is unsure, it refuses rather than allowing. If the identity checker is
unreachable, the answer is "no", never "probably fine." If the list of permitted regions
is missing, *every* region is blocked.

**Dual control**
: Two different staff members with separate credentials must approve, and the second one
cannot be the first. Required for anything that moves money by hand. Recorded as a durable
proposal with a content fingerprint, a minimum delay, and an expiry.

**Audit fact**
: An unchangeable record of who did what, when, and why — written in the same database
transaction as the action itself, so an action can never exist without its audit trail.

**Shadow limit**
: A quiet cap applied to a suspected abuser. The refusal message is deliberately identical
to the ordinary published cap message, because a limit the target can detect is just a
worse ban.

**Kill switch** / **pause**
: An operator control that stops new trades globally or on one market. It is checked at a
fixed point after all the row locks are taken, so a request already queued cannot slip
past it.

## Software words you will see

**Crate**
: A Rust package — one buildable unit of code. This project has five: `domain`,
`application`, `adapters`, `main`, `simswarm`.

**Port** / **adapter**
: A *port* is a description of a job to be done ("something that can save a trade"). An
*adapter* is one concrete way of doing it (PostgreSQL, or an in-memory fake for tests).
Swapping the adapter never changes the rules.

**Domain**
: The pure rules — the maths of pricing, settlement, scoring and state transitions. It
knows nothing about databases or the web, which is what makes it easy to trust and test.

**Migration**
: A numbered file of database changes. Running them in order builds the database from
empty to current. They are never edited after they ship.

**Transaction** (database sense)
: A group of changes that all happen or none happen. This is how "nothing is half-done" is
enforced.

**Lock**
: A temporary claim on a row so two operations cannot change it at the same time. The
order in which locks are taken is fixed project-wide, because taking them in different
orders is how programs deadlock.

**Deadlock**
: Two operations each holding something the other needs, waiting forever. The cure is a
global rule about the order in which things are locked.

**Outbox**
: A table where events are written *inside* the same transaction as the money move, then
delivered afterwards by a separate reader. It guarantees you never get a notification for
something that did not happen, or miss one for something that did. See
[How live updates work](../build/live-updates.md).

**WebSocket**
: A connection that stays open, so the server can push updates to your browser the moment
they happen instead of the page asking "anything new?" on a timer.

**State machine**
: A named list of states plus an explicit list of which moves between them are legal.
Markets have nine states and seventeen legal moves; withdrawals have fifteen legal state
combinations. Anything not on the list is rejected.

**Saga**
: A multi-step process where each step is recorded, so a crash resumes from where it
stopped instead of starting over and duplicating work.

**Contract suite**
: One set of tests written once and run twice — against the in-memory fake and against
real PostgreSQL — so the fast fake can never drift from real behaviour.

**Property test**
: A test that generates thousands of random inputs and asserts a rule holds for all of
them, rather than checking a handful of hand-picked cases.

**Chaos test**
: A test that deliberately kills the process at the most dangerous instant to prove that
recovery is exactly-once.
