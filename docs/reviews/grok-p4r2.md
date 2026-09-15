# Phase 4 plan — Round 2 VERIFY (Grok)

Verified rewritten `docs/plans/phase4-social-notifications.md` + `docs/reviews/p4r1-resolution.md` against `docs/reviews/grok-p4r1.md` (and codex product-facing fixes). No code execution; findings-only.

## Own findings (map)

| ID | Status | One line |
|---|---|---|
| **B1** brigade economics | **VERIFIED** | Floor (`age ≥ reporter_min_age_secs` **or** `tier ≥ reporter_min_tier`) **and** `max_reports_per_window` under reporter lock + e2e “unqualified cannot shadow” closes free sybil shadow; restore **epoch reset** (delete report rows → full new threshold) stops one leftover report from re-shadowing and is the right anti-whack for *authors*, not free re-hide. |
| **B2** accepted speech | **VERIFIED** (copy gap → NEW#1) | Option-1 decision is coherent and honestly framed (structured-only protection; liars make prose policing theater); residual bounded by comment velocity; **but** plan claims residual is “named in `docs/copy/scoring.md`” and that file still has **no** such paragraph, and no task checkbox owns the edit. |
| **B3** normalize + scope | **VERIFIED** | Single pipeline NFKC → lower → strip Cf/controls (keep `\n\t`) → collapse ws → trim; Unicode scalar length; `https?://` link tokens; golden collisions; multi-account copy-paste **explicitly** out of scope until Phase 5 LLM — implementable and honest. |
| **M1** mention + toast | **VERIFIED** | `mention_notifs_per_hour` drop-with-log at materializer; toasts only `resolution_*` / `rep_tier_change`; preferences + mute/block on deferral list. |
| **M2** comment velocity | **VERIFIED** | `max_comments_per_window` + `Blocked("rate")` under author lock (serialized with spam/hash reads). |
| **M3** shadow interactions | **VERIFIED** | Vote/report/reply rejected on non-`visible`; no score drift / report pile-on while hidden; author read + banner detectability accepted. |
| **M4** holder∧voter payload | **VERIFIED** | Single `resolution_trade` carries `{realized_delta, redemption, score_bp, side}` post-resolve; copy template includes score variant. |
| **M5** cast timestamps | **VERIFIED** | Privacy rule states timestamps public (D6), only side/score gated; holders cost-basis + MTM deferral in utoipa. |
| **M6** notifications copy | **VERIFIED** | Task 4.4 creates `docs/copy/notifications.md` first with full type/error/banner set; web renders from it (file correctly not pre-landed). |
| **M7** deferral list | **VERIFIED** | Preferences/mute/block, comment images, X-linked profiles, tape-as-P2-reuse all present; §7.1 minus list is complete. |
| **m1** hot candidate window | **VERIFIED** | Last-500 by created_at; denormalized rank deferred. |
| **m2** no edit/delete | **VERIFIED** | UI ships no affordances; copy promises none. |

### B1 product note — epoch reset vs curator load

Epoch reset does **not** create free whack-a-mole for attackers: each re-shadow costs **threshold NEW qualified reports** under reporter velocity, not a single leftover row. Curator load is still **manual O(successful brigades)** — if N aged/tiered accounts rotate report quota, a curator can re-restore repeatedly. That is the correct Phase 4 tradeoff (no auto-restore SLA, no reporter reputation EWMA yet); e2e + floor makes demo sybil cheap-path dead. **Not a regression.** Residual for later: dismiss-without-restore, reporter cool-down after admin clear, or weighted threshold — out of this phase.

### B2 honesty residual

Decision text in Global Constraints is sound. What is still silent until NEW#1 lands: published scoring/integrity page will not tell users “comments can claim sides during the window” even though the plan promises that naming. Product posture is decided; **docs lag the plan**.

## Codex fixes as product matters

| Topic | Status | One line |
|---|---|---|
| Self-asserted `user_id` / scoped mark-read / `viewer_id` + token | **VERIFIED** | Acceptable **dev-era** UX: honest “not authentication,” blast radius is spoof any user if you hold the shared demo token (already standing Phase-1); real principal binding deferred. Ship only with that sentence in OpenAPI/README. |
| `as_of`-pinned hot cursor | **VERIFIED** | Page staleness is acceptable: freeze age reference so hour-boundary skips/dupes die; users refresh page 1 for “live now” hot. Better than unstable cursors; not a product lie if list is snapshot-ish. |
| Restore = epoch reset | **VERIFIED** | Better product than “count stays ≥ threshold forever”; pairs with brigade floor (see B1 note). |
| Rows exactly-once / frames best-effort + crash recovery | **VERIFIED** | Honest delivery contract; toast/badge must not assume lossless live frames. |
| Fanout cardinalities / terminal-facts aggregation | **VERIFIED** | Implementable product semantics; holder both-outcomes one row; void union. |

## NEW issues from the rewrite (cap 5)

[N1] **`scoring.md` residual not landed and not task-owned.**  
Plan line 16 asserts the accepted-speech residual is “named in `docs/copy/scoring.md`”; grep shows it is absent, and Task 4.4 only creates `notifications.md`.  
**FIX:** One checkbox under 4.4 or 4.5: append a short “Hidden window & comments” note to `docs/copy/scoring.md` (structured surfaces protected; free-text claims not policed; velocity bounds spam) and surface it on how-it-works if that page pulls scoring copy.

[N2] **Report velocity window duration is unpinned.**  
Config has `max_reports_per_window` but no `report_window_secs` (or explicit reuse of `spam_window_secs`). Implementers will invent a constant.  
**FIX:** One sentence: report velocity uses `spam_window_secs` (or add `report_window_secs > 0`).

[N3] **Migration `body_hash` backfill ≠ domain normalize pipeline.**  
0006 SQL backfill is `lower(regexp_replace(body, '\s+', ' ', 'g'))` only — missing NFKC, Cf strip, trim order of domain::moderation. Pre-0006 rows can fail to collide with post-4.0 posts of “the same” spam.  
**FIX:** Document “backfill is best-effort; new posts use full normalize” **or** push a SQL/Rust-equivalent full normalize in the migration (prefer document for Phase 4 if fixture is empty).

[N4] **`comment_reply` remains uncapped while mentions are braked.**  
Intentional (M1 scoped to mentions). Residual: reply-storm on a popular parent still floods badge (silent toast — good) without hourly drop. Acceptable if parent authors are few; not a ship-blocker.  
**FIX:** Optional later: same hourly cap family for `comment_reply`; not required to build.

[N5] **Reporter floor defaults unpinned in plan text.**  
Fields exist; launch defaults (e.g. age 72h matching D21 young-account, tier 1) are not named — e2e “young/tier-0 fail” needs concrete numbers in config tests.  
**FIX:** Pin example defaults next to `SocialConfig` (even if env-overridable).

## Checkpoints (R2 list) — product/engineering skim

1. **Hot SQL + as_of** — **sound** (trunc + value grid + frozen as_of).  
2. **Lock chain + epoch races** — **sound** (key → author → market → parent/comment; restore/threshold tests listed).  
3. **Materializer cursor** — **sound** (seeded FOR UPDATE, no event SKIP LOCKED, max-seq).  
4. **Fanout cardinalities** — **sound** (terminal facts, anti-join, both-outcomes, void union, 1k gate).  
5. **WS post-commit** — **sound** (honest loss + REST recovery + dedupe fields).  
6. **Identity posture** — **sound** as dev-era (see table).  
7. **Backfills + indexes** — **sound** (CTE/cycle/FK; N3 hash backfill residual only).  
8. **Copy + privacy** — **almost** (notifications.md task complete; scoring.md residual = N1; cast-time + accepted speech stated in plan).

## Residual product judgment

- Brigade brakes are **economically sufficient for Phase 4** given shared-token sybil reality: age/tier floor removes free demo farms; velocity limits report spray; epoch reset makes restore meaningful. Curator load under **qualified** re-brigade remains a human ops cost — acceptable and honestly better than R1.  
- Accepted speech + velocity is the right call; only the **published** residual note lags (N1).  
- Codex identity/`as_of`/epoch choices are product-acceptable with stated honesty and snapshot semantics.

---

FINAL VERDICT: sound-to-build
