1. **N1 — VERIFIED.** `ON CONFLICT (user_id, source_seq) WHERE source_seq IS NOT NULL DO NOTHING` exactly matches the partial unique-index predicate and is accepted by PostgreSQL.

2. **N2 — VERIFIED.** Every page freezes candidates with `created_at <= as_of ORDER BY created_at DESC, id DESC LIMIT 500`, then scores and seeks by `(hot_score, created_at, id)`, so later inserts cannot displace or future-score rows.

3. **N3 — REGRESSED.** The global notification contract and Task 4.2 correctly say best-effort and deduplicable with snapshot/REST recovery, but the Goal still promises “honestly at-least-once live frames,” which the required zero-frame commit-to-send crash test disproves.

4. **N4 — VERIFIED.** Legacy `body_hash = NULL` is explicitly never matching, so it cannot create false equivalences with the domain's canonical hashes; only the bounded transition can admit one first post-0006 duplicate per author/hash, after which the stored new hash matches, and the finite spam lookback expires the legacy concern.

5. **N5 — VERIFIED.** The single per-user payload now distinguishes aggregate `payout_total_micro` from the market's `redemption_yes_micro` and `redemption_no_micro` rates, retains `realized_delta_micro`, and conditionally adds post-resolution vote score/side, so both-outcome holders are unambiguous.

ADDENDUM VERDICT: fix-first
