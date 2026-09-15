VERDICT: FIX-FIRST

1. **B1 matrix/freeze — REGRESSED.** The wave-owned sets and most shared surfaces are now explicit, 0007b is gone, and a coordinator-only SHA-256 re-manifest barrier is a credible no-VCS freeze check, but `main.rs` and the 5.0-modified `seed_market.rs` are absent from the declared frozen set, so the manifest's target inventory is still incomplete (N1).

2. **B2 skeleton — VERIFIED.** Task 5.0 now predeclares final traits/types, adds `StoreError::Unavailable(&'static str)`, behavior-asserts every placeholder branch, tests disabled/unavailable startup composition, and requires the full 100%-coverage barrier before releasing the wave.

3. **B3 publication saga — REGRESSED.** Persisted stages, a pre-generated market id, the SeedMarket stored-id replay fix, one sweep/publish-now authority, and boundary crash/race tests close the market-lifecycle replay hole, but job issuance is not durably idempotent across the enqueue→stage gap once the active-only unique stops applying (N2).

4. **B4 job leasing — REGRESSED.** Short SKIP-LOCKED claims, render-outside-tx, fenced success CAS, expired reclaim, and the exact queued/rendering/ready partial unique are pinned, but known render failure has no fenced transition and `attempts > video_max_attempts` permits more attempts than the configured maximum (N3).

5. **B5 assets/share cards — VERIFIED.** Separate market poster/video columns flow through list/detail DTOs, snapshots, and a versioned sequenced asset frame with explicit null-placeholder semantics, while per-user share cards are removed from `video_jobs` and content-addressed by user, market, realization digest, and spec version.

6. **B6 async moderation — REGRESSED.** The remote decision is moved off the comment transaction and the application alone enforces visible→shadow, never-unshadow/never-block, but “the proven cursor pattern” cannot both hold its cursor transaction through processing and run the remote preflight outside every lock; no alternative durable handoff protocol is specified (N5).

7. **M1 split proof/isolation — REGRESSED.** Per-worker targets/databases, scoped tests, and barrier-only workspace gates make the wave operationally isolated, but the pure-move manifest covers only a `pub fn|struct|enum|trait` projection rather than the requested normalized source/API surface, omitting public aliases, constants, statics, re-exports, modules, macros, and non-public behavior.

8. **M2 slotting/TTL/admission — REGRESSED.** Strict UTC boundaries, non-divisor daily arithmetic, ordered slot reservation, persisted expiry, horizon failure, and advisory-locked pending admission are now pinned, but expiry has no exact predicate/sweep/race authority and the published-only daily budget check does not reserve budget against concurrent approvals (N4).

9. **M3 renderer — VERIFIED.** RenderSpec v1 removes maps/floats/clock/locale-dependent money and measurement, pins escaping, wrapping, attribute order, and newlines, asserts exact bytes plus digest across fresh instances, and honestly limits the contract to SVG bytes rather than viewer-font rasterization.

10. **M4 artifact safety — VERIFIED.** UUID/kind-derived names, canonical-root temp writes with fsync and atomic rename, job-and-status lookup rather than path input, restrictive SVG headers, finalized-only immutable caching, and traversal/symlink/partial-write tests close the original crash and serving paths.

11. **M5 transport confinement — REGRESSED.** Mandatory transport injection, one audited real constructor, request-shape assertions, deny transports, and all-or-none credentials close the network/injection gaps, but malformed or unsafe `LLM_BASE_URL` validation and its typed startup-failure tests remain unstated.

NEW BLOCKERS:

[N1] The frozen manifest has no canonical target inventory that includes every 5.0-touched shared file; in particular, `main.rs` is pre-wired and `seed_market.rs` receives the replay fix but neither is named in the frozen ownership paragraph. **FIX:** have 5.0 emit and compare a sorted frozen-path manifest, include both files (plus the manifest-generating script/config), hash exactly that inventory, and make any added/removed target fail before digest verification.

[N2] Publication job enqueue is called idempotent only by the partial unique over `queued|rendering|ready`. If enqueue commits and the publisher dies before persisting `jobs_enqueued`, a worker may attach or fail that job; retry then sees no active conflicting row and creates a second canonical job. **FIX:** add a durable issuance key unique across all statuses (for example `(draft_id, kind)` or `draft:<id>:job:<kind>`) or atomically enqueue and advance the stage in one transaction, and test attach/fail between enqueue commit and stage persistence.

[N3] The leasing state machine does not define a token-fenced error completion. With attempts incremented on claim, `attempts > video_max_attempts` gives `max=1` a second claim, while an immediate renderer error otherwise remains `rendering` until lease expiry. **FIX:** CAS every error on `(id, claim_token, rendering)`, use `attempts >= video_max_attempts` to fail, otherwise queue with the pinned backoff and cleared lease/token, and apply the same boundary on expired reclaim.

[N4] Persisting `expires_at` does not itself expire anything: the revised 5.1 flow omits the exact `now >= expires_at` transition, the pending review/expiry row-lock race, and a bounded expiry sweep. Its approval-time budget reads only the day's already-published seed, so concurrent approvals can each reserve a slot and later exceed the cap. **FIX:** add one draft-locked expiry authority used by review and sweep, and reserve the target UTC day's budget under a day-scoped lock/ledger by counting every approved/in-flight/published reservation, with concurrency tests.

[N5] A Phase-4 cursor transaction locks the cursor, reads inputs, writes effects, and advances atomically; inserting an LLM call “outside any lock” breaks that protocol. Advancing first loses moderation on a crash, while advancing afterward without a durable claim permits unbounded duplicate calls and has no stated multi-pump ordering/fencing rule. **FIX:** materialize idempotent moderation jobs in the ordinary cursor transaction, then lease/fence those jobs for remote preflight and CAS `EscalateComment`, or specify and test an equivalent durable two-stage inbox protocol.

FINAL VERDICT: fix-first
