-- Schema v1 — ER contract (docs/er-diagram.mermaid) + PLAN Task 7 critical DDL.
-- Order: users → markets → outcomes → pools → pool_reserves → ledger_* →
--        agent_runs → pending_actions → trades/votes → remaining ER tables.
-- Requires PostgreSQL 13+ (gen_random_uuid in core).

create extension if not exists pgcrypto;

-- ---------------------------------------------------------------------------
-- Identity
-- ---------------------------------------------------------------------------

create table users (
  id uuid primary key default gen_random_uuid(),
  handle text not null unique,
  kyc_tier int not null default 0,
  created_at timestamptz not null default now()
);

create table wallets (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  chain text not null,
  address text not null unique
);

create table reputation (
  user_id uuid primary key references users(id),
  rep_micro bigint not null default 0,
  tier int not null default 0,
  updated_at timestamptz not null default now()
);

-- ---------------------------------------------------------------------------
-- Markets / outcomes / pools
-- ---------------------------------------------------------------------------

create table markets (
  id uuid primary key default gen_random_uuid(),
  slug text not null unique,
  question text not null,
  status text not null check (status in
    ('draft','scheduled','live','closing','closed','resolving','resolved','paid','voided')),
  -- R1: locked allocation for public vote numbers (update ... returning in the vote txn)
  vote_seq_counter bigint not null default 0,
  -- R1 (D21) + R2 (codex M4): must be configured positive before GoLive — no zero default
  min_votes_to_resolve int not null check (min_votes_to_resolve > 0),
  opens_at timestamptz,
  closes_at timestamptz,
  tally_hidden_at timestamptz,
  created_at timestamptz not null default now()
);

create table outcomes (
  id uuid primary key default gen_random_uuid(),
  market_id uuid not null references markets(id),
  label text not null,
  idx int not null,
  final_vote_bps int,
  redemption_micro bigint,
  unique (market_id, idx),         -- R1 (codex M3): outcome identity within a market
  unique (id, market_id)           -- R2 (codex M2): composite FK target proving market agreement
);

create table pools (
  id uuid primary key default gen_random_uuid(),
  market_id uuid not null unique references markets(id),
  fee_bps int not null,
  seeded_micro bigint not null check (seeded_micro >= 0),
  -- R2 (codex M2): composite FK target for pool_reserves
  unique (id, market_id)
);

create table pool_reserves (
  pool_id uuid not null,
  outcome_id uuid not null,
  market_id uuid not null,
  reserve_micro_shares bigint not null check (reserve_micro_shares >= 0),
  primary key (pool_id, outcome_id),
  foreign key (pool_id, market_id) references pools (id, market_id),
  foreign key (outcome_id, market_id) references outcomes (id, market_id)
);

-- ---------------------------------------------------------------------------
-- Ledger
-- ---------------------------------------------------------------------------

create table ledger_accounts (
  id uuid primary key default gen_random_uuid(),
  -- R1: 'external' is the contra class for the outside world (may go negative)
  owner_type text not null check (owner_type in ('user','pool','fees','house','escrow','external')),
  owner_id uuid,
  -- R1: cash and non-withdrawable bonus credits are separate accounts from day one
  currency text not null default 'usdc' check (currency in ('usdc','usdc_credit')),
  created_at timestamptz not null default now()
);
-- R3 (codex M1): the domain's DuplicateExternal rule, mirrored in SQL — reconciliation
-- breaks if two contra accounts exist for one currency.
create unique index ledger_accounts_one_external_per_currency
  on ledger_accounts (currency) where owner_type = 'external';

create table ledger_transactions (
  id uuid primary key default gen_random_uuid(),
  kind text not null check (kind in
    ('deposit','trade','payout','withdrawal','seed','reversal','credit_grant','credit_convert')),
  idempotency_key text not null unique,
  created_at timestamptz not null default now()
);

create table ledger_entries (
  id bigint generated always as identity primary key,
  txn_id uuid not null references ledger_transactions(id),
  account_id uuid not null references ledger_accounts(id),
  amount_micro bigint not null check (amount_micro <> 0),
  created_at timestamptz not null default now()
);
create index ledger_entries_account_idx on ledger_entries (account_id, id);

-- R1 (codex M5): sum-to-zero cannot be a row CHECK (cross-row). Phase 0: enforced by
-- domain::ledger (the only writer is the test suite). A DEFERRED CONSTRAINT TRIGGER is a
-- BLOCKING precondition for merging any Phase-1 write adapter — and it must validate
-- sums GROUPED BY (txn_id, account currency), not the transaction-wide scalar
-- (R3/codex M1: an all-currencies scalar sum would readmit credit→cash conversion),
-- and reject a transaction header committed with zero entries.

-- ---------------------------------------------------------------------------
-- Agent / conversation audit (unpartitioned; retention via scheduled jobs)
-- ---------------------------------------------------------------------------

create table agent_runs (
  id uuid primary key,
  thread_id text not null,
  channel text not null,                 -- R1: dedupe scope
  trigger text not null,
  user_id uuid references users(id),
  inbound_msg_id text,
  final_intent text,
  status text not null check (status in ('running','ok','error','guard_blocked')),
  total_tokens int,
  cost_micro bigint,
  trace_id text,
  started_at timestamptz not null,
  ended_at timestamptz,
  -- R1 (codex m4): webhook redelivery can never double-run a message
  unique (channel, inbound_msg_id)
);
create index agent_runs_thread_idx on agent_runs (thread_id, started_at);
create index agent_runs_started_idx on agent_runs (started_at);
-- R2 (codex M6): partitioning is deliberately NOT promised — PostgreSQL partitioned
-- unique keys must include the partition key, which would break PK(id) and the global
-- (channel, inbound_msg_id) dedupe. Decision: these tables stay UNPARTITIONED with
-- scheduled retention jobs (batched DELETE by started_at + archival copy + PII
-- redaction per data class) — policy in spec §10.3.

create table agent_steps (
  id uuid primary key,
  run_id uuid not null references agent_runs(id),
  seq int not null,
  node text not null,
  kind text not null check (kind in ('llm','tool','gate')),
  state_before jsonb,
  state_after jsonb,
  prompt_template_id text,
  prompt_version text,
  rendered_prompt text,
  model text,
  params jsonb,
  raw_output text,
  parsed_output jsonb,
  rationale text,
  tokens_in int,
  tokens_out int,
  latency_ms int,
  error text,
  unique (run_id, seq)
);

create table pending_actions (
  id uuid primary key default gen_random_uuid(),
  thread_id text not null,
  run_id uuid not null references agent_runs(id),        -- R2 (codex M3): FK made explicit
  kind text not null check (kind in ('trade','vote')),   -- R1: votes are in the corridor
  payload jsonb not null,
  expires_at timestamptz not null,
  consumed_at timestamptz,
  created_at timestamptz not null default now()
);
-- R2 (codex B3): at most ONE active pending action per thread, enforced by the database —
-- two replicas cannot both hold an active preview for the same phone.
create unique index pending_actions_one_active_per_thread
  on pending_actions (thread_id) where consumed_at is null;
-- R2 (codex B3) + R3 (codex B1): protocol, always inside the per-thread advisory-lock
-- transaction (pg_advisory_xact_lock(hashtext(thread_id))):
--   1. EXPIRY SWEEP (prevents the expired-row deadlock — an expired unconsumed row
--      would otherwise block the partial unique index forever):
--        update pending_actions set consumed_at = now()
--          where thread_id = $1 and consumed_at is null and expires_at <= now();
--   2. CONFIRM consume:
--        update pending_actions set consumed_at = now()
--          where id = $2 and consumed_at is null and expires_at > now()
--          returning id, kind, payload;
--      zero rows = lost the race or expired → reply "nothing pending".
--   3. CREATE pending: plain insert (safe: sweep ran, index enforces one-active).
-- Required tests (Task 8 integration + Phase 1): expiry then new preview on the same
-- thread succeeds; two concurrent connections racing confirm → exactly one consume;
-- two concurrent previews on one thread → exactly one active row.

-- ---------------------------------------------------------------------------
-- Trading / voting / positions
-- ---------------------------------------------------------------------------

create table trades (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  market_id uuid not null,
  outcome_id uuid not null,
  -- R2 (codex M2): the composite FK makes a cross-market (outcome, market) pair unrepresentable
  foreign key (outcome_id, market_id) references outcomes (id, market_id),
  run_id uuid references agent_runs(id),                       -- R1: agent causal chain
  pending_action_id uuid unique references pending_actions(id), -- R2 (codex M3): 1:1 as ER shows
  txn_id uuid references ledger_transactions(id),               -- R2 (codex M3): trade → ledger
  side text not null check (side in ('buy','sell')),
  collateral_micro bigint not null check (collateral_micro > 0),
  shares_micro bigint not null check (shares_micro > 0),
  fee_micro bigint not null check (fee_micro >= 0),
  seq bigint not null,
  created_at timestamptz not null default now(),
  unique (market_id, seq)
);

create table votes (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  market_id uuid not null,
  outcome_id uuid not null,
  foreign key (outcome_id, market_id) references outcomes (id, market_id), -- R2 (codex M2)
  run_id uuid references agent_runs(id),                        -- R1: agent causal chain
  pending_action_id uuid unique references pending_actions(id), -- R2 (grok M5): oracle chain parity
  crowd_guess_pct int not null check (crowd_guess_pct between 0 and 100),
  seq bigint not null,
  idempotency_key text not null unique,             -- R1 (codex M4): retry-safe casts
  created_at timestamptz not null default now(),
  unique (user_id, market_id),
  unique (market_id, seq)
);
-- R1 (codex M4/grok m2): seq is ALLOCATED, not raced: the Phase-1 cast-vote use case runs
--   update markets set vote_seq_counter = vote_seq_counter + 1
--     where id = $1 returning vote_seq_counter
-- in the same transaction as the vote insert (row lock serializes; idempotency_key
-- makes network retries return the original vote instead of a second number).

create table vote_scores (
  vote_id uuid primary key references votes(id),
  accuracy_bp int not null check (accuracy_bp between 0 and 10000),
  majority_bp int not null check (majority_bp between 0 and 10000),
  score_bp int not null check (score_bp between 0 and 10000)
);

create table positions (
  user_id uuid not null references users(id),
  outcome_id uuid not null references outcomes(id),
  shares_micro bigint not null check (shares_micro >= 0),
  cost_micro bigint not null,
  realized_pnl_micro bigint not null default 0,
  primary key (user_id, outcome_id)
);

-- ---------------------------------------------------------------------------
-- Social / notifications / payments / content / outbox
-- ---------------------------------------------------------------------------

create table comments (
  id uuid primary key default gen_random_uuid(),
  market_id uuid not null references markets(id),
  user_id uuid not null references users(id),
  parent_id uuid references comments(id),
  body text not null,
  score int not null default 0,
  moderation_status text not null default 'visible',
  created_at timestamptz not null default now()
);

create table comment_votes (
  comment_id uuid not null references comments(id),
  user_id uuid not null references users(id),
  value smallint not null check (value in (-1, 1)),
  primary key (comment_id, user_id)
);

create table notifications (
  id bigint generated always as identity primary key,
  user_id uuid not null references users(id),
  type text not null,
  market_id uuid references markets(id),
  payload jsonb not null,
  read_at timestamptz,
  created_at timestamptz not null default now()
);

create table deposits (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  chain_sig text not null unique,
  amount_micro bigint not null check (amount_micro > 0),
  status text not null check (status in ('seen','confirmed','credited')),
  txn_id uuid references ledger_transactions(id),
  created_at timestamptz not null default now()
);

create table withdrawals (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  dest_address text not null,
  amount_micro bigint not null check (amount_micro > 0),
  status text not null check (status in ('queued','risk_hold','sent','settled')),
  chain_sig text,
  txn_id uuid references ledger_transactions(id),
  created_at timestamptz not null default now()
);

create table video_jobs (
  id uuid primary key default gen_random_uuid(),
  market_id uuid not null references markets(id),
  tier text not null check (tier in ('hero','template')),
  status text not null,
  asset_url text,
  created_at timestamptz not null default now()
);

create table events_outbox (
  seq bigint generated always as identity primary key,
  aggregate_type text not null,
  aggregate_id uuid not null,
  event_type text not null,
  payload jsonb not null,
  published_at timestamptz,  -- null until relayed; retained after relay (append-only event log)
  created_at timestamptz not null default now()
);
create index events_outbox_unpublished_idx
  on events_outbox (seq) where published_at is null;
