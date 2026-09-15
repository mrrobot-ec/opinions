# Opinions — Phase 1 (Core Write Path) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Money moves end-to-end through clean architecture: `application` use cases (PreviewTrade, PlaceTrade, CastVote, ResolveMarket, CreditDeposit) over ports, SQLx + Axum adapters, `main` wiring — proven by an e2e demo where a webhook text → preview → lexical confirm → executed trade → visible position, with every gate green.

**Architecture:** Hexagonal, dependencies point inward and are **machine-enforced** (`scripts/check_dependency_rule.py` in CI): `domain` ← `application` ← `adapters` ← `main`. Ports are small role traits defined in `application` (ISP); every port implementation — Postgres and in-memory fake — passes the **same contract-test suite** (LSP as executable tests, not doctrine); use cases are generic over ports (DIP; static dispatch); one use case per module (SRP); extension points are enums/traits already in domain (OCP). Patterns on purpose: repository + unit-of-work (the ledger transaction is the UoW), transactional outbox (events commit with state), saga/idempotency keys on every money flow, CQRS-lite (positions/price reads bypass use cases), newtypes end-to-end.

**Tech Stack:** Rust (pinned via rust-toolchain.toml), sqlx 0.8 (postgres, runtime-tokio, uuid, time), axum 0.8, utoipa 5 (+ utoipa-axum), async-trait, tokio, serde; Python: datamodel-code-generator (API models from OpenAPI), httpx; infra: docker compose Postgres 16 (host port **15434** — 5432 is occupied by another project on this machine).

## Global Constraints

Everything from Phase 0 (docs/plans/phase0-foundations.md Global Constraints) still binds. Phase 1 adds:

- **Dependency rule is CI-blocking:** `domain` depends on no internal crate; `application` only on `domain`; `adapters` only on `application` + `domain`; `main` may depend on all. `scripts/check_dependency_rule.py` parses `cargo metadata` and fails on any other internal edge.
- **Coverage gates:** `domain` stays 100% line; **`application` ≥ 90%, `adapters` ≥ 90%** (line, `cargo llvm-cov -p <crate> --fail-under-lines 90`), measured with the Postgres service up so adapter integration tests count. Floors never go down (ratchet by editing the gate values upward only).
- **Mutation gate goes blocking** on the nightly job (remove `continue-on-error`) for `domain`; `application` added to mutants at ≥85% kill, non-blocking this phase.
- **Every money- or oracle-writing use case takes an idempotency key** and must return the original result on replay (tested).
- **No SQL outside `adapters`**; no `axum`/`sqlx` types in `application` signatures (the dependency-rule script plus review enforce the spirit; types crossing ports are domain types or plain data).
- **The DB is the last line:** migration 0002's deferred constraint triggers validate per-(txn, currency) zero sums and non-empty transactions at COMMIT — the write adapter cannot bypass what the domain enforces.
- Ledger mapping is fixed (per-market escrow account, `owner_type='escrow', owner_id=market_id`):
  - BUY: user −gross · fees +fee · escrow +net
  - SELL: escrow −proceeds · user +net · fees +fee
  - PAYOUT/VOID-settle: escrow −(Σ payouts + dust) · each holder +payout · fees +dust
  - SEED: house −S · escrow +S
  - DEPOSIT: external −a · user +a (chain_sig dedupe)
- **Locking protocol (codex-p1r1 B1/B2/B3 — fixed order, every write use case):** (1) `pg_advisory_xact_lock(hashtext(idempotency_key))` FIRST, before any read — this serializes duplicate requests so read-or-create is race-free (a unique violation would abort the whole PG transaction, making "catch and re-read" impossible inside one tx); (2) `market_for_update` (row lock) and re-validate state/cutoffs under it; (3) `pool_for_update` / `position_for_update` as needed; (4) `ledger_apply` internally locks every touched `ledger_accounts` row `FOR UPDATE` **in account-id order** (deadlock-free), aggregates entries per account, validates post-balances (non-External ≥ 0) under the locks, then inserts. Contract tests: two-connection double-spend (exactly one debit commits), concurrent identical requests (one write, equal receipts), trade racing a freeze transition (loser rejects).
- Shared-worktree worker rules from Phase 0 (explicit `git add` paths, index.lock retry, tolerate cargo target-dir lock waits).

**Worker protocol:** Task 1.0 ∥ Task 1.1 first (independent); 1.2 after 1.1; 1.3 after 1.0+1.1; 1.4 after 1.1 (uses fakes, parallel with 1.3); 1.5 after 1.2+1.3+1.4; 1.6 (exit check) last.

---

### Task 1.0: DB triggers, infra corrections, dependency-rule gate

**Files:**
- Create: `migrations/0002_ledger_triggers.sql`, `scripts/check_dependency_rule.py`
- Modify: `docker-compose.yml` (postgres ports → `"15434:5432"`), `justfile` (default `DATABASE_URL`, new recipes), `.github/workflows/ci.yml` (postgres service, dependency-rule step, mutants blocking)

**Interfaces:**
- Produces: a database that rejects unbalanced money at COMMIT no matter who writes; CI that fails on architecture violations.

- [x] **Step 1: Write migration 0002**

```sql
-- Deferred, per-currency balance enforcement (R2/R3 review contract).
create or replace function assert_txn_balanced() returns trigger
language plpgsql as $$
declare bad record;
begin
  select la.currency, sum(le.amount_micro) as s
    into bad
    from ledger_entries le
    join ledger_accounts la on la.id = le.account_id
   where le.txn_id = coalesce(new.txn_id, old.txn_id)
   group by la.currency
  having sum(le.amount_micro) <> 0
   limit 1;
  if found then
    raise exception 'ledger txn % unbalanced in currency %: sum=%',
      coalesce(new.txn_id, old.txn_id), bad.currency, bad.s;
  end if;
  return null;
end $$;

create constraint trigger ledger_entries_balanced
  after insert or update or delete on ledger_entries
  deferrable initially deferred
  for each row execute function assert_txn_balanced();

-- Reject a header committed with zero entries (R3).
create or replace function assert_txn_nonempty() returns trigger
language plpgsql as $$
begin
  if not exists (select 1 from ledger_entries where txn_id = new.id) then
    raise exception 'ledger txn % committed with no entries', new.id;
  end if;
  return null;
end $$;

create constraint trigger ledger_transactions_nonempty
  after insert on ledger_transactions
  deferrable initially deferred
  for each row execute function assert_txn_nonempty();

-- Entries are append-only: forbid update/delete outright.
create or replace function forbid_entry_mutation() returns trigger
language plpgsql as $$
begin
  raise exception 'ledger_entries are append-only';
end $$;
create trigger ledger_entries_append_only
  before update or delete on ledger_entries
  for each row execute function forbid_entry_mutation();
```

(The balance trigger tolerates the append-only trigger: update/delete on entries can never fire it because they abort first. Statement ordering inside one transaction is irrelevant — validation happens at commit because the constraint triggers are `initially deferred`.)

Amendments (grok-p1r1 M4): the script parses each dependency's `kind` explicitly and fails on **both** normal and dev-dependency violations (a test-only inversion is still an inversion); and because a crate-graph check cannot see *type* leakage, the `deps-check` recipe also greps — `rg -l 'sqlx|axum' crates/application/src` must match nothing (skip gracefully while the crate doesn't exist yet). Necessary + sufficient is the pair, not either alone.

- [x] **Step 2: Write `scripts/check_dependency_rule.py`**

```python
#!/usr/bin/env python3
"""Fail if any internal crate dependency violates the inward-only rule."""
import json, subprocess, sys

ALLOWED = {
    "domain": set(),
    "application": {"domain"},
    "adapters": {"domain", "application"},
    "main": {"domain", "application", "adapters"},
}

meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--no-deps"]))
internal = {p["name"]: p for p in meta["packages"]}
bad = []
for name, pkg in internal.items():
    if name not in ALLOWED:
        bad.append(f"unknown internal crate {name}: add it to ALLOWED with its layer")
        continue
    deps = {d["name"] for d in pkg["dependencies"] if d["name"] in internal}
    for d in deps - ALLOWED[name]:
        bad.append(f"{name} -> {d} violates the dependency rule (allowed: {sorted(ALLOWED[name])})")
if bad:
    print("\n".join(bad)); sys.exit(1)
print(f"dependency rule OK ({len(internal)} internal crates)")
```

- [x] **Step 3: Infra corrections** — compose: postgres `ports: ["15434:5432"]`; justfile: top of file `export DATABASE_URL := env_var_or_default("DATABASE_URL", "postgres://opinions:opinions@localhost:15434/opinions")`, add recipes `deps-check: ; python3 scripts/check_dependency_rule.py` (indented body form), wire `deps-check` into `ci`; CI `rust` job gains a `postgres:16-alpine` service (user/password/db `opinions`, health-checked, mapped 5432) with `DATABASE_URL=postgres://opinions:opinions@localhost:5432/opinions` env, a `just deps-check` step, and migration application before tests (`cargo install sqlx-cli --no-default-features --features postgres` pinned, `sqlx migrate run --source migrations`); nightly job: remove `continue-on-error` from mutants (now blocking).

- [x] **Step 4: Verify** — `just infra-up`; recreate DB fresh; apply 0001 then 0002; then in one psql transaction: insert accounts (usdc user + usdc_credit user), a txn header, one entry only → `commit` must RAISE (unbalanced); balanced same-currency pair → commits; overall-zero cross-currency pair (usdc −5 / usdc_credit +5) → RAISE; header with no entries → RAISE; `update ledger_entries set amount_micro = 1` → RAISE append-only. `python3 scripts/check_dependency_rule.py` → OK.

- [x] **Step 5: Commit** — `git add migrations/0002_ledger_triggers.sql scripts/check_dependency_rule.py docker-compose.yml justfile .github/workflows/ci.yml && git commit -m "feat(db): deferred per-currency balance triggers; chore(ci): dependency-rule gate, pg service, blocking mutants"`

---

### Task 1.1: `application` crate — ports, fakes, contract harness, PreviewTrade + PlaceTrade

**Files:**
- Create: `crates/application/Cargo.toml`, `crates/application/src/lib.rs`, `crates/application/src/ports.rs`, `crates/application/src/model.rs`, `crates/application/src/error.rs`, `crates/application/src/fakes.rs`, `crates/application/src/contract.rs`, `crates/application/src/preview_trade.rs`, `crates/application/src/place_trade.rs`
- Modify: root `Cargo.toml` (workspace members += `crates/application`)

**Interfaces:**
- Consumes: everything `domain` exports (verify names against source — `Pool::new(yes, no, fee)`, `quote_buy(pool, side, collateral)`, `apply_fee`, `Transaction::new`, `AccountId`, `Currency`, `TxnKind`, `MarketState`, `transition`).
- Produces (used by Tasks 1.2–1.5):

```rust
// model.rs — plain data crossing ports (no sqlx/axum types anywhere in this crate)
pub struct MarketId(pub uuid::Uuid); pub struct UserId(pub uuid::Uuid);
pub struct OutcomeId(pub uuid::Uuid); pub struct TradeId(pub uuid::Uuid);
pub struct MarketRow { pub id: MarketId, pub slug: String, pub state: domain::market::MarketState,
    pub min_votes_to_resolve: i32, pub closes_at: OffsetDateTime, pub tally_hidden_at: OffsetDateTime,
    pub yes_outcome: OutcomeId, pub no_outcome: OutcomeId }
pub struct PoolRow { pub market: MarketId, pub pool: domain::amm::Pool }
pub struct TradeReceipt { pub trade_id: TradeId, pub ledger_txn: uuid::Uuid,
    pub side: domain::amm::Side, pub action: TradeAction, pub shares: MicroShares,
    pub gross: MicroUsd, pub fee: MicroUsd, pub avg_price_micro: i64, pub replayed: bool }
pub enum TradeAction { Buy, Sell }
pub struct Event { pub event_type: &'static str, pub aggregate_type: &'static str,
    pub aggregate_id: uuid::Uuid, pub payload: serde_json::Value }
pub struct Tally { pub yes_votes: i64, pub no_votes: i64 }
impl Tally { pub fn total(&self) -> i64 { self.yes_votes + self.no_votes }
    /// bps of YES among cast votes; caller handles total()==0 via the D21 void/curator branch.
    pub fn actual_yes_bps(&self) -> Option<u16> { /* yes*10_000/total, None if total==0 */ } }
pub struct PositionView { pub market: MarketId, pub outcome: OutcomeId, pub side: domain::amm::Side,
    pub shares: MicroShares, pub cost: MicroUsd, pub realized_pnl: MicroUsd }

// ports.rs — small role traits (ISP); async_trait for object safety
pub trait Clock: Send + Sync { fn now(&self) -> OffsetDateTime; }   // plain trait — sync method (grok m1)
#[async_trait] pub trait Store: Send + Sync {
    // Transaction factories ONLY — lock-free reads live on MarketQueries (codex ckpt 2).
    async fn trade_tx(&self) -> Result<Box<dyn TradeTx + '_>, StoreError>;
    async fn vote_tx(&self) -> Result<Box<dyn VoteTx + '_>, StoreError>;
    async fn resolve_tx(&self) -> Result<Box<dyn ResolveTx + '_>, StoreError>;
    async fn deposit_tx(&self) -> Result<Box<dyn DepositTx + '_>, StoreError>;
    async fn seed_tx(&self) -> Result<Box<dyn SeedTx + '_>, StoreError>;
    async fn advance_tx(&self) -> Result<Box<dyn AdvanceTx + '_>, StoreError>;
    async fn bootstrap_tx(&self) -> Result<Box<dyn BootstrapTx + '_>, StoreError>;
}
#[async_trait] pub trait MarketReader: Send {
    /// Row-locked read (SELECT ... FOR UPDATE) — every authority check in a write
    /// use case reads the market through THIS, never through Store views (codex B2).
    async fn market_for_update(&mut self, m: MarketId) -> Result<MarketRow, StoreError>;
}
#[async_trait] pub trait IdempotencyGuard: Send {
    /// pg_advisory_xact_lock(hashtext(key)) — MUST be the first call in every write tx (codex B3).
    async fn serialize_key(&mut self, key: &str) -> Result<(), StoreError>;
}
#[async_trait] pub trait VoteReader: Send { async fn user_voted(&mut self, u: UserId, m: MarketId) -> Result<bool, StoreError>;
    async fn tally(&mut self, m: MarketId) -> Result<Tally, StoreError> ; }
#[async_trait] pub trait PoolWriter: Send { async fn pool_for_update(&mut self, m: MarketId) -> Result<PoolRow, StoreError>;
    async fn save_reserves(&mut self, m: MarketId, pool: &domain::amm::Pool) -> Result<(), StoreError>; }
#[async_trait] pub trait LedgerWriter: Send {
    async fn txn_by_key(&mut self, key: &str) -> Result<Option<uuid::Uuid>, StoreError>;
    async fn account(&mut self, owner: OwnerRef, currency: domain::ledger::Currency) -> Result<domain::ledger::AccountId, StoreError>; // get-or-create (unique-indexed per 0003)
    /// Locks every touched account row FOR UPDATE in account-id order, aggregates
    /// entries per account, validates post-balances (non-External >= 0) under the
    /// locks, then inserts header + entries (codex B1). DB triggers stay the backstop.
    async fn ledger_apply(&mut self, kind: domain::ledger::TxnKind, key: &str,
        entries: &[domain::ledger::Entry]) -> Result<uuid::Uuid, StoreError>;
}
#[async_trait] pub trait TradeWriter: Send { async fn insert_trade(&mut self, t: NewTrade) -> Result<TradeId, StoreError>;
    async fn trade_by_ledger_txn(&mut self, txn: uuid::Uuid) -> Result<Option<TradeReceipt>, StoreError>; }
#[async_trait] pub trait PositionWriter: Send {
    async fn position_for_update(&mut self, u: UserId, o: OutcomeId) -> Result<Option<PositionRow>, StoreError>;
    async fn save_position(&mut self, p: PositionRow) -> Result<(), StoreError>;
}
// Position accounting rule (codex M5 — the ONLY formula, tested for partial/full/insufficient):
//   BUY:  shares += q.shares_out; cost += gross
//   SELL: require shares >= s else AppError::InsufficientShares;
//         cost_relieved = floor(cost * s / shares);
//         realized_pnl += net_proceeds - cost_relieved;
//         shares -= s; cost -= cost_relieved
pub struct PositionRow { pub user: UserId, pub outcome: OutcomeId,
    pub shares: MicroShares, pub cost: MicroUsd, pub realized_pnl: MicroUsd }
#[async_trait] pub trait OutboxWriter: Send { async fn append(&mut self, e: Event) -> Result<(), StoreError>; }
#[async_trait] pub trait Committable: Send { async fn commit(self: Box<Self>) -> Result<(), StoreError>; }
pub trait TradeTx: IdempotencyGuard + MarketReader + VoteReader + PoolWriter + LedgerWriter + TradeWriter + PositionWriter + OutboxWriter + Committable {}
// VoteTx / ResolveTx / DepositTx / SeedTx defined analogously in Task 1.2 with only the roles they need
// (every write-tx alias includes IdempotencyGuard; its serialize_key call is step 1 of the locking protocol).

// Store views split (codex ckpt 2 — Store was becoming a god facade): the tx factories
// stay on Store; lock-free reads live on a separate narrow port used by HTTP GET routes
// and PreviewTrade only:
#[async_trait] pub trait MarketQueries: Send + Sync {
    async fn market_by_ref(&self, r: &str) -> Result<MarketRow, StoreError>;
    async fn list_markets(&self, status: Option<&str>) -> Result<Vec<MarketRow>, StoreError>;
    async fn pool(&self, m: MarketId) -> Result<PoolRow, StoreError>;
    async fn positions(&self, u: UserId) -> Result<Vec<PositionView>, StoreError>;
    async fn user_voted(&self, u: UserId, m: MarketId) -> Result<bool, StoreError>;
    async fn user_by_channel(&self, channel: &str, address: &str) -> Result<Option<UserId>, StoreError>;
}
// PgStore implements both Store and MarketQueries; handlers get each by its own bound.

pub enum OwnerRef { User(UserId), MarketEscrow(MarketId), MarketPool(MarketId), Fees, House, External }
```

- Use cases are structs generic over ports: `PlaceTrade<'a, S: Store, C: Clock> { store: &'a S, clock: &'a C }` with `pub async fn execute(&self, cmd: PlaceTradeCmd) -> Result<TradeReceipt, AppError>`.
- `contract.rs` (signature per codex M2, fixed again in P1R2 — `Future<Output = Box<dyn Tx + '_>>` does not compile, E0637): suites are **generic over the Store itself** and open transactions through it, which sidesteps the factory-lifetime problem entirely:

```rust
pub async fn ledger_writer_contract<S: Store + ?Sized>(store: &S) {
    let mut tx = store.trade_tx().await.unwrap();
    assert_double_spend_blocked(tx.as_mut()).await;      // &mut (dyn TradeTx + '_) → &mut dyn LedgerWriter via upcast helper
    tx.commit().await.unwrap();                          // Committable::commit(self: Box<Self>)
}
pub async fn concurrent_duplicate_key_contract<S: Store + ?Sized>(store: &S) {
    let (a, b) = tokio::join!(store.trade_tx(), store.trade_tx());  // two parallel txs from ONE store
    /* drive both, assert exactly one write */
}
// helpers take `&mut (dyn RoleTrait + '_)`; suites never name a boxed-future type.
```

  Every suite runs identically against `InMemoryStore` (Task 1.1) and `PgStore` (Task 1.3) by passing the store. Concurrency suites open two transactions from the same store — the fake implements enough interior locking to honor the semantics, and the Pg run is the real proof (adapter-specific tests stay mandatory for isolation behavior).
- `fakes.rs`: `InMemoryStore` implementing every port over `parking_lot::Mutex<State>` maps + `domain::ledger::Balances` for money (reusing the domain aggregate keeps fake semantics honest).

**Business rules PlaceTrade must enforce (each is a test; P1R2-final sequence):**
1. **The write sequence is fixed and guard-first** (codex P1R2 B2 — there is NO post-unique-violation recovery in PostgreSQL; an aborted tx cannot read): open `trade_tx` → **`serialize_key(idempotency_key)`** → **`txn_by_key`: hit → load receipt via `trade_by_ledger_txn`, return `replayed: true`, write nothing** → miss (we hold the key lock; no concurrent twin can be mid-flight) → `market_for_update()` → `user_voted()` → `pool_for_update()` → validate → quote → write → commit. `StoreError::DuplicateKey` from `ledger_apply` is now an invariant violation (a bug), surfaced as an error — never a recovery path. Contract test: two concurrent identical PlaceTrade commands → exactly one write, two equal receipts (loser serialized behind the key lock and replayed).
2. `Store`'s lock-free read methods are for previews and views ONLY; a use case that consults them for authorization is a review-blocker (TOCTOU).
3. Market must be `Live` and `clock.now() < tally_hidden_at` — validated **under the market row lock** — else `AppError::TradingFrozen` / `MarketNotOpen` (D22 full freeze). Test includes the race shape: market flips to frozen between a stale preview and the tx — PlaceTrade still rejects.
4. Vote-gate: `user_voted` (in-tx) must be true — else `AppError::VoteRequired` (D4).
5. Quote via `domain::amm::quote_buy/quote_sell` against the row-locked pool; ledger mapping per Global Constraints; `save_reserves(pool_after)`; position update via the M5 formula (`position_for_update` → `save_position`).
6. Everything in ONE `TradeTx`; nothing observable on error (fake asserts state unchanged on failure paths).
7. `run_id`/`pending_action_id` pass through to `NewTrade` when supplied (agent causal chain).
8. **PreviewTrade does NOT open a `TradeTx`** — it is pure read (MarketQueries + domain quote), returns numbers only, writes nothing (ISP: read paths never touch writer roles).

- [x] **Step 1: failing tests** for the rules above against `InMemoryStore` (write the fake alongside; the contract suites double as the fake's own tests). Example shape:

```rust
#[tokio::test]
async fn replayed_key_returns_original_and_writes_nothing() {
    let (store, clock, market, user) = seeded_live_market_with_voter().await;
    let uc = PlaceTrade { store: &store, clock: &clock };
    let cmd = buy_cmd(&market, &user, 5_000_000, "key-1");
    let first = uc.execute(cmd.clone()).await.unwrap();
    let snapshot = store.snapshot();
    let second = uc.execute(cmd).await.unwrap();
    assert!(second.replayed && first.trade_id == second.trade_id);
    assert_eq!(snapshot, store.snapshot()); // no double-spend, no reserve drift
}
```

- [x] **Step 2: run to fail** (`cargo test -p application`) → **Step 3: implement** ports/fakes/use cases → **Step 4: green + coverage gates wired** (P1R2: the promise must live in the pipeline, not prose) — update the `justfile` `coverage` recipe to gate all existing crates:

```just
coverage:
    cargo llvm-cov -p domain --fail-under-lines 100
    cargo llvm-cov -p application --fail-under-lines 90
```

(Task 1.3 appends the `-p adapters --fail-under-lines 90` line when that crate lands; CI already runs `just coverage` with `DATABASE_URL` set.) → **Step 5: Commit** `feat(application): ports, fakes, contract harness, PreviewTrade + PlaceTrade`

---

### Task 1.2: `application` — CastVote, ResolveMarket (+auto-void), CreditDeposit

**Files:**
- Create: `crates/application/src/cast_vote.rs`, `crates/application/src/resolve_market.rs`, `crates/application/src/credit_deposit.rs`, `crates/application/src/seed_market.rs`, `crates/application/src/ensure_genesis.rs`, `crates/application/src/create_user.rs`, `crates/application/src/advance_market.rs`; extend `ports.rs` (`VoteTx`, `ResolveTx`, `DepositTx`, `SeedTx`, `BootstrapTx`, `AdvanceTx` + role traits below), `fakes.rs`, `contract.rs`

**Interfaces:**
- Produces:

```rust
#[async_trait] pub trait VoteWriter: Send {
    async fn allocate_vote_seq(&mut self, m: MarketId) -> Result<i64, StoreError>; // locked counter update..returning
    async fn insert_vote(&mut self, v: NewVote) -> Result<VoteId, StoreError>;     // unique(user,market) + idempotency_key
    async fn vote_by_key(&mut self, key: &str) -> Result<Option<VoteReceipt>, StoreError>;
}
pub trait VoteTx: IdempotencyGuard + MarketReader + VoteWriter + OutboxWriter + Committable {}
#[async_trait] pub trait SettlementIo: Send {
    async fn holdings(&mut self, m: MarketId) -> Result<Vec<(domain::ledger::AccountId, domain::amm::Side, MicroShares)>, StoreError>; // positions + POOL INVENTORY as the pool's own account
    /// Locked read of the market escrow account's actual balance — settle_market's
    /// escrow argument comes from HERE, never recomputed from assumptions (codex B4).
    async fn escrow_balance(&mut self, m: MarketId) -> Result<MicroUsd, StoreError>;
    async fn write_outcome_resolution(&mut self, o: OutcomeId, final_bps: u16, redemption: MicroUsd) -> Result<(), StoreError>;
    /// Immutable vote facts out; scores computed IN THE USE CASE via domain::scoring,
    /// persisted one by one — policy stays in application, not the adapter (codex M4).
    async fn vote_facts(&mut self, m: MarketId) -> Result<Vec<VoteFact>, StoreError>;
    async fn save_vote_score(&mut self, vote_id: uuid::Uuid, s: domain::scoring::VoteScore) -> Result<(), StoreError>;
    async fn set_market_state(&mut self, m: MarketId, s: domain::market::MarketState) -> Result<(), StoreError>; // guarded by domain::transition in the use case
    async fn open_interest(&mut self, m: MarketId) -> Result<MicroUsd, StoreError>;
}
pub struct VoteFact { pub vote_id: uuid::Uuid, pub side: domain::amm::Side, pub crowd_guess_pct: u8 }
pub struct ResolveConfig { pub oi_floor: MicroUsd }   // typed config injected into the use case (codex M4)
pub trait ResolveTx: IdempotencyGuard + MarketReader + VoteReader + LedgerWriter + SettlementIo + OutboxWriter + Committable {}
#[async_trait] pub trait DepositWriter: Send {
    async fn deposit_by_sig(&mut self, chain_sig: &str) -> Result<Option<DepositId>, StoreError>;
    async fn insert_deposit(&mut self, d: NewDeposit) -> Result<DepositId, StoreError>;
}
pub trait DepositTx: IdempotencyGuard + LedgerWriter + DepositWriter + OutboxWriter + Committable {}
#[async_trait] pub trait MarketWriter: Send {
    async fn insert_market(&mut self, m: NewMarket) -> Result<MarketId, StoreError>; // + outcome rows
    /// Creates the pools row AND the pool's ledger account (OwnerRef::MarketPool) —
    /// reserves cannot exist without their owner (codex P1R2 B3).
    async fn create_pool(&mut self, m: MarketId, fee: BasisPoints, seeded: MicroUsd) -> Result<(), StoreError>;
    async fn set_market_state(&mut self, m: MarketId, s: domain::market::MarketState) -> Result<(), StoreError>;
}
pub trait SeedTx: IdempotencyGuard + MarketWriter + PoolWriter + LedgerWriter + OutboxWriter + Committable {}
#[async_trait] pub trait UserWriter: Send {
    async fn insert_user(&mut self, handle: &str) -> Result<UserId, StoreError>;
    async fn link_channel(&mut self, u: UserId, channel: &str, address: &str) -> Result<(), StoreError>; // user_channels, unique(channel,address)
}
pub trait AdvanceTx: IdempotencyGuard + MarketReader + MarketWriter + OutboxWriter + Committable {}
pub trait BootstrapTx: IdempotencyGuard + LedgerWriter + UserWriter + OutboxWriter + Committable {}
```

**Bootstrap use cases (codex B5 — the seed path was unimplementable without them):**
- `EnsureGenesis`: idempotent (`genesis:house:<currency>` key) External→House capitalization of `GENESIS_HOUSE_MICRO` (env; demo default $10,000) — the house cannot SEED from a zero balance. Runs first in `seed.rs` and is safe to re-run.
- `CreateUser`: inserts user + optional channel link (`link_channel("imessage", phone)`) — the phone→user identity the converse service resolves via `MarketQueries::user_by_channel`.
- `AdvanceMarket` (codex M3, whitelisted in P1R2 B4): admin-driven lifecycle event through `domain::market::transition` inside `AdvanceTx` (illegal transitions surface as 409), emits `MarketAdvanced` — **accepts ONLY non-financial events** (`Approve, GoLive, EnterCloseWindow, Close, StartIntegritySweep`); `Resolve`, `Pay`, and both `Void*` events are rejected with `AppError::UseResolveMarket` because state changes with money consequences must ride the conservation-checked settlement transaction. This is what `POST /admin/markets/{id}/advance` calls; no business logic in the HTTP adapter.
- `SeedMarket` lifecycle contract (P1R2 B3): creates the market in `Draft`, applies `Approve` internally, and **returns it in `Scheduled`** — exactly one stated post-state; `seed.rs` then advances `Scheduled → Live` via `AdvanceMarket(GoLive)`. Precondition: house balance ≥ S (else `AppError::InsufficientFunds` — run `EnsureGenesis` first).

Composition rules (grok-p1r1 M5): role traits are the unit of implementation; the `*Tx` aliases exist ONLY as `Store` factory return types with blanket impls (`impl<T: MarketReader + … + Committable> TradeTx for T {}`), so a fake or Pg tx implements roles once and gets every alias free; read-only use cases never receive a writer-role object.

**Settlement materialization rule (codex B4):** the payout ledger transaction is built from `settle_market`'s output by **omitting every zero-valued leg** (a holder whose floor payout is 0, or dust of 0 — `Transaction::new` and the DB forbid zero entries); if ALL legs are zero (degenerate empty market) no ledger txn is written at all, only state + outcome rows. `escrow_balance` (locked) supplies the escrow argument, and the use case asserts holdings totals equal each side's outstanding supply before settling. Contract tests at `actual_yes_bps` = 0, 5_000, and 10_000 (the 0/100 cases are exactly where zero legs appear).

**Rules (each a test):** CastVote — same guard-first sequence as PlaceTrade rule 1 (`serialize_key` → replay-check → `market_for_update`); market `Live|Closing` **and `clock.now() < closes_at` validated under the market row lock** (codex P1R2: a lagging `Closing` row must not accept a post-cutoff oracle write); voting continues through the frozen window (trading doesn't); one per (user, market) → `AlreadyVoted`; `crowd_guess_pct <= 100` validated at the edge; idempotency replay returns original receipt + seq; seq strictly increasing. ResolveMarket — legal only from `Closed`/`Resolving` (via `domain::market::transition`, illegal → `AppError::IllegalTransition`); tally decides `actual_yes_bps`; **D21 branch:** votes < `min_votes_to_resolve` AND `open_interest < oi_floor(config)` → transition `VoidLowParticipation`, settle neutrally; votes < min AND OI ≥ floor → `AppError::NeedsCuratorDecision` (no silent auto-void; **ADR, grok-p1r1 M3: the D21 "one voting-window extension" is deliberately NOT built in Phase 1 — the curator path is admin resolve/void via the admin endpoints; `ExtendCloseOnce` ships with the curation dashboard in Phase 5 and is recorded in the Phase 2+ backlog**); else settle at `redemptions(actual_bps)`; **the void path REUSES `settle_market(holdings, 5_000, escrow)` verbatim** — full holdings including pool inventory, identical conservation + dust rules, dust → fees (grok-p1r1 M7); holdings MUST include pool inventory (domain returns `HoldingsIncomplete` otherwise — propagate, don't mask); payout ledger txn key `resolve:<market_id>` (idempotent replay returns without rewriting); vote_scores written via `domain::scoring::score_vote`; states walk `Closed→Resolving→Resolved→Paid` (or `→Voided`); emits `MarketResolved`/`MarketVoided`. CreditDeposit — `chain_sig` replay returns original deposit (dedupe), ledger `Deposit` external→user, event `DepositCredited`. **SeedMarket (grok-p1r1 B3 — new use case, admin-side):** creates market + outcome rows, `create_pool` (pools row + pool ledger account), executes the SEED ledger txn (house −S, escrow +S), **mints the pool inventory**: `save_reserves(Pool::new(MicroShares(S), MicroShares(S), fee_bps))` — S micro-USD of collateral backs S complete sets held by the pool — emits `MarketSeeded`, and leaves the market in `Scheduled` per the lifecycle contract below. `seed.rs` and the admin API call this use case; raw SQL for money or reserves is banned everywhere.

- [x] Steps: failing tests → fail run → implement → green + coverage ≥90 → **Commit** `feat(application): CastVote, ResolveMarket with void branch, CreditDeposit`

---

### Task 1.3: `adapters::pg` — SQLx implementations passing the same contracts

**Files:**
- Create: `crates/adapters/Cargo.toml`, `crates/adapters/src/lib.rs`, `crates/adapters/src/pg/mod.rs`, `crates/adapters/src/pg/store.rs`, `crates/adapters/src/pg/trade_tx.rs`, `crates/adapters/src/pg/vote_tx.rs`, `crates/adapters/src/pg/resolve_tx.rs`, `crates/adapters/src/pg/deposit_tx.rs`, `crates/adapters/src/pg/rows.rs`, `crates/adapters/tests/pg_contract.rs`, `migrations/0003_accounts_identity.sql`
- Modify: root `Cargo.toml` (members += `crates/adapters`)

Migration 0003 (codex M1 — account identity is load-bearing for get-or-create and reconciliation):

```sql
-- one account per (owner, currency) for owned classes; singletons for the rest
create unique index ledger_accounts_owned_uk on ledger_accounts (owner_type, owner_id, currency)
  where owner_type in ('user','pool','escrow');
create unique index ledger_accounts_singleton_uk on ledger_accounts (owner_type, currency)
  where owner_type in ('fees','house');   -- external already has its partial unique from 0002

-- ownership shape: NULL owner_id would make the owned-class unique index vacuous
-- (PostgreSQL NULLs are distinct — codex P1R2 M1)
alter table ledger_accounts add constraint ledger_accounts_owner_shape check (
  (owner_type in ('user','pool','escrow') and owner_id is not null)
  or (owner_type in ('fees','house','external') and owner_id is null)
);

-- account identity is immutable: reclassification would silently re-currency history
create or replace function forbid_account_reclass() returns trigger language plpgsql as $$
begin
  if new.owner_type <> old.owner_type or new.owner_id is distinct from old.owner_id
     or new.currency <> old.currency then
    raise exception 'ledger_accounts identity is immutable';
  end if;
  return new;
end $$;
create trigger ledger_accounts_immutable before update on ledger_accounts
  for each row execute function forbid_account_reclass();

-- converse identity: phone -> user (codex B6)
create table user_channels (
  user_id uuid not null references users(id),
  channel text not null,
  address text not null,
  created_at timestamptz not null default now(),
  unique (channel, address)
);
```

**Interfaces:**
- Consumes: every port + `contract.rs` suites from Task 1.1/1.2 — **the Postgres tests are literally the same generic functions the fakes passed** (`pg_contract.rs` instantiates them with a `PgStore` factory; `#[ignore]`-free but gated: tests early-return with a skip note when `DATABASE_URL` is unset, and CI always sets it).
- Produces: `PgStore::connect(url) -> Result<PgStore>`; each `*Tx` wraps one `sqlx::Transaction<'_, Postgres>`; `pool_for_update` = `SELECT ... FOR UPDATE`; `allocate_vote_seq` = `UPDATE markets SET vote_seq_counter = vote_seq_counter + 1 WHERE id = $1 RETURNING vote_seq_counter`; `ledger_apply` inserts header + entries and maps unique-violation on `idempotency_key` to `StoreError::DuplicateKey`; `append` writes `events_outbox`.

**Adapter-specific tests (beyond the shared contracts):** deferred-trigger interplay — an intentionally unbalanced `ledger_apply` followed by `commit()` surfaces the trigger error as `StoreError::Integrity` (proving the DB backstop is reachable, not theoretical); two concurrent `allocate_vote_seq` on one market yield distinct consecutive numbers; `pool_for_update` blocks a second locker until commit.

- [x] Steps: contract instantiation failing (crate skeleton) → implement per port → `just infra-up && sqlx migrate run` (0001+0002+0003 on port 15434) → `cargo test -p adapters` green locally with `DATABASE_URL` → append `cargo llvm-cov -p adapters --fail-under-lines 90` to the justfile `coverage` recipe and run it (with DB up) → **Commit** `feat(adapters): Postgres store passing the application contract suites`

---

### Task 1.4: `adapters::http` — Axum + OpenAPI (contract for the Python layer)

**Files:**
- Create: `crates/adapters/src/http/mod.rs`, `crates/adapters/src/http/routes.rs`, `crates/adapters/src/http/dto.rs`, `crates/adapters/src/http/error.rs`, `crates/adapters/tests/http_routes.rs`, `openapi.json` (generated, committed), `scripts/gen_openapi.sh`, `scripts/gen_api_models.sh`
- Modify: `justfile` (recipes `openapi`, `api-models`), `.github/workflows/ci.yml` (freshness check step)

**Interfaces:**
- Consumes: use cases from 1.1/1.2, generic over `S: Store + 'static` via `AppState<S>`.
- Produces routes (all utoipa-annotated, DTOs in `dto.rs` mirror application models — **no domain type serializes directly**):
  - `GET /healthz` · `GET /markets?status=live` · `GET /markets/{id_or_slug}` (detail incl. `price_yes_micro/price_no_micro` via `domain::amm::price_micro`)
  - `POST /trades/preview` `{user_id, market_ref, side, action, amount_micro}` → preview DTO (numbers the composer will inject)
  - `POST /trades` = preview shape + `idempotency_key`, optional `run_id`, `pending_action_id` → `TradeReceipt` DTO (`replayed` included)
  - `POST /votes` `{user_id, market_ref, side, crowd_guess_pct, idempotency_key, run_id?}` → vote receipt with public `seq`
  - `GET /users/{id}/positions` → portfolio view
  - `POST /admin/markets/{id}/advance` `{event}` + `POST /admin/markets/{id}/resolve` — gated by `x-admin-token` header == `ADMIN_TOKEN` env (Phase 1 curation stand-in)
  - `GET /openapi.json` — and offline generation that needs **no running server** (codex M6): `crates/adapters/examples/gen_openapi.rs` prints the utoipa doc; `scripts/gen_openapi.sh` = `cargo run -p adapters --example gen_openapi | jq -S . > openapi.json` (sorted keys — grok m2). **The generator pin lands in THIS task** (P1R2 M6 ordering): Task 1.4 modifies `services/converse/pyproject.toml` adding exact-pinned `datamodel-code-generator` to dev deps + `uv lock`; `scripts/gen_api_models.sh` invokes it through the project env — `uv run --project services/converse datamodel-code-generator --input openapi.json --input-file-type openapi --disable-timestamp --output services/converse/src/converse/api_models.py`. CI: install uv, `uv sync --project services/converse --extra dev` (the generator is a dev extra — plain sync won't install it), regenerate **both** artifacts, fail on `git diff --exit-code openapi.json services/converse/src/converse/api_models.py` (closes D15: the Python layer structurally cannot drift). CI's sqlx-cli install is exact-version pinned like every other tool.
  - `GET /users/by-channel?channel=imessage&address=+1555…` → `{user_id}` (converse identity lookup; `MarketQueries::user_by_channel`)
- Error contract: one `ApiError` envelope `{code, message}` with correct statuses (404 unknown market, 409 `AlreadyVoted`/duplicate-idempotency conflicts that aren't replays, 422 validation, 423 `TradingFrozen`, 402 `InsufficientFunds`).
- **Authentication posture, stated (grok-p1r1 M2):** Phase 1 has **no real authn** — `user_id` is trusted dev identity. Locked mitigations: default bind `127.0.0.1` (serving on `0.0.0.0` requires an explicit `BIND_ADDR` and logs a warning), every write route (`/trades*`, `/votes`) requires `x-demo-token == DEMO_TOKEN` env exactly like the admin routes, and the deferred-list entry names real authn (embedded-wallet identity) as Phase 2 scope. A tester texting via converse never sees this — converse holds the token.

**Tests** (`http_routes.rs`, tower `oneshot` against `AppState<InMemoryStore>` — fakes make HTTP tests hermetic): preview happy path returns the same numbers the use case computes; `POST /trades` without prior vote → 4xx envelope `VoteRequired`; replayed key → 200 with `replayed: true`; admin route without token → 401; `openapi.json` contains every route above (assert paths set).

- [x] Steps: failing route tests → implement → green → regen `openapi.json` + `api-models` → **Commit** `feat(http): axum routes with OpenAPI contract and generated python models`

---

### Task 1.5: `main` wiring + seed + Python client + THE E2E DEMO

**Files:**
- Create: `crates/main/Cargo.toml`, `crates/main/src/main.rs` (serve), `crates/main/src/seed.rs` (`--seed-demo` flag), `services/converse/src/converse/core_client.py`, `services/converse/src/converse/pg_stores.py`, `scripts/e2e_demo.sh`
- Modify: root `Cargo.toml` (members += `crates/main`), `services/converse/src/converse/graph.py` + `app.py` (preview/execute nodes call the core; production wiring below), `services/converse/tests/test_graph.py` (client injected as fake), `justfile` (`run-core`, `e2e`) — the `datamodel-code-generator` pin is owned by Task 1.4, not here (P1R2 addendum)

**Converse production wiring (codex B6 — the demo must run on the real stores, not the unit-test doubles):** `pg_stores.py` implements against `migrations/0001+0003`: `PgRecorder` (already spec'd in Task 8/Phase 0 — now the app default when `DATABASE_URL` is set), `PgPendingStore` (the expiry-sweep + select + consume-after-success protocol, one active row per thread), `AsyncPostgresSaver` checkpointer, webhook dedupe against `agent_runs (channel, inbound_msg_id)`, and the **per-thread `pg_advisory_xact_lock` turn serialization** from spec §7.5. Identity: `load_session` resolves phone → `user_by_channel("imessage", phone)` via the core API (`GET /users/by-channel` added to Task 1.4 routes) — unknown numbers get the onboarding reply, no auto-created accounts. The demo's deterministic extractor (regex `buy \$(\d+) of (yes|no) on ([a-z0-9-]+)`) is injected through the same `AgentNode` seam as any model — and the **malicious-router suite re-runs against this production wiring** (Pg stores + real core client pointed at a mock HTTP server): the poisoned router must still never reach execute.

**Interfaces:**
- `main`: reads `DATABASE_URL`, `BIND_ADDR` (default `127.0.0.1:8080`), `ADMIN_TOKEN`; constructs `PgStore`, runs pending migrations, serves `http::router(AppState::new(store, SystemClock))`. **Wiring only — any logic in `main` is a review-blocker (the crate stays in the coverage exclusion allowlist with that justification).**
- `seed.rs --seed-demo`: via use cases only (raw SQL for money/markets/reserves is banned): `EnsureGenesis` → `CreateUser` (with `imessage`/`DEMO_PHONE` channel link) → `SeedMarket` (slug `demo-coffee`, `min_votes_to_resolve` from config, closes +2h, hidden window last 10m, $1,000 at 50/50) → `AdvanceMarket` to Live → `CreditDeposit` $50 fake sig → `CastVote` (the gate vote) — prints IDs. Idempotent end to end: re-running the seed replays every step.
- `core_client.py`: thin httpx wrapper typed by `api_models.py` (`preview_trade`, `place_trade`, `cast_vote`, `market_by_ref`, `positions`); injected into graph build (`build_graph(core=...)`) — unit tests keep a `FakeCore`; the real one is exercised by the e2e.
- Graph wiring: `trade_preview_stub` → real `core.preview_trade` (numbers into state for the composer), `execute_pending` → `core.place_trade(idempotency_key=pending_action_id, run_id=run_id)`; vote corridor analogous (`cast_vote` on confirm).
- **Consume-after-success (grok-p1r1 B1 — corridor integrity under faults):** the pending gate **selects** the active pending row but does NOT mark it consumed; `execute_pending` calls the core first and marks `consumed_at` **only after a 2xx**. On core failure/timeout the pending stays active and the reply says so — the user re-confirms, and because `idempotency_key = pending_action_id` is stable, a retry after a partial success replays the original receipt instead of double-trading. A crash between core 2xx and the consume write is equally safe: next confirm replays (`replayed: true`) then consumes. The per-thread advisory lock still serializes the whole turn, so select-without-consume cannot double-fire concurrently. Tests: core-500 leaves pending active and a second "yes" succeeds; duplicate confirm after crash-before-consume returns `replayed` and consumes; the expiry sweep still runs first.
- `scripts/e2e_demo.sh` (this is the "implementation working" proof; `set -euo pipefail`, cleanup trap):
  1. `just infra-up`; wait pg; drop/create db; migrate 0001+0002
  2. `cargo run -p main -- --seed-demo` → capture user/market
  3. start core (`cargo run -p main` &) and converse (`uv run uvicorn converse.app:app --port 8091` & with `CORE_API_URL`, `DATABASE_URL`) — wait on `/healthz`
  4. `curl POST :8091/webhooks/sendblue {"number": DEMO_PHONE, "content": "buy $5 of yes on demo-coffee", "message_id": "e2e-1"}` → assert reply contains a preview (jq: mentions shares + fee + "yes")
  5. same with `{"content": "yes", "message_id": "e2e-2"}` → assert executed reply
  6. `curl :8080/users/$USER/positions` → assert YES shares > 0
  7. psql asserts (grok-p1r1 M6 strengthened): `agent_runs` has 2 ok runs; `pending_actions` consumed (and consumed AFTER the trade's ledger txn exists); `trades.run_id/pending_action_id` populated; ledger balanced **per currency** (`select la.currency, sum(le.amount_micro) ... group by la.currency` → every row 0); **escrow invariant, defined:** for the demo market, `escrow_balance_micro == total_yes_shares_micro == total_no_shares_micro` where each total sums positions + pool reserve for that side (every micro-share pair is backed by exactly 1 micro-USD in escrow — this is complete-set collateralization made queryable)
  8. prints `E2E DEMO GREEN`
- [x] Steps: failing python unit tests for node wiring (FakeCore) → implement → green; build main; run `just e2e` until GREEN (fix whatever it surfaces — that's the point) → **Commit** `feat(main,converse): wired stack with green text-to-trade e2e demo`

---

### Task 1.6: Phase 1 exit check

Clean-clone `just ci` equivalent (fmt, clippy, tests, coverage gates incl. new crates, deps-check, gate-test) + fresh-db `just e2e` + docs sync: README status flips Phase 1 built, spec §13 progress notes, `docs/plans/phase1-core-write-path.md` checkboxes ticked, any interface drift ADR'd in decisions.md. Deviations from this plan require justification in the worker report. **Commit** `chore: phase 1 exit check — all gates green`.

---

## Deferred from Phase 1 (explicitly, so nothing silently drops)

Solana devnet *listener* (the `CreditDeposit` use case + port land above; the RPC-polling adapter is Phase 2 alongside withdrawals), **real authentication** (embedded-wallet identity — Phase 1 trusts `user_id` behind `DEMO_TOKEN` on localhost, grok-p1r1 M2), WS gateway/ticks, real gpt-4o-mini metaprompts + evals (converse still runs deterministic fakes; the seam is proven by e2e), outbox→NATS relay (rows accumulate durably; relay is Phase 2), curator dashboard + the D21 `ExtendCloseOnce` voting-window extension (admin endpoints stand in; ADR in Task 1.2), full malicious-router adversarial suite re-run with the real `core_client` wired but a poisoned router fake (mandatory in Task 1.5 tests — the e2e alone is not the corridor proof, grok-p1r1 M6/ckpt 8).

## Reviewer checkpoints (verify specifically)

1. **Transaction boundaries:** every use case = exactly one `*Tx`; no read-modify-write escapes the row locks (`pool_for_update`, vote counter). Any TOCTOU between `Store` read-model calls and the tx re-reads?
2. **ISP honesty:** are the role traits genuinely minimal, or is `TradeTx` a god-trait wearing supertraits? Would a new use case force unrelated fakes to grow?
3. **Idempotency semantics:** replay returns the original receipt — is `DuplicateKey` → load-original race-safe under two concurrent identical requests (unique violation then read committed row)?
4. **Trigger SQL:** per-currency deferred validation correct for entry sets touching 2+ currencies? Append-only trigger doesn't break legitimate migrations?
5. **Ledger mapping:** SELL mapping (escrow −proceeds, user +net, fees +fee) — confirm escrow always stays ≥ 0 given quote math; per-market escrow == pool value + user-share backing invariant stated and psql-asserted in e2e.
6. **OpenAPI freshness gate:** does the regenerate-and-diff approach actually fail on drift (serde/utoipa nondeterminism risks)?
7. **Coverage mechanics:** application/adapters gates with DB service — will `-p adapters --fail-under-lines 90` hold when integration tests are the main coverage source in CI?
8. **E2E honesty:** does the demo prove the corridor (lexical confirm, no LLM execute authority) or does the fake router shortcut it? The malicious-router test must still hold with the real client wired.
