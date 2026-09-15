fix-first

Adversarial review of Task 5.4 (LLM adapters + moderation escalation runner), round 1. In-scope surfaces only. Static analysis; no cargo/git.

## Findings

[B1] **Stage 2 never reclaims expired `running` leases — `lease_expires_at` is write-only, so a crash or tick-fatal error permanently abandons jobs (fail-open forever).**
`crates/application/src/moderation_escalate.rs:72` and `:118-136`

Claim: The runner documents “a failed tick leaves claimed jobs to lease expiry” and the contract requires the same leasing machinery as video jobs (SKIP-LOCKED claim, token-fenced CAS, **expired-lease reclaim** with the same `>=` attempts boundary). Video’s worker calls `reclaim_expired` before claim. This runner only calls `claim_moderation`. The in-file test double (`:578-581`) claims `status == Queued && available_at <= now` only — it never returns a `running` row whose lease has elapsed. Migration `0007` even ships `moderation_jobs_reclaim_idx` on `(lease_expires_at, id) where status = 'running'`, but nothing in this tick reads it.

Failure scenario: Runner claims a Visible comment (status=`running`, `attempts+=1`, `lease_expires_at=now+lease_secs`). Process is SIGKILL’d during `preflight.screen`, or `process_job` returns `Err` via `?` (see M1) before `complete`. Every later tick’s claim still filters `queued` only. The job stays `running` until an operator rewrites the row. The comment remains Visible with no further LLM pass — the durable second screen is gone. Same outcome for the unprocessed tail of a 16-job claim batch if job 1 is tick-fatal.

Suggested direction: Add a moderation reclaim (port + runner step, or fold expired-`running` into `claim_moderation`) that matches video: SKIP-LOCKED, clear token/lease, `attempts >= video_max_attempts` → `failed` else `queued`. Cover crash-after-claim and expiry-then-retry in the runner suite. Size the lease to the actual work unit (this tick claims 16 jobs then screens them **sequentially** against a 30s transport timeout; default `lease_secs=60` cannot cover the batch if reclaim is honest).

[M1] **`process_job` treats apply/CAS/invariant failures as tick-fatal and skips token-fenced completion, orphaning the rest of an already-claimed batch.**
`crates/application/src/moderation_escalate.rs:132-134`, `:145-147`, `:172-175`

Claim: Screen errors go through `complete` (requeue or terminal). `apply_escalation(...)?`, missing `claim_token`, and `complete`’s own store error do not: they unwind `run_stage`’s `for job in claimed { process_job(...)? }`. Video render *failures* are absorbed into `complete_error`; only the completion tx itself can fail the tick — and video then has reclaim (B1).

Failure scenario: 16 jobs claimed. Job 1 screens Shadow. `apply_escalation` gets a transient `comment_tx` / `commit` error (or `comment_for_update` → `NotFound`). `?` fires; jobs 2–16 stay `running` with live tokens and are never screened on this tick. Combined with B1 they are never screened again. A missing token (`:145-147`) is the same shape: invariant, no `complete`, batch abandoned.

Suggested direction: On apply failure, `complete(..., Some(err), backoff, terminal)` so the fence and attempts boundary still run; do not `?` out of the batch loop for per-job work errors. Reserve tick-fatal for claim/materialize/probe only.

[M2] **Real transport buffers the entire response body with no cap; adapters enforce length only after the download.**
`crates/adapters/src/llm/transport.rs:144-149` (callers: `draft.rs:157-158`, `moderation.rs:98-101`)

Claim: Over-length is a typed error, but `response.bytes().await` materializes whatever the peer sends (any status). Draft then rejects `> 65_536`, moderation `> 4_096`. The 30s client timeout is the only bound.

Failure scenario: `LLM_BASE_URL` points at a buggy or hostile host (compromised vendor, or a URL that is loopback/link-local — `validate_base_url` allows any `http(s)` host). A 200 with a multi-hundred-MB body (or a slow stream for ~30s) is fully buffered in the runner process. If that allocation/timeout hits after Stage 2 has already claimed a batch, B1 leaves those jobs `running` forever. The model is otherwise treated as an untrusted peer; the body should be too.

Suggested direction: Enforce a hard read cap at the transport (stream + take, or reject on `Content-Length` above the adapter max) and return a typed backend error without retaining the tail.

[m1] **`LlmDraftEngine` freezes `ContentConfig::default()` money/participation terms, so env-overridden tier knobs diverge from the HTTP template engine.**
`crates/adapters/src/llm/draft.rs:45-51`, `:93-105`

Claim: The module says LLM drafts “mirror the template engine” so curators review identical starting terms. `LlmDraftEngine::new` always stores `ContentConfig::default()`. The live create path builds `TemplateDraftEngine::new(state.inner.phase5.config)` (env-overridable `DAILY_*` / `FLASH_*` in `main`). Approve re-checks **live** floors against the stored spec.

Failure scenario: Ops sets `DAILY_SEED_MICRO=150_000_000` and `DAILY_SEED_FLOOR_MICRO=150_000_000`. A template draft gets seed 150M and approves. An LLM draft gets default seed 100M; `ReviewDraft::approve` → `validate_floors` → `SeedBelowFloor` until a curator notices and edits. If they approve without noticing a *lower* floor than default, the market is seeded off the stated policy.

Suggested direction: Inject the process `ContentConfig` at engine construction (same `[daily, flash]` slotting as the template engine), or stop claiming term-parity and always stamp terms in the application layer from the live config.

[m2] **`validate_base_url` accepts userinfo; transport errors interpolate the URL, so a password in `LLM_BASE_URL` lands in `StoreError` / job `error` / runner logs.**
`crates/adapters/src/llm/selection.rs:75-93`, `crates/adapters/src/llm/transport.rs:143`

Claim: ApiKey `Debug` is redacted and the Authorization header is sensitive. The configured URL is not. `reqwest::Url::parse("http://ops:s3cret@llm.example")` is a valid http URL with host and no query/fragment, so it is stored and used as the endpoint origin.

Failure scenario: Operator (or a copied `.env`) puts basic-auth in `LLM_BASE_URL`. A connect/TLS failure becomes `StoreError::Backend("llm transport: error sending request for url (http://ops:s3cret@…/v1/moderation): …")`. Stage 2 writes that string into `moderation_jobs.error` (`process_job` `:180-181`) and `main` eprints the tick error.

Suggested direction: Reject userinfo (and consider stripping it from error Display even if kept). Keep scheme/host/optional-path as today.

[m3] **Runner tests never exercise lease expiry or crash-after-claim, so B1 cannot fail CI; `two_concurrent_runners` only proves a mutex-serialized claim.**
`crates/application/src/moderation_escalate.rs:944-983`, `:569-598`

Claim: The suite covers cursor atomicity, only-tighten, unavailable-idle, injection-no-flip, and stale-token *after a manual token overwrite*. It does not advance the clock past `lease_expires_at` on a `running` row and show a second claim. The concurrent test’s `TestVideoTx::claim_moderation` applies under a process mutex with immediate visibility — it cannot miss a reclaim hole.

Failure scenario: A fixer “adds reclaim” in comments but leaves claim SQL/`TestVideoTx` queued-only; every existing test still passes, including `two_concurrent_runners_claim_disjoint_jobs`.

Suggested direction: One test that claims, does not `complete`, advances `now` past the lease, and asserts a later tick reclaims (or, today, documents the gap by failing). That test is the honest counterpart of video’s reclaim suite.

[N1] **`with_env` always `remove_var`s both LLM vars and never restores the pre-test process environment.**
`crates/adapters/src/llm/selection.rs:207-220`

Claim: Tests are serialized on `ENV_LOCK`, but a developer/CI process that started with `LLM_API_KEY`/`LLM_BASE_URL` set loses them after the first `with_env` and for every later test in the crate.

Failure scenario: `cargo test -p adapters` with live keys in the environment; a later test (or a human reusing the process) observes keyless selection.

Suggested direction: Snapshot `var_os` for both names and restore on the way out (including poison-unlock).

## Out of scope notes

- `claim_moderation` SQL in `crates/adapters/src/pg/video_tx.rs` and `VideoTx` lacking `reclaim_expired` for moderation are outside the six surfaces; they are why B1 is unfixable in the runner alone. The unused `moderation_jobs_reclaim_idx` in `0007_content.sql` is the same gap.
- `AVAILABILITY_PROBE` is `""`. Phase 4 `screen` blocks empty bodies before insert, so a stored `""` cannot currently take the local-Visible shortcut. Still a landmine if any future insert path stores empty; an out-of-band probe method would be tighter. Not a present failure.
- Outbox `seq > cursor ORDER BY seq LIMIT 128` can skip a not-yet-committed identity value if a later seq commits first. That is the inherited notifier cursor pattern, not unique to 5.4.
- `apply_escalation` does not emit `CommentShadowed`. Nothing consumes that event today; listings read `comments.moderation_status`. Residual if a later consumer keys on the event.
