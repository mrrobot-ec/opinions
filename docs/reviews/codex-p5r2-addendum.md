1. **N1 frozen inventory — VERIFIED.** The frozen set now explicitly includes `main.rs`, `seed_market.rs`, and the manifest tooling, and 5.5 compares a sorted path inventory for added/removed targets before checking per-file digests over exactly that inventory.

2. **N2 durable issuance — VERIFIED.** Migration 0007 now requires a partial unique issuance key on `(draft_id, kind)` for non-null draft ids across every job status, and the specified crash test covers attachment/failure after enqueue but before the saga persists `jobs_enqueued`.

3. **N3 fenced failures — VERIFIED.** Known render errors and expired-lease reclaim use the same token-fenced ownership rule, fail at `attempts >= video_max_attempts`, and clear claim token plus lease on requeue, making `max=1` exactly one attempt.

4. **N4 expiry and budget authority — VERIFIED.** Review and the bounded ordered sweep share one draft-locked `pending && now >= expires_at → expired` transition with the boundary race test, while a UTC-day advisory lock reserves budget against approved, in-flight, and published drafts before concurrent approvals can pass.

5. **N5 two-stage moderation — VERIFIED.** Migration 0007 adds and cursor-seeds idempotent `moderation_jobs`; the cursor transaction only materializes jobs and advances atomically with zero remote calls, while the separately leased, fenced runner reuses the video-worker protocol for remote preflight and only-tighten escalation.

ADDENDUM VERDICT: sound-to-build
