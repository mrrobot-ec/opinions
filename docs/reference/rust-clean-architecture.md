# Clean Architecture, SOLID, and Patterns: Python Instincts → This Rust Repo

Practical onboarding for contributors who already write hexagonal Python
(treasuryGraph, ultimate-rag, alphaqa) and are landing in **opinions**. The
architecture goals are the same as those projects; the *mechanisms* change
because Rust makes dependency direction, purity, and error shape compile-time
concerns instead of convention-plus-import-linter.

**Dependency rule (machine-enforced):**

```text
domain  ←  application  ←  adapters  ←  main
```

`scripts/check_dependency_rule.py` fails CI if any workspace crate depends the
wrong way. `application` must not mention `sqlx` or `axum` (grep in
`just deps-check`). This is the Rust twin of treasuryGraph's
`[tool.importlinter]` contracts and ultimate-rag's one-way layer rule.

---

## S1 — Side-by-side translation (your Python → this repo)

Named patterns from the Python trees you already use:

| Name (as you use it) | Where in Python | Rust idiom here | File pointers in *this* repo |
|---|---|---|---|
| **Ports / Protocols** | `typing.Protocol` in ultimate-rag `app/adapters/protocols.py`; treasuryGraph `application/ports/*` | Small role traits + `async_trait` for object-safe async methods | `crates/application/src/ports.rs` (`LedgerWriter`, `MarketReader`, `OutboxWriter`, …) |
| **ISP (role traits, not god repos)** | Split ports (`LedgerHeadRepository`, payment rails, queries) | Many narrow traits composed into tx aliases (`TradeTx`, `VoteTx`) via blanket impls | `ports.rs` (`TradeTx = IdempotencyGuard + MarketReader + … + Committable`) |
| **Use cases / application services** | One module per command (`execute_payment.py`, feature `service.py`) | One module + `struct UseCase<'a, S: Store>` + `execute` | `place_trade.rs`, `cast_vote.rs`, `resolve_market.rs`, `credit_deposit.rs`, … |
| **Command / result DTOs** | `@dataclass` commands + results; Pydantic at HTTP edge | Plain Rust structs (no framework types) | `model.rs` (`PlaceTradeCmd` lives next to use case; `TradeReceipt`, `MarketRow`) |
| **DI container / IoC** | `dependency-injector` (ultimate-rag `core/container.py`); frozen `Container` dataclass (treasuryGraph `composition/container.py`) | **No container.** Composition root wires concrete types once | `crates/main/src/main.rs` (`PgStore`, `SystemClock`, `AppState`, relay + scheduler spawns) |
| **Repository + Unit of Work** | Tx factories / session scopes; payment `ExecutePaymentTransactionFactory` | `Store` factories return `Box<dyn TradeTx + '_>`; `Committable::commit` is the UoW boundary | `ports.rs` (`trade_tx`, `commit`); `crates/adapters/src/pg/trade_tx.rs` |
| **Adapters (driven)** | SQLAlchemy / httpx implementations under `infrastructure/` or `adapters/` | `adapters` crate: Postgres + Axum only | `crates/adapters/src/pg/`, `crates/adapters/src/http/` |
| **Pure domain** | `domain/` with no FastAPI/SQLAlchemy (import-linter forbidden list) | Pure library crate: no I/O, no framework types | `crates/domain/src/*` (`money`, `ledger`, `amm`, `market`, `resolution`, `scoring`) |
| **Pydantic boundary models** | Request/response models at the edge | Serde DTOs + OpenAPI (`utoipa`) only in HTTP adapter | `crates/adapters/src/http/dto.rs`, `error.rs`, `routes.rs` |
| **Fakes for ports** | Hand-written fakes / provider overrides | `InMemoryStore` honoring lock + commit semantics | `crates/application/src/fakes.rs` |
| **Contract / shared port tests** | “same behavior for fake and real” (your testing doctrine) | Generic contract suites run against both fake and `PgStore` | `crates/application/src/contract.rs`; `crates/adapters/tests/pg_contract.rs` |
| **Dependency rule in CI** | import-linter / ruff import rules | Cargo workspace graph + script | `scripts/check_dependency_rule.py`, root `Cargo.toml` members |
| **Coverage / mutation** | `--cov-fail-under` on domain/features | llvm-cov floors; `cargo-mutants` + kill-rate gate | Phase plan gates; `scripts/mutation_gate.py` |
| **Strategy** | `rail_strategies.py` catalog of payment rails | Trait (or enum match) for interchangeable behavior; CPMM quote paths are free functions + `Side` | Domain: `amm.rs` (`quote_buy` / `quote_sell`); ports for I/O strategies |
| **Error taxonomy** | `DomainError` / `AppError` hierarchies | `thiserror` enums per layer, matchable by callers | Domain: `MoneyError`, `LedgerError`, `AmmError`; app: `StoreError`, `AppError` in `error.rs` |
| **Newtypes / branded IDs** | `MissionId`, `AttemptId` wrappers in treasuryGraph domain | Tuple structs wrapping `Uuid` / `i64` | `model.rs` (`MarketId`, `UserId`); `domain/money.rs` (`MicroUsd`, `MicroShares`, `BasisPoints`); `domain/ledger.rs` (`AccountId`) |
| **Lifecycle / state machine (OCP via enums)** | Domain enums + transition tables in payment/mission aggregates | Exhaustive `match` on `(MarketState, MarketEvent)` — illegal edges are compile-checked arms | `domain/market.rs` (`transition`); use case whitelist in `advance_market.rs` |
| **CQRS-lite** | Separate query ports vs write use cases | `MarketQueries` for lock-free reads; write roles never used for HTTP GET auth | `ports.rs` (`MarketQueries` vs `Store` / `*Tx`); `preview_trade.rs` |
| **Idempotency / saga keys** | Durable attempt ids + leases in payment execute | Every money write takes `idempotency_key`; guard-first then replay | `place_trade.rs`, `credit_deposit.rs`, `ports.rs` (`IdempotencyGuard`) |
| **Transactional outbox** | Event append co-located with state (evidence ledger patterns) | `OutboxWriter::append` inside same tx; relay publishes later | `ports.rs`; adapter relay `crates/adapters/src/relay.rs` |
| **Composition / assembly** | `composition/assembly.py`, `api.py` | Binary-only wiring | `crates/main/src/main.rs`, `seed.rs` |

### SOLID, one line each (as this repo implements them)

| Principle | How it shows up |
|---|---|
| **S** | One use-case module; `Store` is factories only (not a god facade) |
| **O** | New lifecycle edge or `TxnKind` / error variant — extend the enum and the exhaustive match; compiler flags missed arms |
| **L** | Contract suites: fake and Postgres must pass the same tests |
| **I** | Role traits (`VoteReader` ≠ `VoteWriter`); read path uses `MarketQueries` only |
| **D** | Use cases generic over `S: Store` / `C: Clock`; `main` injects `PgStore` |

### Generics vs trait objects (the Rust-specific fork)

Python Protocols are always dynamic. Here:

- **Use cases prefer generics** (`PlaceTrade<'a, S: Store, C: Clock>`) — static dispatch, monomorphized, no vtable in the hot path of *your* code.
- **Transaction objects are trait objects** (`Box<dyn TradeTx + '_>`) — one factory, many role methods, object-safe via `async_trait`. That is the ergonomic compromise for UoW handles with short lifetimes.

Prefer generics when the set of implementations is closed at the call site; prefer `dyn` when you need a single factory return type or heterogeneous storage. Effective Rust and common hexagonal write-ups push “generics first, trait objects when required.”

### Why DI containers do not translate

In Python you register providers and `@inject` / `Provide[Container.x]`. Rust's
ownership and lifetimes fight service-locator graphs: you cannot casually hold
`Arc<dyn Everything>` without erasing lifetimes and paying for object safety
everywhere. The idiomatic substitute is a **composition root** (Ploeh: pure DI
at the edge only) — construct the graph in `main`, pass references into use
cases. That is exactly what `crates/main` does; logic in `main` is a
review-blocker for the same reason business logic in FastAPI `Depends` wiring
is frowned upon.

---

## S2 — Patterns this codebase uses on purpose

Each row: **problem here** → **pattern** → **where to read**.

| Pattern | Problem it solves in opinions | Where |
|---|---|---|
| **Repository + Unit of Work** | Money, pool, position, trade row, and outbox must commit atomically or not at all; partial writes are safety bugs | `Store::trade_tx` etc. open one backend transaction; `Committable::commit` publishes state. See `place_trade.rs` sequence; PG impl under `adapters/src/pg/` |
| **Guard-first idempotency (saga key)** | Concurrent identical requests; PG unique violations abort the whole transaction so “catch and re-read” inside one tx is impossible | `IdempotencyGuard::serialize_key` first (`pg_advisory_xact_lock` analogue), then `txn_by_key` replay. Documented at top of `ports.rs` and `place_trade.rs` |
| **Transactional outbox** | WS/clients must not see events for rolled-back state; dual-write without 2PC | `OutboxWriter::append` in the same UoW; `OutboxRelay` claims rows, broadcasts, then marks `published_at` (`adapters/src/relay.rs`) — at-least-once; consumers dedupe |
| **CQRS-lite** | Preview and GETs must not take write locks or open write txs; write-path auth must not use stale views (TOCTOU) | `PreviewTrade` + HTTP reads → `MarketQueries`; writes → `market_for_update` / row locks on tx roles |
| **Newtypes** | `Uuid` soup and raw `i64` money mix silently in Python-shaped code | `MarketId`/`UserId`/…; `MicroUsd`/`MicroShares`/`BasisPoints`; `AccountId` — wrong unit is a type error |
| **Exhaustive enums as OCP / lifecycle table** | Illegal market transitions must be unrepresentable as “forgot an if” | `domain::market::transition`; `AdvanceMarket` additionally rejects financial events (`UseResolveMarket`) so settlement stays conservation-checked |
| **Strategy (domain pure functions + port strategies)** | CPMM quoting must be deterministic and float-free; I/O strategies stay behind ports | `domain/amm.rs` pure quotes; clock/store behind traits so tests inject fixed clocks and fakes |
| **Fakes over mocks + contract tests** | Mock-call assertions tautologically pass and drift from SQL reality | `fakes.rs` implements real semantics (locks, buffered commit); `contract.rs` suites run on fake and again on `PgStore` in `adapters/tests/pg_contract.rs` |
| **thiserror taxonomies** | Callers need matchable business rejections vs infrastructure failures | `AppError` (vote required, frozen, …) vs `StoreError` (not found, conflict, backend); domain errors `#[from]`-wrapped where appropriate |
| **Workspace crates as the dependency rule** | Soft layer folders get violated under deadline pressure | Four crates + `check_dependency_rule.py`; domain has zero internal deps |
| **DB as last line of defense** | Application bugs must not unbalance money | Migration deferred triggers (`migrations/0002_ledger_triggers.sql`); domain `Transaction::new` / `Balances::apply` still enforce first |
| **Fixed locking order** | Deadlocks and double-spend races under concurrency | Documented protocol in `ports.rs`: key lock → market FOR UPDATE → pool/position → ledger accounts sorted by id |

**Typestate note:** this repo does **not** use PhantomData compile-time state
machines on market handles. Lifecycle safety is an **exhaustive runtime state
table** plus use-case whitelists — the same intent as typestate (illegal
sequences rejected early), implemented in a way that maps cleanly to SQL-backed
aggregates.

**GoF patterns: what dissolves vs what survives**

| GoF-ish idea | In Rust / this repo |
|---|---|
| Abstract Factory | `Store` tx factories |
| Adapter | `adapters` crate implementing application ports |
| Strategy | Traits / enum dispatch (`Side`, rail-like ports if added) |
| Observer | Outbox + broadcast relay (not classic subject/observer classes) |
| Singleton | Process-wide resources constructed once in `main` (`PgPool`, relay) — not a global `lazy_static` service locator |
| Builder | Rare; prefer explicit structs and `Pool::new` constructors |
| Decorator / Proxy | Mostly unnecessary; middleware lives at HTTP edge if needed |
| Visitor | Exhaustive `match` on enums |
| DI container | Dissolves into composition root + generics |

---

## S3 — Pitfalls when porting Python clean-architecture instincts

1. **Reaching for a DI framework**  
   You do not need `dependency-injector`. Pass `&PgStore` and `Arc<dyn Clock>`
   (or a concrete clock) from `main`. If the graph feels painful, the ports are
   probably too wide — split roles (ISP), do not add a container.

2. **God `Repository` / god `Store` facade**  
   Python services sometimes grow one session object that does everything.
   Here `Store` only *opens* role-composed transactions; lock-free reads live
   on `MarketQueries`. Do not add authorization reads to `MarketQueries` from
   a write use case (TOCTOU; review-blocker per port comments).

3. **Mocks instead of fakes**  
   Python's `unittest.mock` / easy protocol stubs tempt call-count tests. Prefer
   a behavioral fake (`InMemoryStore`) and **shared contract tests**. If the
   fake diverges from Postgres, the contract suite is the bug report.

4. **Putting SQL or Axum types in application signatures**  
   Feels convenient (pass a `sqlx::Transaction`). It inverts the dependency
   rule and breaks the fake. Cross-port types are domain types or plain data in
   `model.rs` only.

5. **Exception-oriented control flow**  
   Python `raise DomainError` everywhere maps poorly. Use `Result` + layered
   enums; map `AppError` → HTTP in the adapter (`http/error.rs`), not with
   panics. `unwrap`/`expect` are Clippy-denied in library crates.

6. **Floats and “just use Decimal”**  
   Money and AMM math are integer micro-units with checked arithmetic and
   explicit rounding (fees ceil toward the house). `float_arithmetic` is denied
   workspace-wide. Do not smuggle `f64` “for the quote UI.”

7. **Post-hoc idempotency recovery**  
   Instinct: catch unique violation, re-select. Under PostgreSQL that aborts the
   transaction. Always `serialize_key` → lookup → maybe write. Replays set
   `replayed: true` and write nothing.

8. **Forgetting the outbox is part of the UoW**  
   Publishing to WS/HTTP side channels before commit reintroduces dual-write
   bugs. Append in-tx; let the relay handle at-least-once delivery.

9. **Treating `main` as a place for business rules**  
   Same smell as fat `api.py` modules. Scheduler and seed should call use cases
   (`AdvanceDue`, seed helpers), not reimplement transitions.

10. **Skipping the dependency-rule / coverage gates**  
    In Python, import-linter + cov thresholds kept ultimate-rag honest. Here,
    `just deps-check`, llvm-cov floors (domain 100%; application/adapters ≥ 90%
    per phase plan), and mutation kill rates (`mutation_gate.py`) are the same
    social contract. Do not “temporarily” depend adapters from application.

11. **Over-abstracting for speculative OCP**  
    ultimate-rag's own architecture doc warns against ceremony. Same here:
    add a trait when there is a second implementation or a test seam you
    already need — not for a future third database.

12. **Assuming typestate will replace domain validation**  
    Compile-time state machines are great for in-memory protocols; markets live
    in Postgres and are advanced by many actors. Keep the exhaustive transition
    function + row locks; do not invent unusable `Market<Live>` types that
    cannot be loaded from SQL.

---

## S4 — Sources

External material synthesized into the above (not copied). Prefer the repo
itself when local paths conflict with generic advice.

### Architecture & DI

- [Composition Root (Mark Seemann / ploeh)](https://blog.ploeh.dk/2011/07/28/CompositionRoot/) — pure DI at the edge; containers only there if at all.
- [Effective Rust — generics vs trait objects](https://www.lurklurk.org/effective-rust/generics.html) — prefer generics; trait objects for code size / heterogeneous dyn.
- Community discussion on hexagonal wiring in Rust (generics tax vs dyn ergonomics): [Lobsters thread](https://lobste.rs/s/j0hure/master_hexagonal_architecture_rust), [r/rust hexagonal](https://www.reddit.com/r/rust/comments/1lajcou/hexagonal_architecture_in_rust/).
- Why Rust rarely uses DI containers: [r/rust discussion](https://www.reddit.com/r/rust/comments/1ei32ik/i_want_to_understand_why_the_rust_community_is/).

### Idioms & patterns

- [Rust Design Patterns — Newtype](https://rust-unofficial.github.io/patterns/patterns/behavioural/newtype.html)
- [Shuttle — patterns with Rust types (newtypes)](https://www.shuttle.dev/blog/2022/07/28/patterns-with-rust-types)
- [thiserror](https://docs.rs/thiserror) / [crates.io thiserror](https://crates.io/crates/thiserror) — library-style matchable errors vs anyhow at binary edges.
- GoF → idiomatic Rust translation overview: [From GoF to Rustacean](https://medium.com/@theopinionatedev/from-gof-to-rustacean-translating-classic-design-patterns-to-idiomatic-rust-36e62735e1eb)

### Testing

- [Contract tests + infrastructure adapters](https://understandlegacycode.com/blog/if-you-mock-are-you-even-testing/) — same suite for fake and production port.
- [Fakes over mocks (tyrrrz)](https://tyrrrz.me/blog/fakes-over-mocks)
- Hexagonal ports and fakes discussion: [n14n.dev testing notes](https://n14n.dev/articles/2023/testing-part-2/)

### Your prior art (local, not web)

- ultimate-rag: `docs/architecture/code-architecture.md`, `app/adapters/protocols.py`, `app/core/container.py`
- treasuryGraph: `procuregraph/application/ports/*`, `application/payment/*`, `composition/container.py`, import-linter in `backend/pyproject.toml`
- alphaqa: feature slices + `adapters/` + domain passwords; RLS-enforced tenancy (`docs/architecture/foundation.md`)
- This plan's architecture charter: `docs/plans/phase1-core-write-path.md` (Architecture + Global Constraints)

---

## Quick contributor checklist

When you add a write path:

1. Put pure rules in `domain` (or extend an existing exhaustive enum/match).
2. Add or reuse **small** ports in `application/ports.rs`; compose them into a tx alias if they share a UoW.
3. Implement the use case guard-first with an idempotency key if money or oracle state moves.
4. Append outbox events before `commit`.
5. Extend `contract.rs` (and ensure `pg_contract` still passes).
6. Implement Postgres in `adapters/pg`, HTTP mapping only in `adapters/http`.
7. Wire in `main` only.
8. Run `python3 scripts/check_dependency_rule.py` and the coverage recipe for the crate you touched.

When you add a read path: prefer `MarketQueries` (or a new read-only port), not a write transaction.

That is the whole discipline — the same clean architecture you already practice in Python, expressed so the compiler and CI share the review load.
