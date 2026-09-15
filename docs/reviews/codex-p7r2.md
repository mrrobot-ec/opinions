# Phase 7 plan — round 2 adversarial review

## Verdict: FIX-FIRST

Revision 2 materially improves the design, but it does not yet justify the resolution log's claim that every round-1 finding is closed. Of my 19 round-1 findings, **9 are resolved, 8 are partial, and 2 are unresolved**. The new credit, suspense, send-attempt, and lock-graph surfaces also introduce **6 new blockers, 9 new majors, and 3 new minors**.

The highest-risk regressions are concrete rather than editorial: a grant can convert on fees that a later D30 unwind refunds; the “bonus reserve” is only the same singleton House cash account used by other debits; successful idempotency replays are placed after fallible remote calls; the daily limit stops counting a withdrawal once it settles; the two new account classes still violate migration 0003's owner-shape constraint unless 0011 replaces it; and the new unwind requirement closes a `user → market` / `market → user` deadlock cycle in the existing code.

## Round-1 finding verification

| R1 finding | Status | Revision-2 verification |
|---|---|---|
| **B1 — cross-currency credits / credit-funded collateral** | **RESOLVED** | D32 now makes credits non-tradeable, retires `User(UsdcCredit) → House(UsdcCredit)`, and separately pays `House(Usdc) → User(Usdc)` in the same DB transaction (`phase7-money-compliance.md:23–26`). Each ledger transaction is balanced in one currency, and External USDC is untouched. The newly introduced fee-finality and reserve defects are NEW-B1/B2 below, not the original dimensional defect. |
| **B2 — memo + post-send CAS is not exactly-once** | **PARTIAL** | D31 adds leased attempts and persists immutable signed bytes/signature before broadcast, with same-byte rebroadcast and `unknown` retaining Withheld (`:14,18`). That closes the original crash-after-broadcast replay. It still says replacement requires “proof of non-landing” without defining the proof, provider/quorum, finalized block-height condition, or attempt lineage; see NEW-M2. |
| **B3 — read-only lien / pending-withdrawal and new-cash races** | **PARTIAL** | Request and send now enforce the lien transactionally, and cash-producing paths are required to call the shared collector (`:15,18–19`). However, the current unwind locks the market before it even discovers participant users (`crates/application/src/ops/unwind_market.rs:114–124,180–206`), while PlaceTrade and revised withdrawal take user before money/market. Revision 2 gives no safe multi-user unwind lock algorithm or owner for that edit; see NEW-B6. |
| **B4 — compliant rejection cannot ignore an arrived deposit** | **RESOLVED** | D32 correctly separates observation from availability: every finalized inflow books `External → DepositSuspense`, and only an admitted observation moves to User (`:23–24`). Direct blocked/no-KYC deposits therefore remain ledger-visible. The new observation/admission/refund authority is incomplete separately; see NEW-M1. |
| **B5 — withdrawal row cannot prove one hold and one terminal disposition** | **PARTIAL** | D31 adds hold/release/settle ids, approval/send authorities, state checks, atomic transitions, and the requested (a)–(e) invariants (`:14,20`). But it merely promises a “published transition table” without publishing one, lists no columns for the new review/send dimensions, and leaves the legacy `txn_id`/`chain_sig` authority unexplained; see NEW-M3. |
| **B6 — ownership matrix is not path-level/exhaustive** | **UNRESOLVED** | Section 2 is more specific and gives PlaceTrade/Deposit to W3, but it still uses buckets such as “fakes, Pg impls,” “suspense watcher + admission,” and “wiring,” not one owner per concrete file (`:49–56`). Required existing files for unwind, payout, remedial collection, preview, HTTP DTOs, converse, OTP, owner mappings, and invariant reads remain unassigned; see NEW-M4. |
| **M1 — executable hold-first sequence** | **PARTIAL** | The local lock/check/hold sequence and limit semantics are substantially pinned (`:15`). However, replay lookup occurs only after geo/sanctions/KYC remote work, and the fingerprint includes a server policy version; a successful retry can fail before reaching its hit or conflict after a policy bump. Remote refusals also have no durable replay receipt. See NEW-B3. |
| **M2 — webhook and settlement at-least-once seams** | **RESOLVED** | D32 specifies authenticated durable inbox, payload hash, same-key replay/conflict behavior, and identity binding; D31 locks and verifies receipt then commits settlement leg, state, event, and notification together (`:18,24`). |
| **M3 — fail-closed KYC/geo/sanctions algebra** | **RESOLVED** | D33 now has an explicit US-state allowset, trusted-proxy rule, `Clear | Hit | Indeterminate`, freshness/TTL, KYC validity and revocation, and named fail-closed entry points (`:28–31`). |
| **M4 — dual-control perimeter** | **PARTIAL** | Flag clearance and fee override are now two-person, and low-value single-finance withdrawal approval is a named exception (`:17,34,43–44`). The resolution claims an exhaustive command matrix, but the plan contains no such matrix: ban is only “dual-control,” unban is not described, manual grant still points to D30 by analogy, and role splits/TTL/replay rows are absent for several commands. |
| **M5 — lifetime wager counter reuses old volume** | **RESOLVED** | D32 replaces it with immutable lots, grant-time policy, oldest-first per-lot fee allocation, and one contribution per trade id (`:25`). The missing allocation authority and reversal-after-conversion behavior are new findings, not the lifetime-counter bug. |
| **M6 — normal in-flight withdrawals look like wallet drift** | **PARTIAL** | D35 explicitly adds in-flight outbound/inbound terms and tests every state (`:40`). The displayed equation still says “ledger External ±” rather than defining the sign, opening baseline, and common as-of cut; NEW-m1 records the remaining executable gap. |
| **M7 — new config keys lack a D24 catalog** | **UNRESOLVED** | The heading says “Every new key ships with the full D24 attributes,” but the following sentence is not that table (`:46–47`). Most keys still have no exact JSON type, numeric bounds, seed, apply class, write role, or max delta; e.g. `deposit_confirmations` is merely `>0`, AML/shadow/SLA knobs are unnamed groups, and allowset contents are not modeled. The existing validator rejects unknown keys (`crates/application/src/ops/config.rs:644–687`), so workers still lack a buildable catalog specification. |
| **M8 — finality/mint/destination/fee semantics** | **PARTIAL** | Finalized receipt verification, canonical destination, signature/mint/source/delta checks, and unknown/definitive-failure split are now explicit (`:18,24`). The promised cluster/mint/decimals are not actually pinned in the plan, and “network cost from the requested amount” has no balanced ledger legs or USDC/SOL conversion rule; see NEW-M6. |
| **M9 — full 2,000-agent release SLO gate** | **RESOLVED** | D35 and exit criterion 9 require both gated smoke and a pinned 2,000-agent profile, with nonempty series and constant targets (`:38–41,68`). NEW-M5 catches a new timer-origin error. |
| **M10 — self-exclusion and deposit limits omitted** | **RESOLVED** | D34 adds user self-exclusion, cooling-off, dual-control lift, deposit limits/no-instant-raise, and return-to-source exceptions (`:33–36`). |
| **m1 — identity numbering/classes** | **RESOLVED** | D31 explicitly extends identity 3 and does not replace D27 (`:16`); Withheld and DepositSuspense are non-external classes. |
| **m2 — alert dedup has no incident lifecycle** | **RESOLVED** | D35 specifies a durable outbox, episode key, open-incident dedup, delivery retry, ack/resolve, and recurrence (`:41`). |
| **m3 — `requested`/`held` conflates money and review state** | **PARTIAL** | D31 names three dimensions and their vocabularies (`:14`), but supplies neither persisted dimension columns nor the promised mapping table, and later text still uses removed names (`requested`, `held`). See NEW-M3 and NEW-m2. |

## NEW findings

### NEW-B1 — Fees can qualify a cash conversion before D30 makes those fees irreversible

**Evidence.** D32 converts a lot as soon as cash trade fees since `granted_at` reach the lot amount, then says an unwind merely “reverse[s] their attributed fee progress via compensating fact” (`docs/plans/phase7-money-compliance.md:25`). Phase 6 D30 explicitly reverses trade fees to users and permits true unwind from `Voided` (`docs/plans/phase6-simswarm-ops.md:59–61`). The current unwind enumerates only ledger transactions touching the market's pool/escrow (`crates/application/src/ops/unwind_market.rs:180–197`), so a separate House→User credit-conversion transaction is not part of the market reversal. Exit criterion 4 says a “wash-pair-after-grant does NOT convert,” while criterion 5 says conversion fires exactly when fees reach the lot (`phase7-money-compliance.md:63–64`).

**Failure.** A user receives a $10 lot, pays $10 in fees on a still-unwindable market, receives $10 cash, and withdraws it. If the market is later voided and truly unwound, the fees are returned; decreasing `consumed_fee_micro` after `converted_at` does not claw back the $10 cash. The user has recovered the fees and retained the conversion. The two exit criteria are also mutually satisfiable only if the wash pair is silently kept below the threshold.

**Resolution.** Treat trade fees as provisional until their market reaches a terminal state from which D30 forbids unwind—normally `Paid`. Allocate provisional fee facts by trade, finalize them at Paid, and convert only from finalized fee allocations. If product insists on pre-Paid conversion, the unwind transaction must atomically reverse conversion cash or open a linked receivable and cancel/collect any unsent withdrawal; specify that authority and invariant. State the exact fee amount in the BonusWash attack so “does not convert” proves a rule rather than insufficient test volume.

### NEW-B2 — The “bonus reserve” is not a segregated reserve and can be spent by unrelated House paths

**Evidence.** D32 calls the source a “pre-funded bonus reserve” but the actual cash leg is `House(Usdc) → User(Usdc)` and conversion 422s if “the reserve” cannot cover (`:25`). The real account identity permits only one singleton House account per currency (`migrations/0003_accounts_identity.sql:1–12`; `crates/application/src/model.rs:727–734`). Existing market seeding, receivable collection/unwind, and remedial credit all use that same `OwnerRef::House` (`crates/application/src/seed_market.rs:109–114`; `ops/receivable_collection.rs:38–62`; `ops/remedial_credit.rs:164–187`). Revision 2 assigns none of those debit paths a reserve guard.

**Failure.** A grant can be fully backed at issuance, then an unrelated seed/remedial/unwind debit can reduce House below outstanding grant value. The continuous invariant will detect insolvency only after it is created, and a user who already paid the qualifying fees receives a 422 instead of promised cash. The plan's “reserve-coverage invariant” and “conversion 422” clauses are internally at odds: if the invariant is transactionally preserved, earned conversion cannot encounter a shortfall.

**Resolution.** Add a dedicated non-negative `BonusReserve` account class and fund it with an explicit `House → BonusReserve` cash transaction; only conversion may debit it. Alternatively create a durable encumbrance subledger and require **every** House debit to share one reserve lock and prove post-balance ≥ outstanding grant liability. Reject grants/funding withdrawals before liability becomes unbacked; a qualified conversion shortfall must be an invariant failure, not an expected 422. Add concurrent seed/remedial/unwind/conversion Pg tests and assign every affected file.

### NEW-B3 — Remote-first request processing defeats idempotent replay and policy-version fingerprinting makes replays conflict

**Evidence.** D31 orders geo, sanctions, and KYC before `begin → serialize_key → hit lookup`; the hit fingerprint includes `policy_version` (`phase7-money-compliance.md:15`). Only after those calls can the service return the original receipt.

**Failure.** A request succeeds and holds money. Its exact retry arrives while the sanctions provider is unavailable: it returns the new remote error instead of the committed original receipt. If the provider is available but the allowlist/sanctions policy version advanced, the same user/amount/destination computes a new server-side fingerprint and returns 409. This reverses Phase 6's pinned replay precedence, where a hit returns before pause/config checks, and makes at-least-once callers depend on vendor uptime.

**Resolution.** Fingerprint only immutable client intent `(user, amount, canonical destination, client idempotency key namespace)`; store decision/policy versions as separate authorization facts. Perform a fast persisted hit+fingerprint lookup before remote calls, then on a miss call vendors and begin the guarded transaction, rechecking the key under `serialize_key` to close the race. Alternatively create a short-transaction `screening` command row first and let remote screening advance it asynchronously. Persist remote-refusal results if the contract promises replay-stable refusals. Add retry-after-provider-outage and retry-after-policy-bump tests.

### NEW-B4 — The rolling daily limit explicitly stops counting settled withdrawals

**Evidence.** D31 says the rolling-24-hour limit is “reserved by requested+held+sent amounts” (`:15`). Revision 2's actual coarse states are `queued|risk_hold|sent|settled|denied|failed` (`:14`), and `settled` is omitted from the limit formula.

**Failure.** A user requests the per-day maximum, waits for rapid settlement, then immediately requests it again. Because the first row is no longer requested/held/sent, it no longer reserves the window, so repeated fast settlements bypass the compliance/risk limit. The formula also names two states that the revised machine no longer stores.

**Resolution.** Define one authoritative reservation query/fact. For a withdrawal with `requested_at` in the rolling 24-hour window, count all live states **and settled**, releasing the reservation only for denied or definitive-failed/non-landed requests (or count failed attempts too if AML policy requires). Apply the same semantics to per-destination and hot-wallet caps under the user/cap lock. Add sequential settle-and-retry plus concurrent boundary tests.

### NEW-B5 — ALTERing only the ledger account vocabulary does not make Withheld or DepositSuspense representable

**Evidence.** Revision 2 says 0011 alters the ledger account CHECK and adds one Withheld per currency plus DepositSuspense (`:16,24,51`). Migration 0003 has a second constraint allowing non-null owners only for user/pool/escrow and null owners only for fees/house/external; its singleton unique index covers only fees/house (`migrations/0003_accounts_identity.sql:1–12`). Current Pg and fake owner matches are also exhaustive over the old variants (`crates/adapters/src/pg/trade_tx.rs:190–241,594–602`; `crates/application/src/fakes/state.rs:260–268`), while the invariant reader maps every unknown owner string to `External` (`crates/adapters/src/pg/invariant_read_tx.rs:36–44`).

**Failure.** Merely extending `ledger_accounts.owner_type`'s original CHECK still makes inserts fail `ledger_accounts_owner_shape`; without a new unique predicate, concurrent workers can create multiple singleton Withheld/Suspense accounts. If adapter mapping is missed, the invariant can classify the new internal liability as External and report a false result.

**Resolution.** Make 0011 explicitly drop/recreate `ledger_accounts_owner_shape`, replace or supplement the singleton unique index for `withheld` and `deposit_suspense`, preserve immutable account identity, and specify owner_id nullability. Enumerate every domain/Pg/fake/invariant mapping file in 7.0a ownership. Add migration tests for one-per-currency identity, concurrent get-or-create, non-negativity, and total owner-string decoding; never use a wildcard-to-External decoder.

### NEW-B6 — The required unwind cancellation creates a deadlock cycle with the pinned user-first lock order

**Evidence.** D31 pins request/PlaceTrade as `lock_user` before any money authority and requires unwind to cancel unsent withdrawals and collect in the same unwind transaction (`:15,19`). The current unwind begins by locking the market and unwind row, then enumerates ledger participants (`crates/application/src/ops/unwind_market.rs:114–126,180–206`). `UnwindTx` does not include `UserLockGuard` (`crates/application/src/ports/ops.rs:347–377`). PlaceTrade already locks user before market (`crates/application/src/place_trade.rs:98–105`).

**Failure.** A literal revision makes unwind hold the market then wait for user U, while U's concurrent trade/withdraw holds the user lock then waits for the market: a closed deadlock cycle. Unwind cannot simply lock users first because it does not know the full participant set until it reads the market's ledger history, and it may need to cancel withdrawals for many users.

**Resolution.** Publish a multi-user algorithm. One viable form is lock-free participant enumeration → sorted user-lock acquisition → market/unwind row lock → re-enumeration; if a newly committed participant was not locked, roll back and retry from enumeration. Then cancel/collect withdrawals and compute the reversal under that stable lock set. Add `UserLockGuard` to `UnwindTx`, assign `ops/unwind_market.rs` and both Pg/fake implementations, and prove with Pg tests against PlaceTrade, withdrawal request/send claim, deposit admission, and payout. Do not let a worker invent a market→user exception.

### NEW-M1 — DepositSuspense has no per-observation admission/refund state machine or exact suspense invariant

**Evidence.** D32 names `deposit_observations` and the observation/admission/refund movements, but no observation states, transaction-id columns, lock/CAS protocol, or invariant tying the pooled suspense balance to pending facts (`:23–24,51`). The existing `deposits` table already owns unique `chain_sig`, `status seen|confirmed|credited`, and `txn_id` (`migrations/0001_init.sql:299–307`); revision 2 creates a second observation table without saying whether deposits is ALTERed, superseded, or linked. Refund is said to use “the D31 send protocol,” whose authority and invariant are withdrawal-specific.

**Risk.** Two admission workers can move the same observation from singleton suspense twice unless a row state and ledger key linearize it. An orphan/misattributed suspense amount can pass identity 3. A refund cannot lawfully reuse `withdrawal_send_attempts` if no withdrawal/Withheld hold exists, while adding it as a withdrawal falsifies D31's row/signature invariant. Returning to an exchange omnibus source can also send money somewhere the user cannot recover it.

**Resolution.** Choose one canonical deposit authority—prefer ALTERing/linking the existing `deposits` row—and publish states such as `observed_finalized|admission_pending|admitted|compliance_hold|refund_approved|refund_sending|refunded`, with unique observation/admission/refund tx ids and fingerprint. Row-lock and CAS observation plus `DepositSuspense → User`/collection in one transaction. Add `balance(DepositSuspense,usdc) = Σ pending observation liabilities` and exact admission XOR refund legs. Generalize the rail layer to an `outbound_payment` authority with typed subject (`withdrawal|deposit_refund`) or give refunds their own attempts/invariants; define a screened, user-controlled refund destination policy rather than blindly echoing an exchange source.

### NEW-M2 — “Proof of non-landing” and replacement-attempt lineage are still undefined

**Evidence.** D31 permits a new signed transaction after blockhash expiry only “with proof of non-landing” (`:18`) and models attempts as `unknown|finalized|definitive_failed` (`:14`). It also states payment facts/**signatures** pair 1:1 with withdrawals (`:20`), even though a legitimate expiry replacement yields multiple signatures for one withdrawal.

**Risk.** A single lagging or partitioned RPC saying “not found” is not proof. Creating a replacement while the old fork can still land restores the double-pay risk. Conversely, treating all attempt signatures as 1:1 makes any safe replacement violate the invariant.

**Resolution.** Pin `last_valid_block_height` in each attempt and require an observed **finalized** block height beyond it plus signature absence from a specified quorum/archive source before marking definitive-failed. Link replacements with monotone attempt number and `replaces_attempt_id`, allow at most one prepared/broadcast/unknown attempt, and retain every immutable attempt. State the invariant as attempts 1:N, exactly one-or-zero finalized attempt, and exactly one finalized payment fact/external settlement per withdrawal. Add lagging-RPC, fork, late-observation, and two-replacers races.

### NEW-M3 — The ALTERed withdrawal machine does not persist its advertised three dimensions or retire legacy authority

**Evidence.** D31 extends the one `status` CHECK and lists new tx/fingerprint/timestamp columns, then separately names review and send vocabularies without listing `review_state` or `send_state` columns or a mapping (`:14`). Existing rows also retain `chain_sig` and generic `txn_id` (`migrations/0001_init.sql:309–317`), while new receipt and hold/release/settle columns become competing authorities. No backfill/quarantine rule is stated before “hold_tx_id NOT NULL after queue.”

**Risk.** W1 can encode dimensions in the coarse status, events, or satellite rows in mutually incompatible ways. Queries, constraints, limit accounting, stuck detection, and the Withheld sum will disagree. Existing staging rows can fail migration or preserve an unlinked `txn_id`; `chain_sig` can conflict with the selected finalized attempt.

**Resolution.** Publish the actual schema and full transition table. Either add explicit `review_state` and `send_state` CHECKed columns or define a deterministic derived mapping and forbid impossible combinations with constraints. Rename legacy `txn_id` to its one meaning or migrate/drop it; make legacy `chain_sig` a generated/reference of the finalized attempt or remove it. Specify backfill for each old status and how pre-0011 rows without holds are quarantined. Include every CAS source/target and terminal constraint.

### NEW-M4 — Newly required integration files remain unowned despite the “exhaustive” matrix

**Evidence.** Revision 2 requires unwind cancellation/fee compensation, payout/sell/remedial auto-collection, fee override in preview and execute, required config versions in every client, converse-region propagation, OTP referral binds, and new invariant owner mappings. Yet §2 assigns no owner to `crates/application/src/ops/unwind_market.rs`, `resolve_market.rs`, `ops/remedial_credit.rs`, `preview_trade.rs`, `crates/adapters/src/http/{dto, routes}/market.rs`, `services/converse/src/converse/{graph.py,core_client.py}`, their tests/generated model, `crates/adapters/src/pg/{rows.rs,invariant_read_tx.rs}`, or a phone-verification surface. W4 “wiring” also needs the 7.0a-frozen `crates/main/src/main.rs`, and 7.0a “fakes + PgStore” overlaps W3's trade adapters (`:49–56`).

**Risk.** The build either omits mandatory behavior or stops mid-wave on exactly the conflicts the matrix claims to prevent. The repository has no VCS safety net.

**Resolution.** Expand §2 to one row per exact path, including tests and generated artifacts, and state pre-wave→wave ownership transfers for files that must first compile new enum variants. Give unwind/resolve/remedial changes an owner; give preview/HTTP/converse coherence one owner; add OTP and fee-allocation paths; name all invariant/adaptor mappings; and make W4 provide concrete adapters behind predeclared composition traits without editing frozen main, or reserve a coordinator integration barrier.

### NEW-M5 — The SLO timer is described as `tally_hidden_at → Paid`, which makes the one-second gate impossible

**Evidence.** D35 calls the series `close-to-paid` but parenthetically defines it as “hidden→Paid” (`:39`). Phase 6 allows hidden windows from 60 to 900 seconds (`docs/plans/phase6-simswarm-ops.md:21`), while the target is p99 <1 second.

**Risk.** If “hidden” means `tally_hidden_at`, every legitimate market includes at least a minute before close and must fail. If it means something else, independent workers will instrument different origins and produce incomparable reports.

**Resolution.** Pin the event pair exactly: normally authoritative `closes_at`/transition to Closed (or resolution-start event, if following spec wording) to the committed Paid event, with only the named risk-hold interval subtracted. Keep `tally_hidden_at` out of this timer. Add a deterministic unit trace with known timestamps that asserts the computed latency.

### NEW-M6 — “Network cost from the requested amount” is dimensionally and ledger-wise incomplete

**Evidence.** D31 holds the requested USDC amount, says the user pays network cost from it, verifies the exact delivered delta, then describes settlement only as `Withheld → External` (`:18`). It sets minimum to “dust+gas,” but Solana gas is paid in SOL while withdrawal amount and conservation are USDC. D31's invariant calls the terminal leg exact (`:20`).

**Risk.** For requested `A`, delivered `B`, and fee `F=A−B`, `Withheld(-A)+External(+B)` is unbalanced unless a Fees/House leg receives `F`. If the actual network charge is SOL, no stable `F` in micro-USDC exists without an oracle/quote policy. The destination confirm does not state net delivery, so the user may approve economics different from execution.

**Resolution.** Choose one policy: platform absorbs SOL gas and delivers/settles full USDC `A`; or quote a fixed/bounded USDC service fee before hold, include `(gross, fee, net)` in the confirm echo/fingerprint, and settle `Withheld(-A) + External(+net) + Fees/House(+fee)`. Account for SOL treasury expense in its own non-USDC operational reconciliation. Update the terminal invariant and exact-delta test to the chosen multi-leg semantics.

### NEW-M7 — The schema cannot enforce unique fee allocation or reverse it from aggregate lot fields

**Evidence.** D32 promises oldest-first allocation, each trade id contributing once, and unwind compensating facts, but its lot shape has only aggregate `consumed_fee_micro` and `converted_at` (`:25`). The 7.0a migration list includes `credit_grant_lots` but no fee-allocation/reversal table (`:51`). The existing trade row already supplies a stable id and `fee_micro` (`migrations/0001_init.sql:211–228`).

**Risk.** An aggregate counter cannot prove which trades contributed, prevent a replay from counting the same trade twice, split one fee across lots without loss/duplication, or reverse only the allocation from one unwound market. In-place decrements also destroy audit history. Two conversion workers can race the threshold unless both ledger transactions and lot terminal state share one key/lock protocol.

**Resolution.** Add append-only `credit_fee_allocations(trade_id, lot_id, amount_micro, kind=allocated|reversed, source_allocation_id, idempotency_key)` with constraints that net allocation per trade never exceeds its actual fee and net lot allocation never exceeds its requirement. Define deterministic splitting across lots. Convert under a lot row lock with two namespaced ledger keys and the `converted_at` write in one DB transaction; same-key replay returns both original tx ids. Extend unwind and invariants through allocation lineage.

### NEW-M8 — Referral grants depend on an OTP identity system that the phase does not specify or own

**Evidence.** D32 brings D21 phone OTP “in scope” for every grant and binds uniqueness to verified phone (`:26`). The migration/port/ownership lists contain referral codes/binds and KYC ports, but no phone challenge/verification table, OTP delivery port, rate limits, route, fake, or owner (`:51–55`). Existing `user_channels` records only `(channel,address)` uniqueness and has no verification fact (`migrations/0003_accounts_identity.sql:26–32`).

**Risk.** W3 cannot prove “verified phone” or one grant per phone. Treating an iMessage address or KYC tier as implicit OTP silently weakens a launch-blocking D21 identity rule, while adding an ad hoc provider crosses W2/W3 and frozen composition ownership.

**Resolution.** State whether the KYC provider supplies an attested phone or build an explicit `PhoneVerification` port and sandbox adapter. Persist normalized-number HMAC, challenge id, expiry, attempts/rate limit, verified_at, provider reference, and uniqueness policy without raw-phone leakage. Assign migration, application, adapter, route, composition, and tests; make grant issuance require the durable verification fact and test concurrent accounts on one phone.

### NEW-M9 — `bonus_structure` changes have no rule for already-issued lots

**Evidence.** Lots retain `policy_version`, while `bonus_structure=real_money|sweeps` determines whether cash conversion “exists at all” (`:25`). D24 says held withdrawals use current policy but says nothing about credit lots (`:47`).

**Risk.** A switch to `sweeps` can either retroactively revoke cash conversion after users incurred fees, or allow old real-money conversions after counsel has ordered them off. Stamping policy_version without defining which version authorizes conversion leaves both legal and ledger liability ambiguous.

**Resolution.** Define transition semantics before issuing any lot: stamp an immutable grant class and its redemption promise, specify whether an emergency structure change grandfather-converts, refunds, freezes, or replaces existing lots, and reserve cash accordingly. Make the config proposal preview report affected outstanding liability and refuse an impossible transition. Add real_money→sweeps and sweeps→real_money tests with open, qualified, and converted lots.

### NEW-m1 — The reconciliation “equation” still has no signs or common snapshot boundary

**Evidence.** D35 writes “expected wallet = ledger External ± ...” (`:40`). In this ledger, deposits make External negative and withdrawals make it positive; the invariant is `−balance(External)=Σ internal` (`docs/plans/phase6-simswarm-ops.md:40–42`).

**Resolution.** Write the exact signed formula from an attested opening wallet balance and define one as-of cut across DB and chain observations. Name whether broadcast-unfinalized is subtracted from `−External`, and how an inbound can be “observed-unbooked” when D32 promises every finalized observation is booked. Test a numeric example for each state.

### NEW-m2 — Removed state names survive in decisions and alerts

**Evidence.** The new dimensions use `screening|review_required|approval_proposed|approved` and `unsent|...`, but D31 limits mention `requested+held+sent` and D35 pages stuck “`screening`/`requested`” rows (`:14–15,41`). The coarse DB states are `queued|risk_hold|sent|settled|denied|failed`.

**Resolution.** Use exactly one vocabulary in config keys, queries, event payloads, tests, and ops copy. The transition table should state which persisted dimension each SLA/cap query reads.

### NEW-m3 — Machine deposit admission is called an admin audit without an actor contract

**Evidence.** D32 calls `DepositSuspense → User` an “audited tx” (`:24`). Phase 6 D26 and the current audit port state machine actors do not write `admin_actions`; admin audit rows require an authenticated token (`docs/plans/phase6-simswarm-ops.md:36–38`; `crates/application/src/ops/audit.rs:84–106`).

**Resolution.** Distinguish an immutable compliance-decision/domain event written for every machine admission from `admin_actions`, which is written only for manual overrides/refunds with an AdminContext. Specify both links where a human action causes admission.
