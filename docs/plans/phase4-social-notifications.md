# Opinions — Phase 4 (Social & Notifications) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. **This revision integrates the full P4R1 fix set (codex BLOCKED + grok fix-first; see docs/reviews/p4r1-resolution.md).**

**Goal:** Social fabric + nervous system: threaded comments with votes, deterministic moderation with real anti-brigade economics, live Top Holders, public profiles, and the §7.1 notification fanout — exactly-once notification rows, **best-effort deduplicable live frames** (snapshot + REST are the recovery), delivered through the existing WS with unread counts.

**Architecture:** unchanged — `domain ← application ← adapters ← main`; pure ranking/moderation/mentions in `domain`; the notification materializer is a second outbox consumer with its own **seeded, row-locked cursor** and NO event row locks; `web/` REST+WS only.

## Global Constraints (Phase 4 deltas)

Everything standing binds (no VCS; 100% ×3 with empty allowlist; TDD; contract suites fake+Pg; typed fail-fast configs).

- Config: `SocialConfig { max_comment_len_chars: u32 (1..=4000, Unicode scalar count), max_links: u8, max_mentions: u8 (≤5), spam_window_secs: u64 (>0), max_comments_per_window: u32 (≥1), report_shadow_threshold: u32 (≥2), reporter_min_age_secs: u64, reporter_min_tier: u8, max_reports_per_window: u32 (≥1), report_window_secs: u64 (>0 — grok P4R2 N2), mention_notifs_per_hour: u32 (≥1), max_thread_depth: u8 (≥1, default 8) }`. Hot gravity pinned at 2 in the domain formula (no knob).
- **Caller identity, dev posture stated (codex B1):** every comment write body carries a self-asserted `user_id` (validated to exist) alongside the demo token, list endpoints accept an optional `viewer_id` honored only with a valid token, mark-read is `POST /users/{id}/notifications/read` scoped to that user, and `subscribe_user` **replaces** the socket's single user subscription after the token check and before any snapshot. None of this is authentication — it is the standing Phase-1 posture, restated; real principal binding stays deferred.
- **Lock order extension:** comment posting takes the **author lock** (advisory class 2 — same as votes) after `serialize_key`, before `market_for_update`, then the **parent comment row lock** after the market lock (codex B5). Vote/report take the comment row lock; insert-the-unique-row-first, mutate denormalized counters only for an inserted row.
- **Hidden-window comment posture — decided (grok B2, option 1):** free-text is **accepted speech** — the platform does not police claimed vote sides in prose (people can lie; censoring claims is theater). The hidden-window protections apply to **structured** surfaces only (tallies, receipts, profiles, notifications). The residual — coordinated "I voted X" comment spam — is bounded by the per-author comment velocity cap and named in `docs/copy/scoring.md` as a known limit of the window.
- **Vote-side privacy rule:** unchanged from R1 draft (side/score resolution-gated on every structured surface; **cast timestamps on profiles are public — D6 — only side/score are gated**, stated). No system payload carries a vote side pre-resolution.
- Notifications: in-app only this phase; rows exactly-once, **live WS frames best-effort and deduplicable** (codex P4R2 N3: the injected crash test proves a legitimate zero-frame execution — "at-least-once" would be a false promise; duplicates remain possible on retries, hence the `{id, source_seq}` dedupe fields) — reconnect/crash recovery is the authenticated snapshot + REST list, and the crash test proves exactly that.
- Deferred (complete list, grok M7): LLM moderation adapter (Phase 5, behind `Screen`), Web Push/APNs + digest email, OG share cards, Redis unread caching, comment edit/delete + vote-changing, mention autocomplete, real admin roles (`ADMIN_HANDLES` stand-in), `market_live` (needs follows), `withdrawal_settled` (Phase 7), **notification preferences + mute/block**, **comment image attachments (§6)**, **X-linked profiles (§6)**; the activity tape is the Phase 2 surface, reused not rebuilt.

**Worker protocol:** one worker owns the Rust chain **4.0 → 4.1 → 4.2 → 4.3**; `web/` 4.4 parallel; 4.5 last.

---

### Task 4.0: Migration 0006 (with backfills) + pure domain math

**Files:** create `migrations/0006_social.sql`, `crates/domain/src/ranking.rs`, `crates/domain/src/moderation.rs`, `crates/domain/src/mentions.rs`; modify `lib.rs`.

**Migration 0006 (backfill-correct per codex M1; indexes per codex M3):**

```sql
alter table comments add column body_hash text;
alter table comments add column depth int not null default 0;
alter table comments add column reply_count int not null default 0;
alter table realizations add column payout_micro bigint not null default 0
  check (payout_micro >= 0); -- terminal notification fact; sell stores net proceeds too

-- BACKFILL before constraints (pre-0006 rows may already be nested):
with recursive d as (
  select id, parent_id, 0 as depth, array[id] as path from comments where parent_id is null
  union all
  select c.id, c.parent_id, d.depth + 1, d.path || c.id
    from comments c join d on c.parent_id = d.id
   where not c.id = any(d.path)              -- cycle guard
)
update comments set depth = d.depth from d where comments.id = d.id;
-- fail the migration if any row was untouched by the CTE (cycle or orphan) or depth > 32:
do $$ begin
  if exists (select 1 from comments c where c.parent_id is not null
             and c.depth = 0) or exists (select 1 from comments where depth > 32) then
    raise exception 'comment thread backfill failed (cycle/orphan/overflow)';
  end if;
end $$;
update comments p set reply_count = sub.n
  from (select parent_id, count(*) n from comments where parent_id is not null group by parent_id) sub
 where p.id = sub.parent_id;
-- body_hash stays NULL for pre-0006 rows (grok P4R2 N3): SQL cannot reproduce the domain
-- pipeline (NFKC + Cf-strip), and a wrong hash is worse than none — the spam rule treats
-- NULL as never-matching, so the duplicate window only spans post-0006 posts (stated).

alter table comments add constraint comments_depth_nonneg check (depth >= 0);
alter table comments add constraint comments_replies_nonneg check (reply_count >= 0);
alter table comments add constraint comments_modstatus check (moderation_status in ('visible','shadow','blocked'));
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
insert into outbox_cursors values ('notifier', 0);   -- codex B3: seeded, never implicit

alter table notifications add column source_seq bigint;
create unique index notifications_dedupe_uk on notifications (user_id, source_seq) where source_seq is not null;
create index notifications_unread_idx on notifications (user_id) where read_at is null;
create index notifications_list_idx on notifications (user_id, id desc);
create index comments_page_idx on comments (market_id, created_at desc, id desc);
create index positions_holders_idx on positions (outcome_id, cost_micro desc, user_id);
create index realizations_market_src_idx on realizations (market_id, source, user_id);
create index realizations_user_idx on realizations (user_id, created_at desc);
create index trades_user_time_idx on trades (user_id, created_at desc);
create index votes_user_time_idx on votes (user_id, created_at desc);
```

**`domain::ranking` (pure; SQL semantics pinned per codex B2):** `hot_score(score: i32, age_secs: i64) -> Result<i64, RankError>` — `score_num = (max(score,0) as i128 + 1) * 1_000_000`, `denom = (age_secs/3600 + 2)^2` (floor division; `age_secs < 0 → FutureTimestamp`), result `score_num / denom` truncated. The SQL twin uses `numeric` operands with explicit `trunc(...)::bigint` (Postgres `/` on numeric is fractional — truncation must be explicit). Contract test compares **exact values AND ordering** on the grid: scores {min, -1, 0, 1, i32::MAX} × ages {0, 3599, 3600, 7199, huge}, tie cases, plus the future-time error on both sides.

**`domain::moderation` (deterministic pipeline pinned per codex M4 / grok B3):** one normalization used for BOTH the hash and the checks: Unicode NFKC → lowercase → strip all `Cf`/control chars except `\n` `\t` → collapse whitespace runs to one space → trim. `screen(body, link_count, recent_same_hash, author_posts_in_window, cfg)`: `Blocked("empty")` when normalized is empty; `Blocked("too_long")` when **Unicode scalar count** of the raw body > cap; `Shadow("links")` when `link_count > max_links` (`link_count` = occurrences of `https?://` tokens in the normalized text); `Shadow("duplicate")` when `recent_same_hash > 0` (author-scoped; multi-account copy-paste is explicitly out of scope for the hash rule — the LLM screen is the Phase 5 answer, stated); `Blocked("rate")` when `author_posts_in_window ≥ max_comments_per_window` (grok M2). Storage keeps the raw body (length-capped, control-stripped via the same pipeline definition); hash = sha256(normalized UTF-8) hex. Golden vectors: `"hello"` / `"hello "` / `"hello\u{200B}"` / `"HELLO"` collide; unmatched backticks safe.

**`domain::mentions`:** `parse_mentions(body: &str, max: u8) -> Vec<String>` (owned, lowercased — codex M4) — `@[a-z0-9_]{3,32}` case-insensitive match, code-fence spans ignored, deduped, first `max`. Property tests on adversarial unicode.

- [x] Failing tests → implement → domain 100% holds → deps-check. Pre-0006 nested fixture goes through the migration in a Pg test (codex M1).

---

### Task 4.1: Comments application (post, vote, report, list) — brigade-braked

**Interfaces (P4R1-corrected):**
- `CommentTx: IdempotencyGuard + UserLockGuard + MarketReader + CommentWriter + UserReader + VoteReader + OutboxWriter + Committable`. Post sequence: `serialize_key` → `lock_user(author)` (the spam rule is now serialized — codex B5) → `market_for_update` → parent `comment_for_update` (status/depth revalidated under the lock; reply_count bumped in the same tx) → screen (with `recent_same_hash` + `author_posts_in_window` read under the author lock) → insert → events. `CommentPosted` payload carries `{market_id, comment_id, parent_id, author_id, author_handle, parent_author_id?}`; `MentionCreated` payload `{comment_id, market_id, mentioned_user_id, author_id}` (codex M2: fanout needs ids, not lookups).
- **Report economics (grok B1):** reporters must satisfy `account_age ≥ reporter_min_age_secs` **or** `tier ≥ reporter_min_tier` (D21 primitives reused), AND per-reporter velocity `max_reports_per_window` (under the reporter's user lock). Vote/report: comment row lock → unique insert first → counter/threshold mutation only when inserted (codex B5); threshold event emitted in the same tx. **Restore semantics: curator restore writes `moderation_status='visible'` AND deletes the comment's report rows (epoch reset — a fresh brigade must fully re-clear the bar); a second shadow requires threshold NEW reports.** Vote/report/reply are rejected on non-`visible` comments (grok M3 — no score drift the author can see, no report pile-on while hidden); author's own read path unaffected.
- Listing: `comments(market, sort, viewer, limit, cursor)` — **hot pagination is `as_of`-pinned (codex B2): the first page computes `as_of = now()` and returns it in the cursor `(as_of, hot_score, created_at, id)`; subsequent pages reuse `as_of` and order by all cursor fields** — stable across hour boundaries. Hot is computed over a **frozen candidate set** (codex P4R2 N2): every page's candidate subquery is `created_at <= cursor.as_of ORDER BY created_at DESC, id DESC LIMIT 500` — inserts between pages can neither displace candidates nor appear as future-scored rows; scoring and seeking then order by `(hot_score, created_at, id)`. (A denormalized rank column is deliberate future work if e2e shows pain — grok m1.) Recent sort keeps the `(created_at, id)` cursor. Shadow visible only to the author-viewer.

**Tests:** every rule; the new concurrency set: concurrent same-hash posts by one author → second shadowed (author lock proof); two reporters racing the threshold → exactly one threshold event; parent shadowed between read and reply → reply rejected (parent lock proof); vote/report on shadow → 409; report-after-restore needs full re-threshold; low-quality reporter rejected; hot page 2 with as_of stability across a simulated hour boundary.

- [x] Failing tests → implement (fake + Pg + contracts) → gates green.

---

### Task 4.2: Notification fanout — cursor-safe materializer + honest WS

**Interfaces (P4R1-corrected):**
- **Materializer loop (codex B3 — the safe shape):** one tx: `SELECT last_seq FROM outbox_cursors WHERE consumer='notifier' FOR UPDATE` → fetch events **plain ordered read, `seq > last_seq ORDER BY seq LIMIT 128`, NO row locks, NO SKIP LOCKED** (immutable rows; SKIP LOCKED could skip a relay-locked event and permanently lose it) → `application::notify::materialize(OutboxEvent { seq, event_type, payload })` per event → bulk insert `ON CONFLICT (user_id, source_seq) WHERE source_seq IS NOT NULL DO NOTHING` (codex P4R2 N1: the dedupe index is partial — the conflict target must name its predicate or Postgres rejects the statement) → `UPDATE outbox_cursors SET last_seq = max_fetched` → commit. Crash anywhere → full rollback → replay; overlap absorbed by the unique index. Tests: empty startup, rollback before/after insert, partial-batch failure, two concurrent pumps (cursor lock serializes), stale pump, unrecognized event advances safely, relay holding event row locks concurrently (no interference — the notifier takes none).
- **Fanout policy (cardinalities pinned per codex M2):** resolution recipients = one row per user from **terminal facts only** (`source IN ('settlement','void')` aggregated per user — sell facts excluded; a user holding both outcomes still gets exactly one row with `payout_total_micro = sum(realizations.payout_micro)` and the summed `realized_delta_micro`); voters anti-joined against that set get `resolution_vote`; **holder∧voter payload carries both money and score**, and the aggregated payload is named for what it is (codex P4R2 N5): `{payout_total_micro, redemption_yes_micro, redemption_no_micro (the market's per-side rates), realized_delta_micro, score_bp?, side?}` — a holder of both outcomes gets ONE row whose totals are well-defined; score/side present only when the user voted (post-resolution, so allowed — grok M4); void → distinct union of traders (facts) and voters, `resolution_void`. `comment_reply` from `parent_author_id` (skip self); `mention` skip when mentioned == parent_author (collapse); **mention rate-brake (grok M1): the materializer drops `mention` notifications beyond `mention_notifs_per_hour` per recipient (dropped count logged), `comment_reply` uncapped**; `RepUpdated` → `rep_tier_change` (the event only exists on tier change — pinned in Phase 3); `CuratorNeeded` → `ADMIN_HANDLES` users.
- **WS user channel (codex B4):** typed internal bus `enum BusEvent { Market(WireEvent), UserNotif { user_id, frame } }` — user frames filtered per connection by subscribed user (single subscription, replaced on re-subscribe, token checked before snapshot); broadcast happens **post-commit** (an injected crash between commit and send is tested: the frames are lost, REST + snapshot recover — that IS the contract); `notif` frames carry `{v:1, id, source_seq, type, payload}` for client dedupe; `notif_snapshot` carries unread count.
- Performance: a 1,000-participant resolution materializes in **< 2s (hard gate, Pg fixture)** — set-based reads + one bulk insert (codex M2).

- [x] Failing tests → implement → gates green.

---

### Task 4.3: HTTP surfaces

As R1 draft, with the P4R1 identity + index corrections: comment writes carry `user_id`; `viewer_id` on lists; mark-read scoped `POST /users/{id}/notifications/read {ids}` (idempotent, only that user's rows); holders per-column limit with pinned tie-breaker `(cost_micro DESC, user_id)`, zero positions excluded, "by committed capital" label + MTM deferral in utoipa; profile realized PnL from `realizations_user_idx`; admin reported-comments list + moderate (restore = epoch reset per 4.1); notifications list via `notifications_list_idx`. OpenAPI + api_models regenerated (twice + `cmp`). Route tests incl. 401s, cross-user mark-read rejection, shadow visibility matrix.

- [x] Failing tests → implement → gates green.

---

### Task 4.4: Web — social surfaces (parallel; owns `web/` + `docs/copy/notifications.md`)

As R1 draft, plus P4R1: **`docs/copy/scoring.md` gains the accepted-speech paragraph** (grok P4R2 N1 — the window's known residual: "comments are free speech; claimed vote sides in prose are unverified and unpoliced; the hidden window protects the actual tallies, receipts, and profiles") — and **`docs/copy/notifications.md` is created first** (grok M6) with exact templates — `resolution_trade` ("Paid out {pnl} on '{question}'" / holder∧voter variant adds "your crowd call scored {score}"), `resolution_vote`, `resolution_void`, `comment_reply` ("{handle} replied to you"), `mention`, `rep_tier_change`, curator/admin lines, report confirmation, shadow banner ("Only you can see this while it's under review"), blocked reasons (empty/too_long/rate), error copy (`ThreadTooDeep`, duplicate vote, reporter-floor rejection) — web renders from it, zero lorem. **Toasts fire only for `resolution_*` and `rep_tier_change`; replies/mentions update the badge silently (grok M1).** Optimistic vote reconcile; no vote/report affordances on shadowed rows; no edit/delete affordances (m2); no `dangerouslySetInnerHTML` (vitest grep assertion); frame dedupe by `(id, source_seq)`. `pnpm build` + `pnpm test` green.

---

### Task 4.5: Phase 4 exit verification

- [x] Full gates + `scripts/e2e_social.sh` **PHASE 4 E2E GREEN** (2026-08-12); docs sync.

As R1 draft (full gates + `scripts/e2e_social.sh`), with P4R1 additions: the brigade section uses **threshold young/tier-0 accounts and must FAIL to shadow** (reporter floor), then threshold qualified accounts succeed, admin restore resets the epoch (a single new report does not re-shadow); the holder∧voter notification carries PnL AND score; mention flood beyond the hourly brake drops (badge count proves it); crash-injected commit-to-send loss recovers via snapshot (run the harness hook if present, else assert the REST list matches rows after a forced reconnect); hot pagination stable across a mocked hour boundary. `PHASE 4 E2E GREEN` marker; docs sync (plan checkboxes, PLAN.md row, README).

## Reviewer checkpoints (R2 verifies specifically)

1. Hot SQL truncation + as_of cursor semantics exactly as pinned; value-equality grid.
2. Comment lock chain (`key → author → market → parent/comment`) global-order-consistent; threshold/restore epoch race-tested.
3. Materializer: seeded locked cursor, no event locks, max-seq advance — the B3 loss scenario dead; relay coexistence test.
4. Fanout cardinalities: terminal-facts aggregation, anti-join, both-outcomes single row, void union — implementable exactly as written; 1k < 2s gate.
5. WS: post-commit boundary + crash test + frame dedupe fields; single-subscription replacement semantics.
6. Identity posture: every write self-asserts user_id, mark-read scoped, viewer_id honored only with token — blast radius honestly bounded.
7. Backfills: CTE cycle guard + failure conditions; composite parent FK validates old rows; index-vs-query fit (EXPLAIN on the four new list shapes).
8. Copy + privacy: notifications.md complete; accepted-speech posture + velocity cap named in scoring.md; cast-timestamp publicity stated; no system payload carries pre-resolution side.
