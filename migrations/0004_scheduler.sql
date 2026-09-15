alter table markets add column curator_flagged_at timestamptz;

create table lifecycle_commands (
  key text primary key,
  market_id uuid not null references markets(id),
  event text not null,
  resulting_state text not null,
  applied_at timestamptz not null default now()
);

create index markets_due_open_idx
  on markets (opens_at) where status = 'scheduled';
create index markets_due_freeze_idx
  on markets (tally_hidden_at) where status = 'live';
create index markets_due_close_idx
  on markets (closes_at) where status = 'closing';
create index markets_due_resolve_idx
  on markets (closes_at) where status = 'closed' and curator_flagged_at is null;
create index trades_market_time_idx on trades (market_id, created_at);
