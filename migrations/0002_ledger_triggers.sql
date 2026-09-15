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
