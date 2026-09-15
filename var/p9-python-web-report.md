# P9 Python converse + Next.js PWA audit/fix report

## Outcome

The owned Python and web surfaces were audited across dimensions C/E/F/G/H/I and repaired only where a red regression reproduced the defect. The agent-free money corridor remains structurally intact, while webhook authentication, production startup, PostgreSQL concurrency, browser DTO normalization, withdrawal safety, PWA cache isolation, money/share units, user-facing copy, test arming, and vulnerable direct dependencies were strengthened. No VCS command was run.

The final owned unit gates are green: converse `43 passed`, web `80 passed`, TypeScript clean, Next 16.3.0 production build green, Python dependency audit clean, and pnpm reports no known vulnerabilities. The live Playwright fixture and one cross-owner static-quality assertion are explicitly dispositioned in the verification section below.

## Authority and scope reviewed

Read before findings:

- `docs/spec.md`, `docs/decisions.md`
- relevant Phase 0–8 plans, with emphasis on Phase 7 money/compliance and Phase 8 converse/web contracts
- relevant `docs/reviews/*.md` and resolutions
- current `openapi.json`, generated Python models, Rust HTTP DTO/route composition (read-only), migrations/schema (read-only), package scripts, all `services/converse/**` Python/config/tests, and all `web/**` source/config/tests

Threat paths checked:

- Sendblue webhook -> provider authentication -> global inbound dedupe -> per-thread lock -> lexical pre-router -> pending preview -> exact confirmation -> Core execute -> consume-after-success -> causal recorder
- pending expiry/replacement, stale config 409/re-preview/new confirmation, request fingerprint and replay behavior, duplicate webhook races, connection-pool saturation, startup failures, and exception mapping
- PWA login/device state -> REST/WS clients -> market vote/trade -> portfolio/profile/notifications -> withdrawal/KYC/self-exclusion -> offline service worker
- API/DTO and route/OpenAPI alignment; sell/buy units; micro-money parsing and display; refusal envelopes; terminal state; SSR/client trust; XSS/CSRF/cache/header posture; reconnect behavior; polling/unbounded work; dependency/runtime and generated-model drift
- verification quality: tautologies/assert-nothing, over-mocking, environment skips, live-fixture arming, and dependency/build gates

## Runtime and baseline

Versions:

```text
Python 3.12.12
uv 0.11.20
datamodel-codegen 0.64.0
Node v26.5.0
pnpm 9.15.4
Next.js v16.3.0
Playwright 1.55.1
```

Initial baselines before the new regressions:

```text
converse: 38 passed, 1 warning in 0.84s
web:      12 files passed, 64 tests passed
```

The Python warning was the existing FastAPI/Starlette `httpx` TestClient deprecation. Vitest also emitted Node's experimental localStorage warning; neither was suppressed.

## TDD defect ledger

### 1. Production turn locks could starve their own recorder

Impact: the production `PgPendingStore.turn()` held a connection while graph recording tried to acquire another from the same saturated pool, allowing a self-deadlock under eight concurrent turns.

Red:

```text
FAILED tests/test_app.py::test_production_isolates_turn_locks_from_recorder_connections
E assert 1 == 2
```

Fix: production now creates distinct bounded turn and recorder pools, wires each owner to its pool, and closes both. Green focused result: `2 passed` (with the existing TestClient warning); final PostgreSQL-armed suite: `43 passed`.

### 2. Concurrent webhook redelivery could violate recorder foreign keys

Impact: the authoritative dedupe read happened before the per-turn advisory lock. A real PostgreSQL race let both deliveries miss; one `agent_runs` insert lost its uniqueness conflict and its later `agent_steps` insert referenced a non-existent run.

Exact real-PostgreSQL red excerpt:

```text
AssertionError: [ForeignKeyViolationError('insert or update on table "agent_steps" violates foreign key constraint "agent_steps_run_id_fkey"'), <Response [200 OK]>]
```

Fix: the dedupe lookup now runs inside `pending_store.turn`, under the same transaction/advisory-lock boundary as the turn. The retained real-PostgreSQL regression forces two concurrent outer misses and proves both responses converge; final armed suite: `43 passed`.

### 3. Sendblue webhook authentication was absent and production could start unarmed

The official provider contract puts the configured secret in `sb-signing-secret`; see [Sendblue webhook security](https://docs.sendblue.com/docs/webhooks/).

Observed red excerpts:

```text
configured secret, missing header: expected 401, received 200
production missing secret: AssertionError: database resources started before auth validation
```

Fix: compare the configured secret with `secrets.compare_digest`, reject missing/wrong signatures with 401, require `SENDBLUE_WEBHOOK_SECRET` for DB-backed startup, and validate it before allocating pools/checkpointers. Injected unit apps remain independent of the production requirement. Focused green: `2 passed`; final: `43 passed`.

### 4. Default production Core auth silently used a literal demo token

Red:

```text
AssertionError: database resources started before core auth validation
```

Fix: DB-backed startup without an injected Core client now requires `DEMO_TOKEN` before allocating resources; no literal fallback is accepted. Focused green: `1 passed`; final: `43 passed`.

### 5. Web request/DTO contracts drifted from the Core authority

The first targeted web red run was:

```text
Tests  5 failed | 9 passed (14)
```

It reproduced all of the following:

- comment-vote POST omitted the required idempotency key;
- notifications returned Core's `{notifications, unread_count}` while the bell expected `{items, unread_count}`;
- withdrawal omitted `confirm_dest` and generated no request key;
- the withdrawal page had no destination re-entry boundary;
- the service worker did not exclude same-origin `/core-api`.

Fixes: add comment-vote idempotency; normalize notifications; align withdrawal request/receipt and refusal fields; require exact destination re-entry; and exclude `/core-api` from service-worker caching. Focused green: `14 passed`.

A second DTO/unit/copy red run was:

```text
Tests  5 failed | 26 passed (31)
TypeError: formatShares is not a function
TradePanel sell label missing
market detail missing "Settled P&L"
profile response missing normalized avg_score_bp / markets_scored / market reference
expected "Paid out $1.50 ..."; received "Paid out $0.00 on 'market'"
```

Fixes: normalize nested `UserProfileDto.voter` and `market_ref`; support the notifier's emitted `realized_delta_micro`; render micro-shares without `$`; make sell controls share-aware; and label `realized_pnl_micro` honestly as settled P&L. Focused green: `31 passed`.

### 6. Phase 7 client routes/refusal/cache namespace were stale

Observed red contract excerpts:

```text
withdraw refusal: expected code "kyc"; received "http_error"
self exclusion: expected URL containing "/self_exclusions"; received "/me/self-exclusion"
service worker: expected "opinions-shell-v2"; received "opinions-shell-v1"
```

Fix: consume `refuse_code`/`refuse_message`, post self-exclusion to `/self_exclusions`, and bump the shell namespace to v2 so activation purges any legacy cache that could contain Core responses. Focused web green: `12 passed`.

### 7. Withdrawal retries, parsing, and refusal copy could move or describe money incorrectly

The coordinator's adversarial follow-up was reproduced before implementation:

```text
Test Files  2 failed (2)
Tests  4 failed | 12 passed (16)

expected '1743f1ce-0fb7-4e58-b52f-ac6a38e53c74'
to be '59b23b95-ac3e-4837-b504-af0309a80707'
TypeError: (0 , parseUsdMicros) is not a function
TypeError: (0 , withdrawReceiptCopy) is not a function
expected page source not to contain 'Math.round(Number(amount) * 1_000_000)'
```

Impact and fix:

- An ambiguous network failure followed by an unchanged retry generated a fresh key, so it could create a second hold. `requestWithdraw` now retains one bounded key for the active `(user, amount, dest, confirm_dest)` fingerprint across ambiguous transport failure. It rotates when a semantic field changes and clears after a definitive receipt or typed HTTP refusal, allowing a later intentional identical withdrawal to be a new attempt.
- Binary `Number` plus `Math.round` accepted exponent/non-finite shapes and rounded over-precision. `parseUsdMicros` now accepts only a positive canonical decimal with at most six places, bounds input/work, converts using integer arithmetic, and rejects values outside exact JS integer range.
- Every non-null receipt said “accepted,” even when `refused=true` and `id=null`. Refusal-aware copy now displays `refuse_code` and `refuse_message`; accepted copy is only used for non-refused receipts.

Focused green:

```text
Test Files  2 passed (2)
Tests  16 passed (16)
```

A follow-up proved that retaining the key after success would instead replay the first hold forever:

```text
money client > rotates the withdrawal key after a definitive successful receipt
expected 'e5742420-2e31-4856-9356-c35da53b5cac'
not to be 'e5742420-2e31-4856-9356-c35da53b5cac'
Tests  1 failed | 11 passed (12)
```

The request client now clears only the matching active attempt after a definitive receipt or `ApiClientError`, while preserving it for ambiguous transport exceptions. Focused green: `12 passed`; final web suite: `80 passed`.

The first exact parser implementation also exposed a real build-target regression:

```text
lib/money.ts(60,26): error TS2737: BigInt literals are not available when targeting lower than ES2020.
lib/money.ts(61,17): error TS2737: BigInt literals are not available when targeting lower than ES2020.
```

Replacing ES2020 literal syntax with the `BigInt(...)` constructor preserved exact arithmetic under the project's ES2017 target; `tsc --noEmit` and the final Next build are green.

### 8. Skip-as-green integration suites

The shared regression initially reported these exact owned violations:

```text
services/converse/tests/test_app.py:207: pytest.skip
services/converse/tests/test_pg_stores.py:16: pytest skipif
services/converse/tests/test_recorder.py:46: pytest.skip
web/e2e/mobile-golden.spec.ts:10: Playwright test.skip
web/e2e/money-surfaces.spec.ts:6: Playwright test.skip
```

Fix: all three PostgreSQL tests and both Playwright suites now fail loudly with a precise missing-arm error. PostgreSQL is armed in the final suite. Playwright `--list` with all arms set discovers exactly two live tests; no environment skip remains.

### 9. Vulnerable pinned tooling/runtime dependencies

Exact initial audit summaries:

```text
pnpm audit: 6 vulnerabilities found
Severity: 2 moderate | 4 high

pip-audit: Found 9 known vulnerabilities in 1 package
datamodel-code-generator 0.28.5 ... fixes through 0.64.0
```

Direct, coordinator-approved remediation only—no override/resolution trick:

- Playwright `1.55.0 -> 1.55.1` removes GHSA-7mvr-c777-76hp.
- `datamodel-code-generator 0.28.5 -> 0.64.0` removes its nine advisories.
- Next `15.5.23 -> 16.3.0` is the earliest direct published Next release whose own declared dependencies use patched PostCSS `8.5.23` and sharp `^0.35.3`.

The generated `api_models.py` was regenerated, not hand-edited. Its syntax modernized (`list`, unions, `AwareDatetime`), while executable equivalence checks compared all `69` BaseModel JSON schemas and all `6` Enum contracts with zero differences. Final audits:

```text
pnpm audit --prod --audit-level=low: No known vulnerabilities found
pip-audit: No known vulnerabilities found
  (local converse 0.1.0 reported only as not present on PyPI)
uv lock --check: Resolved 64 packages
```

## Dimension review and dispositions

### I — agent/money safety

- The model/router cannot execute trades or votes. Lexical confirmation is checked before the router; only normalized explicit allowlist confirmations can consume the pending action. Emoji/tapbacks and substantive text do not confirm; substantive text clears/routes.
- The graph carries the authoritative Core preview's amounts/config version; it does not compute price, fee, shares, collateral, or payout. Execute sends the stored values/expected version and consumes only after Core success.
- A 409 stale-config response expires the old pending action, re-previews, and requires a new lexical confirmation.
- Stable pending/action/run identifiers and Core idempotency retain replay protection. The global webhook dedupe race is now inside its lock.
- Recorder state exists for every graph step, but the residual causal-audit limitation below remains.

### C — correctness/contracts

- Verified request/response field shapes for market, social, notification, profile, withdrawal, and generated Python clients against OpenAPI and source DTOs.
- Repaired buy-vs-sell units, profile nesting/reference normalization, notification delta key, settled-P&L wording, and withdrawal route/body/receipt drift.
- `_fmt_usd` uses float only for bounded display formatting. No in-range failing test was found, so the hypothesis was dropped rather than churned.

### E — security/trust

- Webhook provider secret and production Core token now fail closed before resources allocate; provider comparison is constant-time.
- No `dangerouslySetInnerHTML` exists (retained `noDangerousHtml` test); React text rendering supplies the XSS boundary.
- The browser API uses explicit header tokens rather than cookies, so cookie-CSRF was not an active request path. Tokens/localStorage are explicitly a development session mechanism, not a production auth claim.
- Same-origin Core API is excluded from PWA caching and the namespace bump purges prior entries.
- No project-level CSP/security-header authority was found. This is recorded as deployment hardening, not manufactured as an owned defect without a launch contract.

### F — errors/recovery/offline

- Typed Core errors/refusal envelopes retain their code/message; malformed non-JSON errors become a typed parse error instead of being swallowed.
- Withdrawal retry identity survives ambiguous failures; refusal and accepted results are distinct.
- Service-worker API requests always go to network; no stale money/social response can be served offline. The shell remains cacheable.
- No fixed-interval social polling was found; updates are REST bootstrap plus event-driven WS/debounce.

### G — concurrency/performance/bounds

- Separate bounded Pg pools remove the turn-lock/recorder circular wait; dedupe is serialized in the transaction boundary.
- Amount parsing rejects long inputs before BigInt conversion; social caches and displayed collections are bounded by existing limits.
- A stale-old-WebSocket `onclose` handler potentially nulling a newer connection was inspected but not claimed: no new failing regression was completed, so the hypothesis remains deferred under the mandatory TDD rule.

### H — verification/runtime

- New regressions exercise real PostgreSQL concurrency and production wiring, not only fakes.
- All owned skip constructs were replaced with fail-loud arming. No test, threshold, type rule, or coverage rule was weakened.
- Direct dependency upgrades made both audit commands green; generated API schema/enum equivalence is proven.
- Package scripts inspected: `test`, `test:e2e`, and `build`; there is no lint script. TypeScript was run explicitly.

## Rejected hypotheses / review dispositions

- Display-only `_fmt_usd` floating point: dropped after no failing supported-range case.
- LLM monetary calculation/execution: refuted by control/data-flow inspection and malicious-router corridor tests.
- Raw HTML/XSS sink: refuted by source scan plus `noDangerousHtml` test.
- Fixed polling/unbounded notification loop: refuted; the owned path is event-driven with debounce.
- Service-worker API caching after the fix: refuted by path exclusion plus cache namespace regression.
- Environment skips as “normal CI behavior”: rejected by the new shared quality authority; all owned suites now fail loudly.
- Top-level runtime dependencies patched via overrides: rejected; only direct published package versions were used.

## Residual risk / cross-owner blockers

These are evidence-backed integration risks found while reading outside ownership. They were not edited here and are not presented as fixed owned bugs:

1. Core trade/vote HTTP handlers do not consume a region header, and production converse user lookup does not obtain a KYC region. The converse `x-user-region` client test therefore does not prove the Phase 7 geography gate at the HTTP boundary.
2. The public compliance router defines `/self_exclusions` and related endpoints but was not merged into the inspected Core router composition. Public balances and `/kyc/start` were also absent; the web money surface cannot pass a live Phase 7 run until Core mounts/implements those authorities.
3. Current OpenAPI omits several Phase 7 public money routes, including the mounted withdrawal path, leaving cross-language drift risk despite the regenerated client being schema-equivalent to the current file.
4. Notification payloads do not include `question`; the PWA must fall back to “market” unless Core enriches the event or a further fetch is added.
5. Market REST summary lacks terminal viewer vote/redemption details; if WS state is unavailable after reload, the terminal UI can show unknown values.
6. Viewer vote state is not restored after reload. A returning voter can remain trade-locked in the PWA even though Core safely refuses a duplicate vote.
7. Reputation fee display uses static client assumptions and can diverge from a live config override because the effective configuration is not exposed to this surface.
8. Recorder rows preserve state transitions but do not yet retain the full rendered prompt/model parameters/rationale needed for the strongest D18 causal reconstruction claim.
9. The existing Starlette TestClient deprecation and Node localStorage/module-register warnings remain unsuppressed maintenance items.

## Exact changed deliverables

Converse:

- `services/converse/pyproject.toml`
- `services/converse/uv.lock`
- `services/converse/src/converse/app.py`
- `services/converse/src/converse/api_models.py` (regenerated)
- `services/converse/tests/test_app.py`
- `services/converse/tests/test_pg_stores.py`
- `services/converse/tests/test_recorder.py`

Web:

- `web/package.json`
- `web/pnpm-lock.yaml`
- `web/tsconfig.json` and `web/next-env.d.ts` (Next 16 migration output)
- `web/app/components/TradePanel.tsx`
- `web/app/m/[slug]/page.tsx`
- `web/app/u/[id]/page.tsx`
- `web/app/withdraw/page.tsx`
- `web/lib/api.ts`, `web/lib/copy.ts`, `web/lib/format.ts`, `web/lib/money.ts`, `web/lib/types.ts`
- `web/public/sw.js`
- `web/lib/__tests__/api.test.ts`
- `web/lib/__tests__/format.test.ts`
- `web/lib/__tests__/money.test.ts`
- `web/lib/__tests__/moneySafetyBoundaries.test.ts` (new)
- `web/lib/__tests__/social.test.ts`
- `web/e2e/mobile-golden.spec.ts`
- `web/e2e/money-surfaces.spec.ts`

Backups for risky edits are under `var/backup/<same-relative-path>`. Generated `.next`, `.pytest_cache`, `__pycache__`, dependency directories, and TypeScript build cache are not deliverables.

## Final verification

```text
DATABASE_URL=postgres://opinions:opinions@localhost:15434/opinions_p9_038a_web \
SENDBLUE_WEBHOOK_SECRET=test-signing-secret .venv/bin/pytest -q
43 passed, 1 warning in 1.28s

pnpm test
13 files passed; 80 tests passed

pnpm exec tsc --noEmit
exit 0

pnpm build
Next.js 16.3.0; compiled successfully; TypeScript complete;
12/12 static pages generated; exit 0

pnpm audit --prod --audit-level=low
No known vulnerabilities found

uvx pip-audit --path .venv/lib/python3.12/site-packages
No known vulnerabilities found (local converse package not on PyPI)

uv lock --check
Resolved 64 packages; exit 0

PLAYWRIGHT_* armed pnpm exec playwright test --list
2 tests in 2 files
```

Shared/live final dispositions:

- `python3 -m unittest scripts/test_test_quality.py`: owned skip-detection portion green; last observed full run remained red only on cross-owner `crates/adapters/src/http/middleware.rs:795` tautological assertion.
- `pnpm test:e2e`: the unarmed command exited 1 with `PLAYWRIGHT_MARKET_SLUG is required for the live mobile Playwright contract`, `PLAYWRIGHT_USER_ID is required for the live money Playwright contract`, and `No tests found`. Live execution requires an already-running seeded Core/web fixture because this task forbids Cargo and no service was listening; no test was skipped or reported green in its absence.

## Dispatch lifecycle

The initial recovered dispatch (`ctx_9c3b793fda57`) received exactly one `worker_done` attempt, which Orca recorded as rejected because that terminal context had lost its Dispatch capability; it was not retried. The coordinator then created fresh dispatch `ctx_221778172686` with valid lifecycle authority so this completed artifact and the already-green owned work could be revalidated and settled authoritatively.
