VERDICT: fix-first

Reviewed `docs/plans/phase4-social-notifications.md` against spec §§6–7.1, D6 / D11 / D22, `docs/copy/scoring.md`, `web/` (no social surfaces yet; countdown `setInterval` is local mono only), and `cast_vote.rs` privacy discipline (receipt `seq` suppressed in the hidden window; `VoteCast` outbox payload carries no side). Phase 4’s *shape* is right: pure ranking/moderation/mentions in domain, second outbox consumer with its own cursor, vote-side privacy rule stated, holder∧voter + reply+mention collapses named, honest cost-basis holders label, deferrals mostly explicit. Product-mechanics and adversarial gaps still make “restore + one-report-per-account is enough,” “normalized body hash,” and free-text tally side-channels under-specified for a contested prediction market — amend plan text before 4.1–4.2 build.

## FINDINGS

[B1] **Report-brigading is cheap; “restore + one-per-account” is not a bound.**  
`report_shadow_threshold` (≥2) + `PRIMARY KEY (comment_id, reporter_id)` means any N ≥ threshold demo accounts (Phase-1 shared-token posture restated in 4.2) can shadow any comment. Restore is admin-reactive (`POST /admin/comments/{id}/moderate`); there is no reporter min-age/rep/tier gate, no per-reporter report rate, no cool-down after false reports, and no auto-unshadow when a curator repeatedly restores. Under contest near close, brigade becomes a free “hide the inconvenient take until a human wakes up” button — the opposite of spec §6’s “alive *and* not toxic” differentiator, and worse because shadow is detectable to the author (4.4 copy), so they re-post and the section becomes brigaded spam loops.  
**FIX:** In Task 4.1, pin at least one economic brake before ship: (a) reporters must pass a floor (account age ≥ X **or** tier ≥ 1 **or** phone channel — reuse D21 primitives), **and** (b) per-reporter report velocity (e.g. ≤K reports / window across markets). Optionally weight threshold by distinct reporter quality later; do **not** claim “bounded by restore” without an admin SLA or auto-restore. E2E must include “threshold low-quality accounts cannot shadow a high-rep author’s comment” **or** document explicit acceptance that demo-era brigading is open.

[B2] **Free-text comments are an unaddressed tally side-channel during the hidden window.**  
Global privacy rule (plan lines 15, 126) and `cast_vote` correctly suppress **structured** vote side / seq on API surfaces pre-resolution. Nothing stops coordinated “I voted YES / guess 72” spam in the comment section while tallies are hidden and trading is frozen (D22). That is a copyable oracle leak the product spent integrity engineering to close on the vote path. Plan never chooses: accepted speech (D6 culture) vs. window rule.  
**FIX:** State an explicit product rule in Global Constraints + Task 4.1: either (1) **Accepted speech** — free-text claims are not moderated for vote-side content; hidden-window protection is structured surfaces only (profile, notifications, tally frames) — document that coordinated comment spam is a known residual; or (2) **Window posture** — during `Closing` / `now ≥ tally_hidden_at`, tighten comment rate (per-user posts/min) and/or surface soft copy “vote sides stay private until resolve” without content-policing sides. Prefer (1) for Phase 4 scope honesty **if** comment post velocity is capped (see M2); do not ship silence on the contradiction.

[B3] **`body_hash` / “normalized body” is undefined — duplicate shadow is non-implementable as written.**  
Task 4.0 stores `body_hash`; 4.1 says `hash = sha256(normalized body)` and shadows on `recent_same_hash > 0`. Normalization steps are never listed. Whitespace padding, ZWSP/bidi marks (only “control chars beyond \\n\\t” blocked on *empty/oversize*, not necessarily stripped before hash), homoglyphs, and trivial paraphrase all evade same-hash; multi-account copy-paste of the **same** body is also invisible because `recent_same_hash` is **author-scoped**. `spam_window_secs` appears only as hash window, not post rate. Link count input is unspecified (regex? bare domains? markdown?).  
**FIX:** Pin normalization in `domain::moderation` (e.g. Unicode NFKC → lowercase → collapse whitespace → strip Cf/format chars → hash UTF-8) and define `link_count` (count of `https?://` tokens after the same normalize). State multi-account identical-body spam as **out of scope** for the hash rule (LLM Phase 5) **or** add a market-scoped same-hash counter for Shadow("flood"). Add property tests: `"hello"` vs `"hello "` vs `"hello\\u{200B}"` collide after normalize.

[M1] **Mention / notification harassment posture is incomplete.**  
`max_mentions ≤ 5` caps per-comment fanout, and reply+mention collapse avoids double-ping on one parent author — good. N distinct accounts each posting one `@target` still produce N `mention` notifications with no global “notifications of type mention to user U per window” cap, no mute, no block. Toast-on-every-`notif` (4.4) amplifies this into UI fatigue. Spec §7.1 also promises **preferences per type per channel** — not implemented and **not** on the deferred list (only push/APNs/digest/Redis/OG/`market_live`/`withdrawal_settled`/edit-delete/autocomplete/roles).  
**FIX:** (1) Add to Deferred: notification preferences + mute/block. (2) Phase 4 minimum: materializer or insert path rate-limits `mention` (and optionally `comment_reply`) per recipient (e.g. drop or coalesce beyond R/hour, still store? prefer drop excess with metric). (3) Web: toast only high-priority types (`resolution_*`, `rep_tier_change`) or coalesce toasts 2s; bell badge still updates — state in 4.4.

[M2] **No comment post velocity — spam screen is only links + same-hash.**  
Config has `spam_window_secs` but PostComment has no per-author posts-per-window gate (unlike votes’ global velocity). One account can flood a market’s recent sort and hot baseline (+1) during the close window. Combined with B2, this is the practical gaming lever.  
**FIX:** Add `max_comments_per_window` (or reuse a social velocity on the user lock) in `SocialConfig`; enforce under the same lock order as other user writes; test at-limit.

[M3] **Shadow visibility / detectability — intentional UX, but vote-by-id and score leaks need a line.**  
Author-visible shadow + “only you can see this while it's under review” correctly tells the author they are shadowed (good; silent shadow is cruel). Listing excludes shadow for others. Gap: `POST /comments/{id}/vote` and `report` by raw UUID — if allowed on `shadow`/`blocked` rows, holders of a leaked id can bump `score` the author still sees, or pile reports. Parent must be `visible` for replies — good.  
**FIX:** Reject vote/report/reply on non-`visible` comments (except author read path); author-only GET of own shadow does not expose public score deltas from invisible voters (or freeze score display while shadowed). Document: detectability via banner is **accepted**; detectability via third-party score changes is **not**.

[M4] **Holder∧voter single-notification drops vote score for holders.**  
Spec §7.1 + plan: holders get `resolution_trade` (PnL); voters **without** holdings get `resolution_vote` (outcome + score). A holder who also voted learns PnL only — no score/guess accuracy in-app unless they open the market. Collapse is right for badge spam; payload is incomplete for the dual participant.  
**FIX:** Keep one row per user; for holder∧voter put `{realized_delta, redemption, score_bp?, side?}` on `resolution_trade` (side/score only post-resolve — already allowed) **or** document “holders check profile/market for score; notif is money-first” as intentional and match web toast copy.

[M5] **Profile “recent votes” timing still leaks activity near close; holders concentration is D6-coherent.**  
Side/score suppressed pre-resolve is correct and matches `cast_vote` spirit. Remaining: public `cast_at` on profiles shows *when* someone voted (cluster of high-rep accounts voting in the last 2 minutes of a flash market is a soft signal). Acceptable under D6 **if** stated; not a side leak. Holders panel by committed capital (YES/NO columns) reveals position concentration — **coherent with D6** (“everything public”) and competitive parity (§6 Top Holders); cost-basis vs spec’s “position value” is the right Phase 4 honesty move (checkpoint 7).  
**FIX:** One sentence in privacy rule: “cast timestamps on profiles are public (D6); only side/score are resolution-gated.” Holders: keep cost-basis + label; note deliberate drift from §6 “position value” until MTM exists.

[M6] **Notification + moderation user copy is under-specified (placeholder risk).**  
4.4 gives two resolution examples; no pinned strings for `comment_reply`, `mention`, `resolution_void`, `rep_tier_change`, CuratorNeeded/admin, report confirmation, shadow banner, blocked reject reasons, holders “by committed capital” tooltip, or error copy (`ThreadTooDeep`, duplicate vote). Phase 3 needed `docs/copy/scoring.md` for the same reason.  
**FIX:** Add `docs/copy/notifications.md` (or a checklist in 4.4) with exact templates and placeholders (`{handle}`, `{question}`, `{pnl}`, `{score}`). Web task: no lorem; strings sourced from that file or inline constants matching it.

[M7] **§7.1 / §6 promised-but-missing beyond named deferrals.**  
Named deferrals cover push/APNs, digest, Redis badge, OG cards, `market_live` (+ follows), `withdrawal_settled`, LLM screen, edit/delete, autocomplete, real roles. Still missing from the deferral bullet list: **notification preferences**, **image attachments on comments** (§6), **X-linked profiles** (§6), **activity tape as a dedicated social surface** (tape already exists from P2 — say “reused, not re-built”), **daily digest**. Admin `CuratorNeeded` via `ADMIN_HANDLES` is a right stand-in; reply+mention collapse and demo-token WS user channel are right calls with honest blast-radius (stolen token → that user’s notifs only).  
**FIX:** Extend Deferred list with preferences, comment images, X-link; one line that activity tape is Phase 2 surface unchanged.

[m1] **Hot index vs hot formula mismatch risk.**  
Migration proposes `comments_market_hot_idx on (market_id, created_at desc)` while hot order is `hot_score(score, age)`. Index helps **recent** sort and pagination cursors, not hot. Acceptable if hot is small-N in-memory/SQL formula per market; at viral depth it sequential-scans.  
**FIX:** State “hot is computed on the candidate window (e.g. last N by created_at or score floor), not fully indexed this phase”; or add a denormalized `hot_score` column maintained on vote — YAGNI unless e2e shows pain.

[m2] **No edit/delete this phase is acceptable.**  
Stated product-decision-pending. Fine for MVP if report+shadow+admin restore cover abuse and copy doesn’t promise “delete your comment.”  
**FIX:** 4.4 UI: no edit/delete affordances; report only.

## CHECKPOINTS (plan’s 8)

1. **Hot formula:** Integer gravity 2, `score_num = max(score,0)+1`, `age_hours = age_secs/3600` floor, SCALE 1e6, `age_secs < 0 → error` — sound. Boundary grid (0, 3599, 3600, huge) + **value equality** Rust↔SQL (not order-only) is the right bar; implementers must use identical integer division semantics in SQL (`/` on bigint) and pin golden vectors. **No product issue; engineering must not weaken to order-only.**

2. **Comment write path:** Guard-first + parent `visible` + depth cap are right. Residual races: (a) parent transitions to shadow/blocked between read and insert without `comment_for_update(parent)`; (b) concurrent replies bumping `reply_count` need parent row lock; (c) duplicate vote / report idempotency need unique constraints (votes already PK; reports PK) + mapped 409. **Plan should require parent locked for update on reply path.** Report threshold crossing under concurrency: two reporters can both observe count=threshold-1 — use count-after-insert or `set_moderation` where count≥threshold in same tx.

3. **Materializer exactly-once:** Cursor advance **same transaction** as bulk insert + `ON CONFLICT DO NOTHING` on `(user_id, source_seq)` is the correct shape; crash between commit and WS push → at-least-once frame, client dedupes by notif id (state that). Paths to **lost** notif: cursor advanced without insert (forbidden if same tx); paths to **dup rows**: unique index blocks; paths to **dup WS**: bus at-least-once — OK. Relay independence: never touch `published_at` — contract test both consumers on one fixture. **Sound if implemented as written.**

4. **Fanout recipients:** Holder∧voter → one `resolution_trade` (see M4 payload gap); void → all participants; reply skip self; mention skip if already reply target; self-mention dropped; `RepUpdated`→tier change only (name implies filter to tier boundary — **pin “only when tier changes”**); `CuratorNeeded`→`ADMIN_HANDLES`. §7.1 minus stated deferrals is complete **after** M7 deferral list fix. Preferences missing (M1/M7).

5. **Privacy rule:** Structured side suppression on profile + resolution-gated voter notif payloads is airtight **if** every DTO path uses the same helper (profile recent votes, any admin export, converse if it ever surfaces votes). Comments free-text (B2) and cast timestamps (M5) are the residual honesty items. **No side in `VoteCast` outbox already — keep CommentPosted free of inferred vote side.**

6. **WS user channel:** Demo-token + subscribe_user to arbitrary `user_id` if token known = stated Phase-1 hole; blast radius = that user’s notifications only (no market admin). Acceptable for Phase 4 with the honesty sentence already in the plan. Real auth still deferred.

7. **Holders by cost basis:** Honest label “by committed capital”; partial sells / cost relief ordering must be defined in SQL (remaining cost basis, not lifetime gross). Right Phase 4 metric; MTM deferred — state in utoipa. Coherent with D6 public concentration.

8. **No-VCS / no-polling:** Holders re-fetch on `trade` frame with **2s coalesce** is event-driven, not a timer poll — good (coalesce window is debounce of bursts, not `setInterval` fetch). Notification bell is WS snapshot + push frames. Confirm 4.4 adds no `setInterval` API refresh (local countdown mono intervals on market page already exist and are fine). Activity/comments list: initial REST + optional live comment frames? Plan doesn’t add `comment` WS frames — list is pull-on-navigate unless they subscribe; **not polling** if no interval. If live comments are desired later, fanout a market-scoped comment frame; out of Phase 4 unless added.

## WHAT CHECKS OUT

- Architecture: domain pure ranking/moderation/mentions; application owns notify policy; adapter materializer mirrors relay with separate cursor — matches standing hexagonal + outbox discipline.
- Shadow ≠ delete; author-visible; admin restore path — right gray-case model for §6 (economics still need B1).
- Vote-side privacy as a named global constraint extending hidden-window discipline is the correct product frame; `cast_vote` seq suppression is the precedent.
- Holder cost-basis + “committed capital” label is more honest than fake MTM.
- Reply+mention collapse, self-filters, exactly-once `(user_id, source_seq)`, e2e social loop sketch — strong.
- Web MVP without edit/delete/autocomplete is scope-honest **with** m2/M6 copy discipline.
- Deferral of LLM screen behind the same `Screen` enum is the right seam.

## BUILD READ

Do not start Tasks **4.1–4.2** until plan text amends **B1–B3** (report economics, hidden-window comment posture, hash/link normalization). Land **M1–M2** (mention rate / toast posture, comment velocity) and **M4/M6** (holder-voter payload or explicit money-first rule; copy source) in the same amend so implementation and e2e don’t encode free brigading and placeholder strings. Task **4.0** can proceed on ranking + a fully specified `screen`/`normalize` once B3 text exists. Task **4.4** parallel is fine against frozen API shapes after the amend.
