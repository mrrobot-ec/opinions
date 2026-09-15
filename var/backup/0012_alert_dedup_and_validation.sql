-- ---------------------------------------------------------------------------
-- 0012 — make D35 incident dedup structural, and close 0011's NOT VALID
--        escape hatch now that its grandfathering has served its purpose.
-- ---------------------------------------------------------------------------

-- D35 is normative: "durable alert outbox; incident key = detector+subject+
-- episode; dedup within an open incident". `IncidentManager::raise` cannot
-- deliver that in application code: `find_open` and `insert` are separate
-- autocommit statements on separate pooled connections, so two concurrent
-- raisers both read "absent", both insert, and the operator is paged twice for
-- one condition. The rule has to live in the database.
--
-- The index is deliberately PARTIAL. A resolved episode is KEPT as its own row
-- so that a recurrence after recovery re-pages (also D35), and
-- `alert_contract.rs` asserts exactly that two-row outcome. A total
-- `unique (incident_key)` would therefore forbid the behaviour the spec
-- requires. `acked` is inside the predicate because an acked incident still
-- dedups a recurrence — it is acknowledged, not over.
create unique index alert_outbox_one_open_per_key
    on alert_outbox (incident_key)
 where status in ('open', 'acked');

-- The at-least-once pump runs every minute and scans for never-paged live
-- incidents; without this the table (which retains every resolved episode
-- forever) is sequentially scanned on every tick.
create index alert_outbox_pending_delivery_idx
    on alert_outbox (created_at, id)
 where last_paged_at is null and status in ('open', 'acked');

-- 0011 added `deposits_observation_identity` NOT VALID, reasoning that the
-- legacy rows it had just re-labelled must not be retro-failed. That reasoning
-- does not actually require the marker: the predicate itself exempts
-- `machine_status is null` and both grandfathered labels, which are exactly
-- the three shapes 0011's own backfill produces. VALIDATE is therefore a
-- no-op over every row 0011 could have left behind, and it is proven so by
-- `crates/adapters/tests/migration_0011_deposit_identity.rs`, which validates
-- the constraint with all three legacy shapes present.
--
-- Leaving it unvalidated forever would mean nobody ever proved the existing
-- rows satisfy it — `pg_constraint.convalidated` is the only durable record of
-- that. This takes SHARE UPDATE EXCLUSIVE and scans `deposits`; it is
-- deliberately a separate, later migration rather than a change to the applied
-- 0011 semantics.
alter table deposits validate constraint deposits_observation_identity;
