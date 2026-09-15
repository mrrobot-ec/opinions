alter table votes add column cast_ip inet;
alter table votes add column device_hash text;
alter table vote_scores add column created_at timestamptz not null default now();

alter table markets add column lp_pnl_micro bigint;
alter table markets add column settled_at timestamptz;
alter table markets add column integrity_due_at timestamptz;
create index markets_settled_idx on markets (settled_at) where lp_pnl_micro is not null;

insert into reputation (user_id, rep_micro, tier, updated_at)
  select id, 0, 0, now() from users
  on conflict (user_id) do nothing;

create table realizations (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  market_id uuid not null references markets(id),
  outcome_id uuid not null references outcomes(id),
  source text not null check (source in ('sell','settlement','void')),
  realized_delta_micro bigint not null,
  txn_id uuid not null references ledger_transactions(id),
  created_at timestamptz not null default now(),
  unique (txn_id, user_id, outcome_id),
  foreign key (outcome_id, market_id) references outcomes (id, market_id)
);
create index realizations_window_idx on realizations (created_at, user_id);

create table integrity_reports (
  id uuid primary key default gen_random_uuid(),
  market_id uuid not null references markets(id) unique,
  checks jsonb not null,
  verdict text not null check (verdict in ('pass','flag')),
  created_at timestamptz not null default now()
);

create index vote_scores_time_idx on vote_scores (created_at);
create index votes_market_created_idx on votes (market_id, created_at);
create index ledger_txn_kind_time_idx on ledger_transactions (kind, created_at, id);
create index ledger_entries_account_time_idx on ledger_entries (account_id, created_at);
create index markets_review_due_idx on markets (integrity_due_at)
  where status = 'resolving' and curator_flagged_at is null;
