VERDICT: FIX-FIRST

1. **B1 identity posture — VERIFIED.** Every comment write now self-asserts an existing `user_id`, viewer identity is token-gated, mark-read is path-user-scoped, and `subscribe_user` replaces the socket's sole user only after token validation and before its snapshot; the shared-token limitation is stated honestly.

2. **B2 hot math/pagination — REGRESSED.** The numeric/truncating SQL twin, exact-value grid, future-time failure, and full `as_of` cursor are pinned, but the recent-500 candidate subquery is not constrained to `created_at <= as_of`, so an insert between pages can displace an old candidate or be scored as future data despite the frozen cursor.

3. **B3 materializer cursor — VERIFIED.** Migration 0006 seeds the cursor; each pump row-locks it, performs a plain ordered event read with no event locks, advances to `max_fetched` in the same transaction, and names all eight requested crash/concurrency/coexistence tests; the separate conflict-target defect is N1.

4. **B4 WS delivery — REGRESSED.** The post-commit boundary, injected commit-to-send crash recovery, typed `BusEvent`, replacement subscription, and `{v:1,id,source_seq}` dedupe fields are present, but a path that deliberately permits zero live deliveries cannot simultaneously promise “at-least-once” live frames.

5. **B5 comment concurrency — VERIFIED.** Posting now follows key → author → market → parent, serialized spam reads precede insertion, vote/report mutate only after a unique insert, non-visible targets reject interaction, and restore deletes reports in the same comment-locked transaction so the next epoch must independently re-cross the threshold.

6. **M1 migration backfills — REGRESSED.** The rooted recursive CTE, cycle/orphan/overflow raise, reply-count aggregate, checks, and same-market composite FK are present, but the SQL body-hash backfill uses only lowercase plus whitespace collapse rather than the NFKC/Cf/control pipeline required for all new hashes.

7. **M2 fanout cardinalities/performance — REGRESSED.** Terminal facts are source-filtered and summed per user, voters are anti-joined, void recipients are unioned, event identities are supplied, and the 1,000-user `<2s` bulk gate is present; however, the promised one row for a holder of both outcomes still contains one undefined `redemption` field even though the two outcomes can have different redemption rates.

8. **M3 query indexes — VERIFIED.** The amended migration adds the requested comment and notification total-order indexes, outcome/cost holder index, market/source realization fanout index, user realization history index, and the existing trade/vote profile indexes; the plan also requires representative `EXPLAIN` checks.

9. **M4 moderation determinism — VERIFIED.** One NFKC/lowercase/Cf-control/whitespace normalization now drives checks and new hashes, raw length is explicitly Unicode-scalar-counted, link matching is pinned to normalized `https?://` tokens, mentions are owned lowercase strings, and golden collision/adversarial-Unicode tests are required; M1 must make legacy hashes use the same rule.

NEW BLOCKERS:

[N1] The stated `ON CONFLICT (user_id, source_seq) DO NOTHING` is not inferable from `notifications_dedupe_uk`, because that unique index is partial (`WHERE source_seq IS NOT NULL`); PostgreSQL rejects the statement with “there is no unique or exclusion constraint matching.” Use `ON CONFLICT (user_id, source_seq) WHERE source_seq IS NOT NULL DO NOTHING`, use targetless `ON CONFLICT DO NOTHING`, or make the constraint non-partial after defining null semantics.

[N2] Freeze the hot candidate set, not only its score clock: every page's recent-500 subquery must include `created_at <= cursor.as_of` with a deterministic `(created_at DESC, id DESC)` limit before calculating and seeking by `(hot_score, created_at, id)`.

[N3] Call post-commit in-process notification frames **best-effort and deduplicable**, not at-least-once. The required crash test proves a legitimate zero-frame execution; snapshot plus REST provides eventual state recovery, not a minimum live-delivery count.

[N4] Migration 0006 must compute legacy `body_hash` with exactly the same canonicalization as `domain::moderation`, or perform an explicitly equivalent application backfill. As written, legacy `HELLO`, NFKC variants, and Cf/control variants can evade author-scoped duplicate detection against new comments.

[N5] Define the aggregated resolution payload for a holder of both outcomes: either `redemption` means total payout and is named accordingly, or emit explicit YES/NO redemption rates (and, if needed, per-side holdings) while retaining one user notification. A singular unqualified redemption cannot be derived uniquely from that cardinality.

FINAL VERDICT: fix-first
