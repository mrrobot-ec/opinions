-- Phase 5 content supply, durable artifact jobs, and moderation work queue.

create table market_drafts (
    id uuid primary key default gen_random_uuid(),
    source text not null check (source in ('template', 'llm')),
    fallback_from text check (fallback_from in ('llm')),
    tier text not null check (tier in ('daily', 'flash')),
    status text not null check (status in ('pending', 'approved', 'rejected', 'published', 'expired')),
    publish_stage text check (publish_stage in ('claimed', 'seeded', 'live', 'jobs_enqueued', 'published')),
    published_market_id uuid,  -- NO FK by design (coordinator re-manifest): the saga's
  -- 'claimed' stage commits this pre-generated id BEFORE SeedMarket creates the market
  -- row; referential convergence is owned by the saga + asserted in the exit e2e,
    question text not null,
    description text not null,
    video_script text not null,
    slug text not null,
    seed_micro bigint not null check (seed_micro > 0),
    fee_bps integer not null check (fee_bps between 0 and 10000),
    min_votes_to_resolve integer not null check (min_votes_to_resolve > 0),
    open_secs bigint not null check (open_secs > 0),
    hidden_window_secs bigint not null check (hidden_window_secs >= 0 and hidden_window_secs < open_secs),
    publish_at timestamptz,
    expires_at timestamptz not null,
    reviewed_by uuid references users(id),
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (tier, publish_at)
);

create index market_drafts_due_idx
    on market_drafts (publish_at, created_at, id)
    where status = 'approved';
create index market_drafts_expiry_idx
    on market_drafts (expires_at, id)
    where status = 'pending';

alter table video_jobs
    add column kind text not null default 'market_video',
    add column draft_id uuid references market_drafts(id),
    add column available_at timestamptz not null default now(),
    add column claim_token uuid,
    add column lease_expires_at timestamptz,
    add column attempts integer not null default 0 check (attempts >= 0),
    add column error text,
    add column updated_at timestamptz not null default now(),
    add constraint video_jobs_kind_check check (kind in ('market_video', 'poster')),
    add constraint video_jobs_status_check check (status in ('queued', 'rendering', 'ready', 'attached', 'failed'));

create unique index video_jobs_active_market_kind_uk
    on video_jobs (market_id, kind)
    where status in ('queued', 'rendering', 'ready');
create unique index video_jobs_draft_kind_uk
    on video_jobs (draft_id, kind)
    where draft_id is not null;
create index video_jobs_claim_idx
    on video_jobs (available_at, updated_at, id)
    where status = 'queued';
create index video_jobs_reclaim_idx
    on video_jobs (lease_expires_at, id)
    where status = 'rendering';

alter table markets
    add column poster_asset_url text,
    add column video_asset_url text;

create table moderation_jobs (
    id uuid primary key default gen_random_uuid(),
    comment_id uuid not null unique references comments(id),
    status text not null default 'queued'
        check (status in ('queued', 'running', 'done', 'failed')),
    available_at timestamptz not null default now(),
    claim_token uuid,
    lease_expires_at timestamptz,
    attempts integer not null default 0 check (attempts >= 0),
    error text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index moderation_jobs_claim_idx
    on moderation_jobs (available_at, updated_at, id)
    where status = 'queued';
create index moderation_jobs_reclaim_idx
    on moderation_jobs (lease_expires_at, id)
    where status = 'running';

insert into outbox_cursors (consumer, last_seq)
values ('moderation', 0)
on conflict (consumer) do nothing;
