alter table comments add column body_hash text;
alter table comments add column depth int not null default 0;
alter table comments add column reply_count int not null default 0;
alter table realizations add column payout_micro bigint not null default 0
  check (payout_micro >= 0);

with recursive comment_depths as (
  select id, parent_id, 0 as depth, array[id] as path
    from comments
   where parent_id is null
  union all
  select child.id, child.parent_id, parent.depth + 1, parent.path || child.id
    from comments child
    join comment_depths parent on child.parent_id = parent.id
   where not child.id = any(parent.path)
)
update comments
   set depth = comment_depths.depth
  from comment_depths
 where comments.id = comment_depths.id;

do $$
begin
  if exists (
       select 1
         from comments
        where parent_id is not null and depth = 0
     ) or exists (
       select 1
         from comments
        where depth > 32
     ) then
    raise exception 'comment thread backfill failed (cycle/orphan/overflow)';
  end if;
end
$$;

update comments parent
   set reply_count = replies.n
  from (
    select parent_id, count(*)::int as n
      from comments
     where parent_id is not null
     group by parent_id
  ) replies
 where parent.id = replies.parent_id;

-- Existing rows deliberately retain NULL hashes: PostgreSQL cannot reproduce
-- the domain NFKC/Cf pipeline exactly. NULL never matches the author-scoped
-- bounded duplicate lookback, so only post-migration rows participate.
alter table comments add constraint comments_depth_nonneg check (depth >= 0);
alter table comments add constraint comments_replies_nonneg check (reply_count >= 0);
alter table comments add constraint comments_modstatus
  check (moderation_status in ('visible', 'shadow', 'blocked'));
alter table comments add constraint comments_uk_id_market unique (id, market_id);
alter table comments add constraint comments_parent_same_market
  foreign key (parent_id, market_id) references comments (id, market_id);

create table comment_reports (
  comment_id uuid not null references comments(id),
  reporter_id uuid not null references users(id),
  created_at timestamptz not null default now(),
  primary key (comment_id, reporter_id)
);

create table outbox_cursors (
  consumer text primary key,
  last_seq bigint not null
);
insert into outbox_cursors (consumer, last_seq) values ('notifier', 0);

alter table notifications add column source_seq bigint;
create unique index notifications_dedupe_uk
  on notifications (user_id, source_seq) where source_seq is not null;
create index notifications_unread_idx
  on notifications (user_id) where read_at is null;
create index notifications_list_idx on notifications (user_id, id desc);
create index comments_page_idx on comments (market_id, created_at desc, id desc);
create index positions_holders_idx on positions (outcome_id, cost_micro desc, user_id);
create index realizations_market_src_idx on realizations (market_id, source, user_id);
create index realizations_user_idx on realizations (user_id, created_at desc);
create index trades_user_time_idx on trades (user_id, created_at desc);
create index votes_user_time_idx on votes (user_id, created_at desc);
