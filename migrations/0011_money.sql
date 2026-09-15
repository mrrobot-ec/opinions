-- Phase 7 money/compliance authority (plan revision 3.3 / 7.0a).
-- CHECKs on withdrawals derive from docs/copy/ops.md "Withdrawal combination table".

-- ---------------------------------------------------------------------------
-- Ledger owner types: withheld / deposit_suspense / bonus_reserve
-- ---------------------------------------------------------------------------
alter table ledger_accounts drop constraint ledger_accounts_owner_shape;
drop index if exists ledger_accounts_singleton_uk;

do $$
declare r record;
begin
  for r in
    select conname from pg_constraint
     where conrelid = 'ledger_accounts'::regclass
       and contype = 'c'
       and pg_get_constraintdef(oid) ilike '%owner_type%'
  loop
    execute format('alter table ledger_accounts drop constraint %I', r.conname);
  end loop;
end $$;
alter table ledger_accounts add constraint ledger_accounts_owner_type_check
  check (owner_type in (
    'user','pool','fees','house','escrow','external',
    'withheld','deposit_suspense','bonus_reserve'
  ));

alter table ledger_accounts add constraint ledger_accounts_owner_shape check (
  (owner_type in ('user','pool','escrow') and owner_id is not null)
  or (owner_type in ('fees','house','external','withheld','deposit_suspense','bonus_reserve')
      and owner_id is null)
);

create unique index ledger_accounts_singleton_uk on ledger_accounts (owner_type, currency)
  where owner_type in ('fees','house','withheld','deposit_suspense','bonus_reserve');

-- ---------------------------------------------------------------------------
-- Users: status
-- ---------------------------------------------------------------------------
alter table users add column if not exists status text not null default 'active';
alter table users drop constraint if exists users_status_check;
alter table users add constraint users_status_check
  check (status in ('active','shadow_limited','banned'));

-- ---------------------------------------------------------------------------
-- Withdrawals: three dimensions + tx ids (ops.md W1–W15)
-- ---------------------------------------------------------------------------
alter table withdrawals add column if not exists review_state text;
alter table withdrawals add column if not exists send_state text;
alter table withdrawals add column if not exists hold_tx_id uuid references ledger_transactions(id);
alter table withdrawals add column if not exists release_tx_id uuid references ledger_transactions(id);
alter table withdrawals add column if not exists settle_tx_id uuid references ledger_transactions(id);
alter table withdrawals add column if not exists request_fingerprint text;
alter table withdrawals add column if not exists risk_reasons jsonb not null default '[]'::jsonb;
alter table withdrawals add column if not exists requested_at timestamptz not null default now();
alter table withdrawals add column if not exists decided_at timestamptz;
alter table withdrawals add column if not exists sent_at timestamptz;
alter table withdrawals add column if not exists settled_at timestamptz;

-- Quarantine legacy rows that cannot be reconstructed as Withheld-shaped.
create table withdrawals_legacy_quarantine (
  id uuid primary key,
  user_id uuid,
  dest_address text,
  amount_micro bigint,
  status text,
  chain_sig text,
  txn_id uuid,
  reason text not null,
  quarantined_at timestamptz not null default now()
);

insert into withdrawals_legacy_quarantine
  (id, user_id, dest_address, amount_micro, status, chain_sig, txn_id, reason)
select id, user_id, dest_address, amount_micro, status, chain_sig, txn_id,
       'pre-withheld 0001 row; do not invent User↔External as hold'
  from withdrawals
 where hold_tx_id is null;

delete from withdrawals where hold_tx_id is null;

alter table withdrawals drop constraint if exists withdrawals_status_check;
alter table withdrawals add constraint withdrawals_status_check
  check (status in ('queued','risk_hold','sent','settled','denied','failed'));
alter table withdrawals alter column review_state set not null;
alter table withdrawals alter column send_state set not null;
alter table withdrawals alter column hold_tx_id set not null;

alter table withdrawals add constraint withdrawals_combination_check check (
  (status, review_state, send_state) in (
    ('queued','screening','unsent'),
    ('queued','approved','unsent'),
    ('queued','approved','sending'),
    ('risk_hold','review_required','unsent'),
    ('risk_hold','approval_proposed','unsent'),
    ('sent','approved','broadcast'),
    ('sent','approved','unknown'),
    ('sent','approved','sending'),
    ('sent','approved','finalized'),
    ('settled','approved','finalized'),
    ('denied','screening','unsent'),
    ('denied','review_required','unsent'),
    ('denied','approval_proposed','unsent'),
    ('denied','approved','unsent'),
    ('failed','approved','definitive_failed')
  )
  and not (release_tx_id is not null and settle_tx_id is not null)
  and (status = 'settled') = (settle_tx_id is not null)
  and (status in ('denied','failed')) = (release_tx_id is not null)
);

create table withdrawal_events (
  id bigint generated always as identity primary key,
  withdrawal_id uuid not null references withdrawals(id),
  kind text not null,
  actor text not null,
  audit_id uuid,
  at timestamptz not null default now(),
  payload jsonb not null default '{}'::jsonb
);
create index withdrawal_events_withdrawal_idx on withdrawal_events (withdrawal_id, id);

-- ---------------------------------------------------------------------------
-- Deposits machine
-- ---------------------------------------------------------------------------
alter table deposits alter column user_id drop not null;
alter table deposits add column if not exists source_address text;
alter table deposits add column if not exists dest_address text;
alter table deposits add column if not exists mint text;
alter table deposits add column if not exists observed_slot bigint;
alter table deposits add column if not exists rail_fingerprint text;
alter table deposits add column if not exists suspense_tx_id uuid references ledger_transactions(id);
alter table deposits add column if not exists admit_tx_id uuid references ledger_transactions(id);
alter table deposits add column if not exists refund_tx_id uuid references ledger_transactions(id);
alter table deposits add column if not exists machine_status text;

update deposits set machine_status = 'admitted_legacy', admit_tx_id = txn_id
 where status = 'credited';
-- Pre-0011 seen/confirmed rows never recorded source/dest/mint/slot, so their
-- observation identity is NOT re-derivable in-migration: quarantine them
-- (operator-only) instead of promoting them to machine liabilities. The
-- machine decoder must exclude 'quarantined_legacy' rows entirely.
update deposits set machine_status = 'quarantined_legacy'
 where status in ('seen','confirmed') and machine_status is null;

alter table deposits drop constraint if exists deposits_status_check;
alter table deposits add constraint deposits_status_check check (
  status in (
    'seen','confirmed','credited',
    'observed_finalized','admission_pending','admitted','admitted_legacy',
    'compliance_hold','refund_approved','refund_sending','refunded'
  )
);
-- New rows should write the machine statuses; legacy credited stays admitted_legacy.
alter table deposits add constraint deposits_admit_xor_refund check (
  not (admit_tx_id is not null and refund_tx_id is not null)
);
-- Observation identity for machine-status rows created by the Phase 7 watcher:
-- exact chain-signature binding needs source/dest/mint/slot. NOT VALID so legacy
-- rows re-labeled above (no chain metadata recorded pre-0011) are not retro-failed;
-- every new/updated row is enforced.
alter table deposits add constraint deposits_observation_identity check (
  machine_status is null
  or machine_status in ('admitted_legacy', 'quarantined_legacy')
  or (source_address is not null and dest_address is not null
      and mint is not null and observed_slot is not null
      and rail_fingerprint is not null)
) not valid;

-- ---------------------------------------------------------------------------
-- Outbound rails + dual-control proposals
-- ---------------------------------------------------------------------------
create table outbound_payments (
  id uuid primary key default gen_random_uuid(),
  subject text not null check (subject in ('withdrawal','deposit_refund')),
  subject_id uuid not null,
  dest text not null,
  amount_micro bigint not null check (amount_micro > 0),
  rail_fingerprint text not null,
  created_at timestamptz not null default now(),
  unique (subject, subject_id)
);

create table outbound_send_attempts (
  id uuid primary key default gen_random_uuid(),
  payment_id uuid not null references outbound_payments(id),
  attempt_number int not null check (attempt_number > 0),
  replaces_attempt_id uuid references outbound_send_attempts(id),
  signed_tx_bytes bytea not null,
  signature text not null,
  last_valid_block_height bigint not null,
  landing_state text not null check (landing_state in (
    'prepared','broadcast','unknown','finalized','definitive_failed'
  )),
  lease_expires_at timestamptz,
  evidence jsonb,
  created_at timestamptz not null default now(),
  unique (payment_id, attempt_number)
);
-- W1 attempt-lineage invariants (D31): at most one live attempt per payment,
-- at most one finalized attempt ever, globally unique signatures, and at most
-- one replacement per expired attempt.
create unique index outbound_send_attempts_one_live_uk
  on outbound_send_attempts (payment_id)
  where landing_state in ('prepared','broadcast','unknown');
create unique index outbound_send_attempts_one_finalized_uk
  on outbound_send_attempts (payment_id)
  where landing_state = 'finalized';
create unique index outbound_send_attempts_signature_uk
  on outbound_send_attempts (signature);
create unique index outbound_send_attempts_replacement_uk
  on outbound_send_attempts (replaces_attempt_id)
  where replaces_attempt_id is not null;

create table money_command_proposals (
  id uuid primary key default gen_random_uuid(),
  kind text not null,
  subject_id uuid not null,
  payload_hash text not null,
  proposer_token_id text not null,
  confirmer_token_id text,
  reason text not null,
  status text not null check (status in ('pending','confirmed','rejected','expired')),
  confirm_not_before timestamptz not null,
  expires_at timestamptz not null,
  replay_key text not null unique,
  created_at timestamptz not null default now(),
  check (expires_at > confirm_not_before),
  check (confirmer_token_id is null or confirmer_token_id <> proposer_token_id)
);
-- One open proposal per (kind, subject): duplicate proposals must replay or
-- conflict, never fork.
create unique index money_command_proposals_one_pending_subject_kind_uk
  on money_command_proposals (kind, subject_id)
  where status = 'pending';

create table kyc_events (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  from_tier int,
  to_tier int not null,
  provider_ref text,
  at timestamptz not null default now(),
  payload jsonb not null default '{}'::jsonb
);

create table sanction_screenings (
  id uuid primary key default gen_random_uuid(),
  user_id uuid references users(id),
  context text not null,
  verdict text not null check (verdict in ('clear','hit','indeterminate')),
  raw_ref text,
  checked_at timestamptz not null default now(),
  expires_at timestamptz,
  policy_version text
);

create table aml_flags (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  rule text not null,
  window_label text not null,
  evidence jsonb not null,
  status text not null check (status in ('open','cleared')),
  at timestamptz not null default now()
);

create table credit_grant_lots (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  source text not null,
  amount_micro bigint not null check (amount_micro > 0),
  granted_at timestamptz not null default now(),
  grant_class text not null,
  policy_version text not null,
  converted_at timestamptz,
  consumed_fee_micro bigint not null default 0,
  idempotency_key text unique
);

create table credit_fee_allocations (
  id uuid primary key default gen_random_uuid(),
  trade_id uuid not null,
  lot_id uuid not null references credit_grant_lots(id),
  split_seq int not null,
  amount_micro bigint not null check (amount_micro > 0),
  kind text not null check (kind in ('allocated','finalized','reversed')),
  source_allocation_id uuid references credit_fee_allocations(id),
  idempotency_key text not null unique,
  -- Shape: allocated facts stand alone; terminal facts (finalized XOR reversed)
  -- name their exact live source. The plan's algebra: <=1 terminal child per source.
  check (
    (kind = 'allocated' and source_allocation_id is null)
    or (kind in ('finalized','reversed') and source_allocation_id is not null)
  )
);
-- One allocated fact per (trade, lot, split); terminal children reuse the triple
-- and therefore must NOT be globally unique on it.
create unique index credit_fee_allocations_allocated_uk
  on credit_fee_allocations (trade_id, lot_id, split_seq)
  where kind = 'allocated';
-- <=1 terminal child per source allocation.
create unique index credit_fee_allocations_terminal_uk
  on credit_fee_allocations (source_allocation_id)
  where source_allocation_id is not null;

create table referral_codes (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  code text not null unique,
  created_at timestamptz not null default now()
);
-- One stable server-issued code per referrer (replay-safe issuance).
create unique index referral_codes_user_uk on referral_codes (user_id);

create table referral_binds (
  id uuid primary key default gen_random_uuid(),
  referrer_id uuid not null references users(id),
  referee_id uuid not null references users(id) unique,
  bind_key text not null unique,
  created_at timestamptz not null default now(),
  check (referrer_id <> referee_id)
);

create table phone_verifications (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  number_hmac text not null,
  hmac_key_version int not null default 1,
  challenge text,
  expires_at timestamptz,
  attempts int not null default 0,
  verified_at timestamptz,
  provider_ref text,
  unique (number_hmac, hmac_key_version)
);

create table self_exclusions (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references users(id),
  starts_at timestamptz not null default now(),
  cooling_off_until timestamptz not null,
  lifted_at timestamptz
);

create table user_deposit_limits (
  user_id uuid primary key references users(id),
  limit_micro bigint not null check (limit_micro >= 0),
  pending_limit_micro bigint,
  pending_effective_at timestamptz,
  updated_at timestamptz not null default now()
);

create table alert_outbox (
  id uuid primary key default gen_random_uuid(),
  incident_key text not null,
  severity text not null,
  body text not null,
  status text not null check (status in ('open','acked','resolved')),
  -- D35 at-least-once delivery bookkeeping + incident lifecycle timestamps
  -- (W4 finalized ask): a durable AlertStore cannot exist without them.
  delivery_attempts int not null default 0,
  -- Distinct from updated_at: pending_delivery is `status='open' AND
  -- last_paged_at IS NULL`, so the pager can never confuse "row touched" with
  -- "operator actually paged".
  last_paged_at timestamptz,
  acked_at timestamptz,
  resolved_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table compliance_decisions (
  id uuid primary key default gen_random_uuid(),
  subject_type text not null,
  subject_id uuid not null,
  kind text not null,
  actor text not null,
  at timestamptz not null default now(),
  payload jsonb not null default '{}'::jsonb
);

-- ---------------------------------------------------------------------------
-- Catalog seeds (region_allowset ABSENT = deny-all)
-- ---------------------------------------------------------------------------
insert into config_entries (key, value) values
    ('deposit_kyc_tier',                 '1'),
    ('withdraw_kyc_tier',                '2'),
    ('withdraw_min_micro',               '5000000'),
    ('withdraw_max_micro',               '1000000000'),
    ('withdraw_daily_limit_micro',       '2000000000'),
    ('withdraw_auto_approve_micro',      '50000000'),
    ('withdraw_dual_control_micro',      '500000000'),
    ('dest_warm_floor_micro',            '100000000'),
    ('dest_warm_age_hours',              '72'),
    ('dest_daily_limit_micro',           '1000000000'),
    ('hot_wallet_daily_limit_micro',     '10000000000'),
    ('deposit_confirmations',            '32'),
    ('pause_deposits',                   'false'),
    ('pause_withdrawals',                'false'),
    ('credit_signup_micro',              '5000000'),
    ('credit_referral_referrer_micro',   '5000000'),
    ('credit_referral_referee_micro',    '5000000'),
    ('referral_min_notional_micro',      '10000000'),
    ('bonus_mint_daily_cap_micro',       '500000000'),
    ('bonus_structure',                  '"real_money"'),
    ('aml_deposit_velocity_micro_24h',   '5000000000'),
    ('aml_withdraw_velocity_micro_24h',  '5000000000'),
    ('aml_structuring_n',                '4'),
    ('aml_structuring_window_hours',     '24'),
    ('aml_structuring_threshold_micro',  '500000000'),
    ('aml_structuring_floor_micro',      '100000000'),
    ('shadow_trade_cap_micro',           '25000000'),
    ('shadow_deposit_cap_micro',         '25000000'),
    ('stuck_send_sla_secs',              '900'),
    ('stuck_screening_sla_secs',         '3600'),
    ('region_allowset_version',          '1'),
    ('withdraw_approve_daily_cap_micro', '5000000000'),
    ('bonus_reserve_topup_daily_cap_micro', '1000000000')
on conflict (key) do nothing;

insert into config_changes (generation, key, old, new, changed_by)
select 1, key, null, value, 'migration:0011'
  from config_entries
 where key not in (select key from config_changes where generation = 1)
on conflict (generation, key) do nothing;
