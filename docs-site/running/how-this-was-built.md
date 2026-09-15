# How this was built

This project has an unusual construction story, and it is worth telling because it shaped
the code you are reading.

## Built by a team of AI agents, coordinated by another

The work was done by several AI agents running in parallel, each in its own terminal, with
one agent acting as **coordinator**: writing the plans, refereeing design questions, owning
shared files, and refusing to let a lane merge something unfinished.

The roles were fixed:

| Role | Job |
|---|---|
| Coordinator | Plans, design rulings, shared and frozen files, integration, verification |
| Builders | Implement one lane each, in files nobody else may touch |
| Reviewers | Attack every plan before code exists; audit afterwards |

Eight phases, built in order, each with its own written plan in `docs/plans/`:

| Phase | What it added |
|---|---|
| 0 | The workspace, the money core, schema v1, the conversation skeleton |
| 1 | The core write path: use cases, ports, the PostgreSQL and HTTP adapters |
| 2 | The core loop: live updates, the lifecycle scheduler, the vote-integrity bar, the installable web app |
| 3 | The economy: reputation, tier fees and caps, the integrity sweep, leaderboards, the liquidity kill switch |
| 4 | Social: threaded comments, moderation, notifications |
| 5 | The content engine: market drafting, the publication saga, poster and video jobs |
| 6 | The swarm, chaos scenarios, and the ops control plane |
| 7 | Money hardening and the compliance gate |
| 8 | This guide |

## Adversarial review before any code

Each phase started with a written plan. That plan was then handed to **two independent AI
reviewers with different instructions**: one hunting correctness and consistency, the other
hunting economic exploits and abuse. Both were told to try to break it.

They did, repeatedly. `docs/reviews/` holds 77 documents: 31 reports from one reviewer, 23
from the other, and 18 written dispositions recording exactly what each round accepted,
rejected, or deferred, and why.

Phase 7's plan alone went through **nine review rounds** — five from one reviewer and four
from the other — and roughly seventy findings before a single line of its code was written.
It was only declared ready when both reviewers independently approved the same revision.

A sample of what the reviewers caught, all of which are now rules elsewhere in this guide:

- A conversion of promotional credits that the ledger's own per-currency rules made
  impossible — found by reading the actual constraint code rather than the plan's prose.
- A path where unwinding a market refunded the very fees that had already earned somebody a
  cash bonus. Free money, twice. The fix was to make fee progress *provisional* until the
  market reaches `Paid`.
- A promotional reserve checked only when a bonus was claimed rather than when it was
  granted, so a promotion could promise more than the house could pay.
- A daily withdrawal limit that stopped counting a withdrawal once it settled: request the
  maximum, wait, request it again.
- A kill switch that could be raced by a request already queued on a row lock.
- A structuring detector that flagged four ordinary small deposits identically to deliberate
  evasion, because it counted everything below the threshold instead of a band.
- A service-level gate that would report success while measuring nothing.
- A transaction-wide ledger balance check that would have readmitted credit-to-cash
  conversion, because it summed across currencies instead of grouping by them.

Every one of those is cheap to fix in a document and expensive to fix in production.

## Parallel building with hard boundaries

Phases 5 and 7 were each built by four lanes working simultaneously. Phase 7's were:
withdrawals, compliance, deposits and credits, and observability.

Two rules made that safe:

1. **One owner per file.** The plan lists every path and its owner. A lane needing a change
   in someone else's file sent a request to the coordinator instead of editing it.
2. **A frozen manifest.** 58 shared files — migrations, ports, the composition root, the HTTP
   router — are checksummed, and any drift fails the build.

This mattered enormously because of the next point.

## There is no version control

The repository has **no git history**, by the owner's explicit decision. No branches, no
commits, no undo.

That constraint drove several practices: the tree must compile between every batch of edits,
because a broken file blocks every lane immediately; migrations are the single source of
truth for the schema; and the frozen manifest is the only guard against one lane overwriting
another's shared work. Several times an agent reported "the tree is red on a file I do not
own" — which is exactly the alarm working.

## Things that went wrong

Told honestly, because the fixes are instructive.

**A migration was edited after being applied.** New databases got the fix; the existing one
did not, and tests failed with a foreign-key error that appeared in no source file. Lesson:
the files are the truth; rebuild rather than patch.

**A column existed only in one worker's private database.** It had never made it into the
migration file at all, so that lane's tests passed and nobody else's would have. Caught
during final integration, not by the lane that introduced it.

**Three PostgreSQL contract suites were silently skipping.** Each needed its own scratch
database in the connection string, and without one, 26 tests reported success in 0.00
seconds having connected to nothing. The lane's own green report had hidden it. Each suite
now creates and migrates its own database.

**The coverage tool was under-reporting, three separate times.** Each investigation cost real
hours before the cause — max-folding of instrumentation records rather than union — was
identified. The fix made the gate stricter, not looser.

**A real deadlock.** Credit conversion and market unwinding took locks in different orders. A
concurrency test caught it by *hanging* instead of failing cleanly, which is the honest
symptom. Fixed by using a lighter lock mode so the cycle could not form.

**A worker's build directory ended up inside the source tree** — 653 MB of it — and a disk
filled up, which took the database down mid-run. Cleaned, and the sequence is now a known
hazard.

**Two workers idled with their instructions typed but unsent**, an interface quirk. The
supervisor now verifies that a worker is actually *working*, not merely that the text
arrived.

**The primary builder model hit its weekly usage limit mid-wave.** The remaining lanes were
handed to different models with a written note about the state the partial work was in.

## What the coordinator actually did all day

Mostly refereeing, in a loop: wait for a message, decide, unblock. Typical rulings:

- "Do not extend that interface — here is a narrower one that keeps the proposal and the
  money movement in a single transaction."
- "Deposit refunds must ride the withdrawal machinery; settling from a bare transaction
  signature is not acceptable."
- "That constraint makes the required terminal record impossible; here is the exact index
  shape to use instead."
- "Your files are green; the red is another lane's in-flight edit — keep going."

## Plans and decisions as durable artifacts

Every phase has a plan document, every review has a report, every round has a written
disposition, and every locked decision has a numbered entry in `docs/decisions.md` with its
rationale. When a question resurfaced weeks later, the answer and its reasoning were already
written down.

The decision log also records what is *not* decided: counsel engagement for real-money
vote-resolved wagering, written use-case approvals from payment and messaging vendors. Those
are marked as open items that gate a launch, not development — which is why this guide can
describe a complete withdrawal pipeline and still say the product cannot legally operate yet.

## Why tell you this

Because it explains the code's character: heavy on written rationale, allergic to silent
defaults, and defended by tests that assert attacks *fail*. Much of that came from a process
where every design had to survive two adversaries before it was allowed to exist.

## Where to go next

- [Tests and quality gates](tests-and-gates.md)
- [The swarm](the-swarm.md)
- [Rules that can never break](../money/invariants.md)
