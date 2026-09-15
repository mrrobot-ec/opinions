-- Phase 6 wave W2: dual-control remedial credit proposals and durable
-- manual-ops (replay/refanout) commands. Plan D26/D30; unwind-grade control
-- rows mirror receivable_write_off_proposals in 0008.

create table remedial_credit_proposals (
    id uuid primary key default gen_random_uuid(),
    market_id uuid not null references markets(id),
    user_id uuid not null references users(id),
    idempotency_key text not null unique,
    amount_micro bigint not null check (amount_micro > 0),
    proposer_token_id text not null,
    confirmer_token_id text,
    reason text not null,
    status text not null check (status in ('pending', 'confirmed', 'rejected', 'expired')),
    confirm_not_before timestamptz not null,
    created_at timestamptz not null default now(),
    settled_at timestamptz,
    check (confirmer_token_id is null or confirmer_token_id <> proposer_token_id)
);
create index remedial_credit_proposals_market_idx
    on remedial_credit_proposals (market_id, status);

create table ops_job_commands (
    id uuid primary key default gen_random_uuid(),
    kind text not null check (kind in ('replay_job', 'refanout')),
    subject text not null,
    idempotency_key text not null unique,
    requested_by text not null,
    status text not null check (status in ('pending', 'executing', 'done', 'failed')),
    attempts integer not null default 0,
    lease_expires_at timestamptz,
    error text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);
create index ops_job_commands_due_idx
    on ops_job_commands (status, lease_expires_at) where status in ('pending', 'executing');
