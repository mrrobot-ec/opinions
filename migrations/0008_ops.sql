-- Phase 6 ops control plane: generation-serialized config, two-phase proposals,
-- admin audit, publication commands, unwind authority, receivables subledger.
-- Design authority: docs/plans/phase6-simswarm-ops.md D24/D26/D30 (revision 4).

-- ---------------------------------------------------------------------------
-- D24 — config store
-- ---------------------------------------------------------------------------

create table config_entries (
    key text primary key,
    value jsonb not null,
    updated_at timestamptz not null default now()
);

-- Singleton authority row: every config write locks this row FIRST, which
-- serializes commit order (outbox sequence is never the watermark).
create table config_generation (
    singleton integer primary key check (singleton = 1),
    generation bigint not null,
    updated_by text not null,
    updated_at timestamptz not null default now()
);
insert into config_generation (singleton, generation, updated_by) values (1, 1, 'migration:0008');

-- Immutable per-generation history: one header row per applied patch and one
-- change row PER CHANGED KEY (a multi-key patch shares one generation).
create table config_generations (
    generation bigint primary key,
    applied_by text not null,
    applied_at timestamptz not null default now()
);
insert into config_generations (generation, applied_by) values (1, 'migration:0008');

create table config_changes (
    generation bigint not null references config_generations(generation),
    key text not null,
    old jsonb,
    new jsonb not null,
    changed_by text not null,
    changed_at timestamptz not null default now(),
    primary key (generation, key)
);
create index config_changes_key_generation_idx on config_changes (key, generation);

-- Durable two-phase proposal authority for sensitive keys. Confirmation
-- requires a DIFFERENT token id than the proposer (dual control).
create table config_change_proposals (
    id uuid primary key default gen_random_uuid(),
    idempotency_key text not null unique,
    patch jsonb not null,
    patch_hash text not null,
    base_generation bigint not null,
    proposer_token_id text not null,
    proposer_role text not null,
    reason text not null,
    status text not null check (status in ('pending', 'confirmed', 'rejected', 'expired')),
    expires_at timestamptz not null,
    confirmer_token_id text,
    resulting_generation bigint references config_generations(generation),
    created_at timestamptz not null default now(),
    settled_at timestamptz,
    check (confirmer_token_id is null or confirmer_token_id <> proposer_token_id)
);
create index config_change_proposals_pending_idx
    on config_change_proposals (status, expires_at) where status = 'pending';

-- ---------------------------------------------------------------------------
-- D26 — admin audit (every admin mutation writes one row in its own tx)
-- ---------------------------------------------------------------------------

create table admin_actions (
    id uuid primary key default gen_random_uuid(),
    actor_role text not null,
    actor_token_digest text not null,  -- sha-256 digest; never the token
    action text not null,
    subject text not null,
    before jsonb,
    after jsonb,
    reason text,
    at timestamptz not null default now()
);
create index admin_actions_subject_idx on admin_actions (subject, at desc);

-- ---------------------------------------------------------------------------
-- D26 — durable publication commands (publish_now = authorize atomically,
-- the existing publisher saga executes; 202 + status URL)
-- ---------------------------------------------------------------------------

create table publication_commands (
    id uuid primary key default gen_random_uuid(),
    draft_id uuid not null references market_drafts(id),
    idempotency_key text not null unique,
    requested_by text not null,
    status text not null check (status in ('pending', 'executing', 'done', 'failed')),
    attempts integer not null default 0,
    lease_expires_at timestamptz,
    result_market_id uuid,
    error text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);
-- At most one OPEN command per draft; done/failed history is retained.
create unique index publication_commands_open_draft_idx
    on publication_commands (draft_id) where status in ('pending', 'executing');
create index publication_commands_due_idx
    on publication_commands (status, lease_expires_at) where status in ('pending', 'executing');

-- ---------------------------------------------------------------------------
-- D30 — unwind authority + non-cash receivables subledger
-- ---------------------------------------------------------------------------

create table market_unwinds (
    market_id uuid primary key references markets(id),
    unwind_key text not null unique,
    stage text not null check (stage in ('proposed', 'confirmed', 'applied', 'rejected', 'expired')),
    proposer_token_id text not null,
    confirmer_token_id text,
    reason text not null,
    proposed_at timestamptz not null default now(),
    confirm_not_before timestamptz not null,
    settled_at timestamptz,
    reversal_txn_id uuid,
    check (confirmer_token_id is null or confirmer_token_id <> proposer_token_id)
);

-- Reversal lineage: an original ledger entry can be reversed at most once.
create table ledger_entry_reversals (
    reversed_entry_id uuid primary key,
    reversal_txn_id uuid not null,
    market_id uuid not null references markets(id),
    created_at timestamptz not null default now()
);

-- Receivable authority: opened when an unwind reversal finds insufficient user
-- cash. NON-CASH facts — never a ledger account class; identity 3 stays cash.
create table receivables (
    id uuid primary key default gen_random_uuid(),
    market_id uuid not null references markets(id),
    user_id uuid not null references users(id),
    origin_reversal_txn_id uuid not null,
    opened_micro bigint not null check (opened_micro > 0),
    created_at timestamptz not null default now(),
    unique (user_id, market_id)
);

-- Append-only movements; outstanding is DERIVED (opened − collected − written_off).
create table receivable_movements (
    id uuid primary key default gen_random_uuid(),
    receivable_id uuid not null references receivables(id),
    kind text not null check (kind in ('opened', 'collected', 'written_off')),
    amount_micro bigint not null check (amount_micro > 0),
    actor text not null,
    audit_id uuid not null references admin_actions(id),
    cash_txn_id uuid,
    idempotency_key text not null unique,
    created_at timestamptz not null default now()
);
create index receivable_movements_receivable_idx on receivable_movements (receivable_id);

-- Write-off is a single-principal economic transfer → full dual-control grade.
create table receivable_write_off_proposals (
    id uuid primary key default gen_random_uuid(),
    receivable_id uuid not null references receivables(id),
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

-- ---------------------------------------------------------------------------
-- D27 — settlement fact for the payout identity
-- ---------------------------------------------------------------------------

alter table markets add column collateral_at_close_micro bigint;

-- ---------------------------------------------------------------------------
-- Outbox consumer cursor seed for the config reconciler (wake-ups only).
-- ---------------------------------------------------------------------------

insert into outbox_cursors (consumer, last_seq)
values ('config_reconciler', coalesce((select max(seq) from events_outbox), 0))
on conflict (consumer) do nothing;

-- ---------------------------------------------------------------------------
-- D24 catalog seeds — published values (docs/copy/scoring.md is the authority;
-- bounds/apply-to/role/max-delta live in the typed validator + docs/copy/ops.md).
-- ---------------------------------------------------------------------------

insert into config_entries (key, value) values
    ('trade_fee_bps',                '100'),
    ('min_fee_bps',                  '10'),
    ('discount_flip_window_secs',    '3600'),
    ('fee_discount_bp_by_tier',      '[0, 0, 10, 20, 30]'),
    ('position_cap_micro_by_tier',   '[25000000, 50000000, 100000000, 250000000, 500000000]'),
    ('rep_tier_thresholds_micro',    '[0, 200000, 400000, 600000, 800000]'),
    ('rep_score_min_pot_micro',      '50000000'),
    ('seed_micro_daily',             '500000000'),
    ('seed_micro_flash',             '100000000'),
    ('daily_seed_budget_micro',      '1000000000'),
    ('hidden_window_secs',           '300'),
    ('min_votes_to_resolve_floor',   '3'),
    ('oi_floor_micro',               '0'),
    ('payout_hold_threshold_micro',  '500000000'),
    ('sweep_delay_secs',             '180'),
    ('max_votes_per_window',         '30'),
    ('vote_window_secs',             '3600'),
    ('integrity_burst_multiplier_ppm',      '2000000'),
    ('integrity_young_share_max_ppm',       '500000'),
    ('integrity_device_share_max_ppm',      '600000'),
    ('integrity_subnet_share_max_ppm',      '600000'),
    ('integrity_min_metadata_coverage_ppm', '500000'),
    ('flash_cadence_secs',           '3600'),
    ('daily_slots',                  '2'),
    ('feature_flash_markets',        'true'),
    ('feature_comments',             'true'),
    ('feature_referrals',            'false'),
    ('trading_paused',               'false'),
    ('faucet_per_call_cap_micro',    '1000000000'),
    ('remedial_credit_market_cap_micro', '500000000'),
    ('remedial_credit_daily_cap_micro',  '2000000000'),
    ('receivable_outstanding_cap_micro', '10000000000'),
    ('writeoff_per_item_cap_micro',  '500000000'),
    ('writeoff_daily_cap_micro',     '2000000000'),
    ('proposal_ttl_secs',            '900'),
    ('dual_control_delay_secs',      '60');

insert into config_changes (generation, key, old, new, changed_by)
select 1, key, null, value, 'migration:0008' from config_entries;
