# Tour of the codebase

A guided walk through the folders. You do not need to read any code to follow this — the
point is to know where things live and why.

## Top level

```
crates/        the Rust code, split into five packages
  domain/        pure rules and maths
  application/   use cases, ports, fakes, contract suites
  adapters/      PostgreSQL, HTTP, blockchain, language models
  main/          the program that wires it all together and starts up
  simswarm/      the 2,000-agent external test harness
migrations/    eleven numbered SQL files that build the database
services/      the Python iMessage service ("converse")
web/           the Next.js website
docs/          the spec, the decision log, plans, and every review report
docs-site/     this guide
scripts/       the guard scripts and end-to-end shell tests
justfile       shortcuts: `just test`, `just coverage`, `just ci`, `just docs`
openapi.json   the generated API description
```

Rough sizes, counted from the source files:

| Package | Lines | Files |
|---|---:|---:|
| `domain` | 3,655 | 15 |
| `application` | 61,258 | 91 |
| `adapters` | 40,804 | 89 |
| `simswarm` | 5,876 | 18 |
| `main` | 1,100 | 2 |

`main` is deliberately tiny because it contains wiring only. Its very first line of
documentation says that any logic in `main` is a review-blocker, and it is the one package
excluded from the coverage gate — with that exclusion justified in writing, in the file.

A large share of the `application` and `adapters` totals is tests: this project keeps unit
tests in the same file as the code they exercise, which is normal Rust practice.

## `crates/domain` — the rules

Small, pure, heavily tested. Every file:

| File | What it decides |
|---|---|
| `amm.rs` | Pricing. Buying and selling against the pool, the constant-product rule, the caps, and the guards that stop a pile being drained. |
| `money.rs` | The units — micro-dollars, micro-shares, basis points — and the fee split. |
| `fee_policy.rs` | One authority for the effective fee: tier discount, minimum floor, anti-flip rule. |
| `ledger.rs` | What a valid money transaction looks like: at least two entries, none zero, summing to zero per currency, and no internal account below zero. |
| `resolution.rs` | Turning a final vote percentage into exact payouts, including the dust and the conservation check. |
| `market.rs` | The nine market states and the seventeen legal moves between them. |
| `scoring.rs` | The published 75%-accuracy / 25%-majority vote score. |
| `reputation.rs` | The fixed-point moving average and the tier thresholds. |
| `integrity.rs` | The four post-close anomaly checks and the two-signal verdict. |
| `moderation.rs`, `mentions.rs` | Comment rules and mention parsing. |
| `drafting.rs`, `render_spec.rs`, `ranking.rs` | Market drafts, poster/video render specs, feed ranking. |

If you want to understand *what the product does mathematically*, this folder is the whole
story.

## `crates/application` — the procedures

The biggest layer, because this is where careful sequencing lives.

- **One file per use case at the root**: `place_trade.rs`, `preview_trade.rs`,
  `cast_vote.rs`, `resolve_market.rs`, `credit_deposit.rs`, `advance_due.rs`,
  `advance_market.rs`, `create_user.rs`, `seed_market.rs`, `comments.rs`, `notify.rs`,
  `moderation_escalate.rs`.
- `money/` — the Phase 7 money path: `withdraw_request.rs`, `withdraw_decide.rs`,
  `withdraw_send.rs`, `withdraw_settle.rs`, `withdraw_reconcile.rs`, `credits.rs`,
  `referrals.rs`, `kyc.rs`, `sanctions.rs`, `geo.rs`, `aml.rs`, `statuses.rs`,
  `self_exclusion.rs`, `phone_verification.rs`, `enforcement.rs`, `admin.rs`.
- `ports/` — the job descriptions the use cases depend on.
- `fakes/` — in-memory implementations used by the fast tests.
- `contract/` — the shared suites that both the fake and PostgreSQL must pass.
- `integrity/` — the continuous invariant sweep.
- `ops/` — the admin control plane: the configuration catalog and its validation,
  two-phase proposals, market unwinding, receivables, audit facts, alerts, the config
  reconciler, and the leased job runner.
- `content/`, `video/` — the market-creation pipeline and the poster/video job engine.
- `model.rs` — the shared data shapes and every configuration struct with its defaults.

## `crates/adapters` — the outside world

- `pg/` — one file per area of the database (`trade_tx.rs`, `vote_tx.rs`, `resolve_tx.rs`,
  `withdraw_tx.rs`, `credit_tx.rs`, `deposit_tx.rs`, `compliance_tx.rs`,
  `invariant_read_tx.rs`, …). This is where the SQL lives.
- `http/` — the web API: `routes/` for endpoints, `dto/` for the request and response
  shapes, `middleware.rs` for admin authentication and role checks, `ws.rs` for the
  WebSocket protocol.
- `rails/` — the Solana client: signing, sending, and confirming payments.
- `compliance/`, `phone/` — sandbox implementations of identity, screening and messaging
  providers, armed only under the two-factor staging switch.
- `llm/` — language-model clients for drafting and moderation, with an honest keyless
  fallback for drafting and a typed startup failure for moderation.
- `relay.rs`, `notifier.rs` — the two readers that turn outbox rows into live updates and
  notifications.
- `render/` — poster and share-card generation.

## `crates/simswarm` — the robot users

A harness that spins up as many as two thousand simulated players. Crucially it drives the
system **only through the public API**, exactly like a real user — its dependency rule
entry allows it no internal crates at all, so it physically cannot take a shortcut into
the database. See [The swarm](../running/the-swarm.md).

## `migrations/` — the database, built in order

Eleven numbered files. Running them from empty produces the current schema; they are never
edited once shipped. [The eleven migrations](migrations.md) walks through what each one
added and why.

## `scripts/` — the robot reviewers and the end-to-end tests

| Script | What it does |
|---|---|
| `check_dependency_rule.py` | Fails the build if a package depends on something it must not. |
| `check_frozen_manifest.py` | Verifies the checksums of 58 shared files that only the coordinating role may change. |
| `coverage_gate.py` | Asserts zero uncovered lines from the raw coverage export. |
| `mutation_gate.py` | Asserts at least 90% of deliberately introduced bugs are caught by tests. |
| `e2e_demo.sh` | Fresh database → seed → a trade over the iMessage service → invariant check. |
| `e2e_live_loop.sh`, `e2e_economy.sh`, `e2e_social.sh`, `e2e_content.sh` | One per phase, each printing its own GREEN banner. |
| `e2e_swarm_smoke.sh` | The full end-to-end swarm run against a live server and database. |
| `gen_openapi.sh`, `gen_api_models.sh` | Regenerate the API description and the Python client models from it. |

## `docs/` — the written record

- `spec.md` — the master specification.
- `decisions.md` — thirty-odd numbered decisions with their rationale, plus the open items
  that gate a launch.
- `plans/` — one execution plan per phase, 0 through 7.
- `reviews/` — 77 documents: 31 review reports from one AI reviewer, 23 from a second
  independent one, 18 written dispositions of what each round changed, and a handful of
  integration notes.
- `copy/` — the published operator and player-facing copy, including the withdrawal state
  table that migration `0011` implements.

## Where to go next

- [Where the data lives](the-database.md)
- [The eleven migrations](migrations.md)
- [The website and the iMessage bot](frontends.md)
- [Placing a trade](../walkthroughs/trade.md) — follow one request through all of this.
