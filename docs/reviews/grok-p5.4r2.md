fix-first

Round-2 verification of Task 5.4 against `docs/reviews/p5.4r1-resolution.md` and the post-fix surfaces. Static analysis only.

## Round-1 verification

- **codex B1 / grok B1** — VERIFIED RESOLVED (the stranding hole). `runner_tick` now reclaims before claim (`moderation_escalate.rs:136-138`); PG SQL mirrors video (`pg/video_tx.rs:585-614`, SKIP-LOCKED, `attempts >= max` → `failed`, token/lease cleared). Runner tests `expired_lease_is_reclaimed_and_the_job_screens_again` (`:1079`) and `reclaim_at_the_attempts_boundary_is_terminal` (`:1115`) would fail if claim stayed queued-only: a `running` row at T0+59 stays leased, at T0+61 becomes `Done`/`Failed`. The r1 “size the lease to the work unit” note was **not** done — see new M1 (now live *because* reclaim exists).
- **grok m3** — VERIFIED RESOLVED. Those two tests are the honest counterpart of video’s reclaim suite; a comments-only reclaim would not pass them.
- **grok M1** — VERIFIED RESOLVED. `job_work` (`:162-186`) absorbs read/screen/apply errors into token-fenced `complete`. The batch loop (`:146-155`) keeps going and records `first_error`. `per_job_read_failure_requeues_fenced_and_the_batch_tail_still_completes` (`:1017`) asserts tick **Ok** + one requeued + one Done — the old `?` unwind cannot pass. `completion_failure_is_contained_to_its_job_and_still_fails_the_tick` (`:1152`) asserts both bodies screened and only one row left `running`.
- **codex M1 / grok M2** — VERIFIED RESOLVED. `HttpRequest.max_response_bytes` is on the wire contract. Real transport rejects `content_length() > cap` before `chunk()` (`transport.rs:149-154`) and streams with `body.len() + chunk > cap` (`:163-166`), never extending past the cap. `oversized_declared_content_length_is_rejected_before_the_body_is_read` (`:414`) would return `Ok` empty if they still called `bytes()` on a header-only 1_000_000 CL. `overlong_undeclared_response_stream_is_capped_mid_read` (`:438`) has no CL and 64 body bytes vs cap 8 — only the streaming branch produces that error string. Adapters still pass their caps and keep post-parse `>` checks. `== cap` is coded as pass (`>` not `>=`) but not unit-tested (N1).
- **codex M2** — VERIFIED RESOLVED. `resolve` builds `Bearer {key}` via `HeaderValue::from_str` (`selection.rs:71-72`) and returns `UnencodableApiKey` (message-only). `header_invalid_key_env_poisons_both_engines_including_the_probe` (`:330`) fails the empty probe, so an enabled runner never reaches claim.
- **grok m1** — PARTIALLY RESOLVED (escalated, as claimed). `LlmDraftEngine::new` still hardcodes `ContentConfig::default()` (`draft.rs:45-51`). Interim “approve re-validates floors” **is real**: `ReviewDraft::approve_as` → `validate_for_publish` → `validate_floors` against live `tier_config` (`review_draft.rs:126-127`, `:179-185`). The resolution’s “never as silent bad terms reaching a market” is **overstated**: lowering `DAILY_SEED_MICRO` while leaving the default floor (1e6) still lets an LLM draft at default seed 1e8 approve. Only a *raised floor* above 1e8 is loud.
- **grok m2** — VERIFIED RESOLVED. Userinfo (username or password) rejects as `MalformedBaseUrl("userinfo is not allowed")` (`selection.rs:96-101`). `malformed_base_urls_are_typed_config_errors` includes `http://ops:s3cret-cred@llm.test` and asserts the secret is absent from `error.to_string()`.
- **grok N1** — VERIFIED RESOLVED. `with_env` snapshots `var_os` and restores after the body (`selection.rs:223-244`). `with_env_restores_the_pre_test_process_environment` drops `ENV_LOCK` before the nested call (no deadlock) and checks the pre-existing key survived.

## New findings

[M1] **Reclaim made the r1 lease-vs-batch note live: two runners + default knobs can steal an in-flight sequential batch and terminal-fail healthy jobs.**
`crates/application/src/moderation_escalate.rs:43-45`, `:130-141`, `:146-155`; `crates/adapters/src/llm/selection.rs:29`

Claim: Stage 2 still claims **16** jobs under one `lease_secs` (default 60), then screens them **sequentially** against a hardcoded 30s transport timeout. A single runner is safe (reclaim only runs at the *next* tick, after the loop). A second runner’s tick — the topology `two_concurrent_runners` and the video twin advertise — will reclaim any `running` row whose lease has elapsed, even though the first worker still holds that row in its local `claimed` vec.

Failure scenario: Two processes with `MODERATION_RUNNER_ENABLED` (default `SCHEDULER_TICK_MS=1000`, `CONTENT_LEASE_SECS=60`, `VIDEO_MAX_ATTEMPTS=3`). Runner A claims 16 Visible jobs at T0 (`attempts=1`, lease T0+60) and starts screening. Each screen can take up to 30s, so jobs 3–16 are still `running` at T0+60. Runner B’s tick reclaims them (`attempts` stays 1) and immediately re-claims (`attempts=2`, new tokens). A’s later `complete` is a stale-token drop (CAS still airtight — not the bug). B is itself sequential, so at T0+120 a third claim generation reaches `attempts=3`. At T0+180 the next reclaim sees `attempts >= 3` and flips the tail to **`failed`** while a worker is still screening. Those comments stay Visible and will never be retried. Stale-token drop is correct; the **lease is shorter than the work unit**, so reclaim now manufactures the terminal boundary.

Suggested direction: Claim one (or a few) jobs per tick, **or** set lease ≥ `CLAIM_BATCH * TRANSPORT_TIMEOUT` (and document it), **or** renew the fence per job. Add a two-clock test: claim 3, do not complete, advance just past lease, second tick must **not** terminal-fail a still-legal in-flight generation under default max_attempts.

[m1] **`moderation_jobs_contract` does not pin the `lease expired` error coalesce; the fake omits it and still passes.**
`crates/application/src/fakes/video.rs:545-552`; `crates/application/src/contract/video.rs:868-880`; contrast `crates/adapters/src/pg/video_tx.rs:596-597`

Claim: Resolution says terminal reclaim coalesces `error` to `lease expired`. PG does (`coalesce(error, 'lease expired')`). `InMemoryStore` only flips status and clears token/lease — `error` stays unset. The new contract section asserts counts, live-lease untouched, limit 1+1, re-claim attempts==2, and boundary → no further claim. It never reads `error`. `TestVideoTx` (`moderation_escalate.rs:657-661`) matches the fake, so runner boundary tests also cannot catch a SQL coalesce regression.

Failure scenario: A later consumer (ops query, dashboard, or a test against the fake) keys on `error = 'lease expired'` after a max-attempts reclaim. Fake/CI: NULL. Postgres: `'lease expired'`. Conversely, deleting the SQL `coalesce` still leaves `moderation_jobs_contract` green on both backends.

Suggested direction: On terminal reclaim, fake and `TestVideoTx` should `error = error.or("lease expired")`. Contract: after the max=2 reclaim, load both rows and assert that string (and that a pre-existing error is kept). Optionally stagger two lease expiries and assert the earlier one is the `limit=1` hit (ordering is implemented the same in fake and PG today, but the contract never constructs unequal expiries).

[m2] **Reclaim is inside the probe gate, so a post-crash keyless/misconfig restart leaves `running` rows stranded until a configured engine returns.**
`crates/application/src/moderation_escalate.rs:84-86`, `:136-138`

Claim: `materialize_stage` runs, then `screen(AVAILABILITY_PROBE)?`, then `run_stage` (the only reclaim call). Unavailable/misconfigured preflights fail the probe on purpose so jobs are not *claimed*. That also skips reclaim.

Failure scenario: Configured runner claims a batch and dies. Ops unsets `LLM_*` (or ships a header-invalid key) and restarts. Every tick materializes new `queued` jobs, then probe-fails, and never reclaims. The crashed rows stay `running` for the whole keyless window. Turning the engine back on recovers them (unlike r1). Permanent keyless after a crash never recovers those rows.

Suggested direction: Reclaim after materialize and *before* the probe; keep claim probe-gated.

[N1] **No exact-cap assertion (`== cap` must succeed, `cap+1` must fail).**
`crates/adapters/src/llm/transport.rs:149-166`

Claim: The inequalities are `>` (correct). Tests use CL=1_000_000 vs cap 8, and 64 undeclared bytes vs cap 8. An accidental `>=` on the stream check would still pass both.

Failure scenario: A later edit writes `>=` and a legitimate 4_096-byte moderation body (exactly `MAX_RESPONSE_BYTES`) starts failing in production while CI stays green.

Suggested direction: One loopback: body of exactly `cap` bytes, 200, `Ok`; one more byte, typed cap error.

## Out of scope notes

- `Url::parse` failures still interpolate `error.to_string()` (`selection.rs:86-87`). A userinfo URL that *fails* to parse (empty host) might echo the secret; not proven against rust-url’s message text.
- `apply_escalation` still emits no `CommentShadowed`. Nothing consumes one.
- grok m1 remains escalated on frozen `main.rs` wiring; do not treat the “never silent” sentence as a closed safety property.
