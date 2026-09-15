-- One account per (owner, currency) for owned classes; singletons for the rest.
create unique index ledger_accounts_owned_uk on ledger_accounts (owner_type, owner_id, currency)
  where owner_type in ('user','pool','escrow');
create unique index ledger_accounts_singleton_uk on ledger_accounts (owner_type, currency)
  where owner_type in ('fees','house');

-- Ownership shape: NULL owner_id would make the owned-class unique index vacuous
-- (PostgreSQL NULLs are distinct).
alter table ledger_accounts add constraint ledger_accounts_owner_shape check (
  (owner_type in ('user','pool','escrow') and owner_id is not null)
  or (owner_type in ('fees','house','external') and owner_id is null)
);

-- Account identity is immutable: reclassification would silently re-currency history.
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

-- Converse identity: phone -> user.
create table user_channels (
  user_id uuid not null references users(id),
  channel text not null,
  address text not null,
  created_at timestamptz not null default now(),
  unique (channel, address)
);
