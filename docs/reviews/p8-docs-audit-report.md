# Phase 8 — learning-site audit report

Every page of `docs-site/` was audited against the actual source (`crates/domain`,
`crates/application`, `crates/adapters`, `crates/main`, `crates/simswarm`,
`migrations/0001..0011`, `services/converse`, `web/`, `justfile`, `docs/spec.md`,
`docs/decisions.md`, `docs/copy/ops.md`, `docs/plans/*`, `docs/reviews/*`).

**Result: 26 pages (was 19), ~36.7k words (was ~14k), 28 mermaid diagrams (was 12).**
`mkdocs build --strict` exits 0 with zero warnings, now with link **and anchor** validation
turned on. `scripts/check_frozen_manifest.py` still passes — no frozen file was touched.

---

## 1. Factual corrections made

Ordered roughly by seriousness. Each was verified in the file named.

### Materially wrong

| # | Claim in the draft | Reality | Source |
|---|---|---|---|
| 1 | "A user balance cannot go negative — enforced *inside PostgreSQL*"; "enforced by the database itself, not merely by the code" | **There is no such database constraint.** Balances are derived by summing entries; there is no balance column. Non-negativity is enforced by `domain::ledger::Balances::apply` and re-validated in the Pg adapter under `FOR UPDATE` account locks. | `crates/adapters/src/pg/trade_tx.rs:262` `validate_posts`; no matching constraint in any migration |
| 2 | "**Voided** — everyone gets their money back / everyone is refunded / nobody wins or loses" | A void is a **neutral settlement at 5,000 bps**: every share redeems at 50¢. A buyer at 70¢ *loses*. Historical-replay refunds were explicitly rejected (not non-negativity-safe). | `resolve_market.rs` `decide()` → `(true, 5_000)`; `docs/decisions.md` D21 |
| 3 | "the house never takes a side against you" | The house **is** the counterparty: it seeds every pool, redeems the pool's leftover inventory at settlement, and books an LP P&L per market, with an automatic kill switch on realised LP loss. | `resolve_market.rs` `set_lp_result`; `LpKillConfig` in `model.rs` |
| 4 | Invariants listed as 7 + 4 "Phase 7 additions", with invented names | There are exactly **14** named identities, in a fixed order. The draft's "market pots reconcile" and "withheld matches withdrawals" descriptions were both wrong; four withdrawal identities were collapsed into one. | `application/src/integrity/invariant_sweep.rs` (asserted list of 14 in its own test) |
| 5 | "a human is paged" (invariant breach, ledger drift) | The sweep runs **only under `--continuous`** and `eprintln!`s the violation. The incident system exists (durable outbox, dedup, ack/resolve, 1-min delivery pump, named detectors incl. `invariant_breach`) but **no production path calls `raise` with those detectors**, and the wired alerter is `RecordingAlerter` (in-memory `Vec`). Rewritten to state intent vs. reality. | `main.rs:680-697`; `ops/alerts.rs`; `money_ports.rs:14` |
| 6 | `ADMIN_TOKENS_JSON='{"local-admin":{"role":"superadmin"}}'` | Wrong shape entirely. It is a JSON **array** of `{id, roles:[…], sha256:"<64 hex>"}`, strictly validated (empty array, empty id, no roles, repeated role, duplicate id, duplicate digest, bad hex → startup error). Replaced with a working example plus the `shasum` command to generate it. | `adapters/src/http/middleware.rs:108` |
| 7 | "PostgreSQL 18" | **PostgreSQL 16** (`docker-compose.yml`). | `docker-compose.yml` |
| 8 | "Python 3.11+" | **3.12+**, FastAPI + LangGraph, iMessage/Sendblue. | `services/converse/pyproject.toml` |
| 9 | Vote integrity table listed device fingerprint / network clustering as **vote-time checks** | They are **recorded metadata only**; neither can refuse a vote. They feed the post-close sweep. Split the page into "checked at vote time (refuses)" vs "recorded now, analysed later". | `cast_vote.rs` (no check on `cast_ip`/`device_hash`); `domain/integrity.rs` |
| 10 | "the public sequence can be shown without leaking who voted which way" | The vote number is **withheld during the hidden-tally window** and only returned when `now < tally_hidden_at` or the market is Resolved/Paid/Voided. | `cast_vote.rs:156-161` |
| 11 | `outbox_events` table | Actual name is **`events_outbox`**. | `migrations/0001_init.sql:329` |
| 12 | Migration table: "0005 added reputation, anti-fraud signals" | `reputation` is in **0001**. 0005 added `realizations`, `integrity_reports`, `votes.cast_ip`/`device_hash`, `markets.lp_pnl_micro`. | `migrations/0005_economy_integrity.sql` |
| 13 | "Phase 7's plan went through **five** review rounds" | **Nine** — 5 codex + 4 grok, ~70 findings, double-approved at revision 3.3. | `PLAN.md`, `docs/reviews/` |
| 14 | Crate sizes: adapters ~28k, application ~68k | adapters **40,804**; application **61,258**; domain 3,655 ✔; simswarm 5,876 ✔; main 1,100 ✔. | `wc -l` over `crates/*/**.rs` |
| 15 | Coverage "on the domain, application, and adapter layers" | Four packages — `simswarm` lib is gated too. | `justfile` `coverage` |
| 16 | `just ci` implied to include `e2e_swarm_smoke.sh` | `ci: fmt clippy test coverage deny audit gate-test deps-check`. The e2e and `mutants` are separate. | `justfile` |
| 17 | "`DATABASE_URL` — where PostgreSQL is" (implied required) | It **defaults** to `postgres://opinions:opinions@localhost:15434/opinions`, and the server **runs pending migrations itself at startup**. The manual migration loop was presented as mandatory. | `main.rs:565-570` |
| 18 | README's "Redis + NATS" repeated implicitly | Both are in `docker-compose.yml`; **no crate depends on either**. Everything is PostgreSQL. Stated explicitly on two pages so a reader is not left hunting for a queue that does not exist. | `grep -i redis\|nats crates/*/Cargo.toml` → nothing |

### Imprecise / overstated

| # | Was | Now |
|---|---|---|
| 19 | "multiply the two piles together, and that number must never change" | Must never **decrease**; rounding dust stays in the pool. Shown with the real product before/after (1,000,000.0000000 → 1,000,000.0007950). |
| 20 | "the market currently prices yes at 22–25c, about 21.7 shares, fee 5c" (invented) | Real quote for $5 into a 770/230 pool at 100 bp: **21.1726 shares, 23.62¢ avg, $0.05 fee**, payout at 41% = **$8.68**. Computed from `amm.rs` and used consistently on six pages. |
| 21 | "A few hours before closing, the tally goes dark" | Built-in defaults: **1 hour** (daily) / **5 minutes** (flash); live-immutable per market. |
| 22 | "Two endings exist: Paid / Voided" | Plus *held for review* and *curator required*; auto-void needs **both** too-few-votes **and** open interest under the floor. |
| 23 | Market lifecycle diagram (5 boxes) | Real machine: 9 states, 17 legal edges, incl. `Draft`/`Scheduled` and admin-void from seven states. |
| 24 | "the leftover fraction of a cent goes to fees" (settlement only) | True at settlement; on a *trade* the crumb stays in the **pool**. Both now stated separately. |
| 25 | "verification level: none, basic, or full" | Tiers **0/1/2**; expired horizon ⇒ Indeterminate; revocation to 0 ⇒ **Hit**, not absence. |
| 26 | "the fifteen legal combinations" (mentioned once) | Now a full page reproducing the W1–W15 table, the CAS edges, and the four crash windows, from `docs/copy/ops.md` + `0011`. |
| 27 | Fee described only as "a small fee, ~1%" | Full mechanics: ceil-rounded split, per-market stamp, 10–200 bp override, tier discount, `min_fee_bps` floor, anti-flip window — with the resolution order. |

### Claims deleted as unverifiable

- **"Sam gets a small signup bonus"** — no automatic signup grant exists. `GrantCredit` is an
  admin (dual-controlled) or referral path. Replaced with an accurate description of credits.
- **"Sam adds money with a card"** — the onramp is an open, unbuilt item. Now labelled as such.
- **"an army of fake accounts voting together" as the sweep's stated criterion** — replaced
  with the four actual ratios, the two-signal rule, the weak/medium labels and the skip rules.

---

## 2. Pages added (7)

| Page | Why |
|---|---|
| `start-here/scoring-and-reputation.md` | Required gap. The 75/25 formula, worked examples, the EWMA + half-lives, tiers, and the honest note that a bare server applies no discount or cap. |
| `money/fees-and-pricing.md` | The task's "exact fee/pricing mechanics". Two fully worked trades, the fee-resolution order, the sell quadratic, the caps, settlement arithmetic. |
| `money/withdrawal-states.md` | The blessed W1–W15 table, the CAS transitions, the four crash windows, the 2-of-3 non-landing predicate, warmth and daily limits with real numbers. |
| `build/migrations.md` | Required gap. What each of the 11 migrations added, in order, with line counts. |
| `build/live-updates.md` | Required gap. Outbox → relay → WebSocket end to end, at-least-once and why, every frame type, reconnect, hidden-tally-at-source, the notifier as second consumer. |
| `build/control-plane.md` | The four catalog attributes, apply classes, the published seed values, fence/generation propagation, pauses, dual control. |
| `running/the-swarm.md` | Required gap. Profiles, 7 personas, 6 attack rings, determinism, 10 chaos scenarios, mid-run invariant checks, the SLO gate. |

## 3. Verification performed

- `mkdocs build --strict` → **exit 0, zero warnings**, with `validation: {omitted_files, absolute_links, unrecognized_links, anchors} = warn` added to `mkdocs.yml` (so `--strict` fails on any broken internal link or heading anchor).
- **92 path-like tokens** cited in the docs checked to exist on disk — all resolve.
- Every `` `table_name` `` cited cross-checked against `create table` in `migrations/*.sql` — all 64 exist.
- Every `UPPER_CASE` token (env vars, constants) cross-checked against `crates/`, `services/`, `scripts/` — all present.
- AMM and settlement numbers recomputed in Python from the algorithms in `amm.rs` / `resolution.rs`.
- `scripts/check_frozen_manifest.py` → exit 0.

## 4. Also updated

`README.md` and `PLAN.md` carried stale "19 pages / ~14k words" counts for Phase 8. Both
corrected. Neither is in the frozen manifest.
