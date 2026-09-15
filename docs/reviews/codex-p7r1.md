# Phase 7 plan — round 1 adversarial review

## Verdict: FIX-FIRST

The three intended withdrawal ledger movements are algebraically sound if each is atomic: request moves `User(-A) + Withheld(+A)` and leaves internal collateral unchanged; deny/definitive pre-send failure reverses that pair; finalized settlement moves `Withheld(-A) + External(+A)`, while the settled-withdrawal fact increases by `A`, so both sides of the collateral equation fall by `A`. The plan does not yet make those “ifs” enforceable. In particular, it has no row-to-ledger reconciliation for the pooled Withheld account, no durable Solana send identity, and no atomic answer for an unwind that opens a receivable after funds have moved into Withheld.

I count **6 blockers, 10 majors, and 3 minors**. The build wave should not start until the blockers have explicit dispositions in the plan and ownership matrix.

## Blockers

### B1 — Bonus-credit conversion and credit-funded trades are not legal under the existing per-currency ledger

**Evidence.** D32 says “`credit_grant` books `External → UserCredits`,” “`credit_convert` books `UserCredits → User` cash,” and “Credits are spendable on trades” (`docs/plans/phase7-money-compliance.md`, D32). D13 locks two currencies, `usdc` and `usdc_credit` (`docs/decisions.md`, D13). The code is stricter still: `crates/domain/src/ledger.rs` says cash and credits “never mix inside one transaction leg set” (lines 23–29), validates zero-sum **per currency** (lines 147–187), and has a test named `cross_currency_two_leg_transaction_rejected` (lines 365–389). Its legal conversion example requires four legs, one balanced pair in each currency (lines 418–479). The current trade path also touches only the user's USDC account and a USDC escrow/fee account (`crates/application/src/place_trade.rs`, lines 220–247).

**Failure.** The literal two-leg conversion debits `usdc_credit` and credits `usdc`; it will fail the domain check and deferred DB trigger. A literal credit-funded trade has the same dimensional defect if it debits credit and funds a USDC escrow. Adding an `External(usdc)` issue leg makes the transaction compile, but invents a cash liability with no wallet inflow/payment fact, violating D27 identity 4 and D35 wallet reconciliation. It also leaves unanswered what collateral backs shares bought with credits.

**Resolution.** Specify the promotional reserve and all legs before dispatch. A viable shape is: burn/retire user credit against the `usdc_credit` contra account, and transfer already funded USDC from a dedicated non-negative bonus reserve/House account to either the user (conversion) or market escrow/fees (credit-funded buy), all in one four-or-more-leg transaction balanced separately per currency. Never debit `External(usdc)` without a paired on-chain funding fact. Define whether `UserCredits` means the existing `OwnerRef::User + Currency::UsdcCredit` account or a genuinely new owner class. Add a reserve-coverage invariant for outstanding convertible/spendable credits and tests for mixed cash/credit buys, sell proceeds, conversion, unwind, and insufficient promotional reserve.

### B2 — `sent_at` CAS plus a Solana memo cannot provide the claimed exactly-once chain send

**Evidence.** D31 claims “rails-side memo/dedup + our `sent_at` CAS” means “a crashed sender never double-pays,” and the chaos point kills “between rails send and `sent` CAS” (`docs/plans/phase7-money-compliance.md`, D31). The proposed `withdrawals` row has no signature, signed payload, attempt, lease, or receipt reference; it has only timestamps and hold/release transaction ids (D31's table declaration).

**Failure.** A CAS performed **after** an irreversible external effect cannot fence that effect. Two workers can both pass the pre-send state and broadcast before either CAS. More importantly, worker A can broadcast and die; worker B can build a fresh transfer. An SPL-token memo is metadata, not an on-chain uniqueness constraint, so a second valid signature can pay twice. An RPC timeout is also `unknown`, not `failed`; reversing the hold on that response can both restore user cash and leave the first transfer landed.

**Resolution.** Choose and document an actual external idempotency primitive. For a custody provider, require and contract-test a provider-enforced idempotency key. For direct Solana, add a durable `withdrawal_send_attempts` authority: claim/lease under a row lock, construct and persist the immutable signed transaction bytes and signature **before** broadcast, and only ever rebroadcast those exact bytes while their landing state is unknown. Specify blockhash-expiry recovery and the proof required before creating a replacement transaction. Persist receipt/signature/mint/destination/amount with uniqueness constraints; `unknown` keeps funds Withheld indefinitely and pages, while `failed` is allowed only after a definitive non-landing result. Test concurrent senders, crash before persistence, after persistence/before broadcast, after broadcast/before state update, RPC timeout, retry, blockhash expiry, and late landing. If the selected rail cannot meet this contract, weaken the product claim rather than calling memo lookup exactly-once.

### B3 — The receivable lien is a non-atomic read and can be bypassed by a pending withdrawal or mishandle newly converted cash

**Evidence.** D31 calls the Phase 6 `WithdrawalEligibility` guard only at request time. Phase 6 deliberately defines that role as read-only (`docs/plans/phase6-simswarm-ops.md`, D30). The real trait takes `&self`, outside any write transaction (`crates/application/src/ports/ops.rs`, lines 380–390), and the Pg implementation performs independent, non-locking cash and receivable queries (`crates/adapters/src/pg/unwind_tx.rs`, lines 804–845). Phase 6 auto-collects only “at deposit time.” D32 now says conversion creates cash atomically with a trade and that converted cash is withdrawable.

**Failure.** A user with no receivable can request a withdrawal, moving all cash into the singleton Withheld account. A later market unwind sees no user cash, books a House shortfall, and opens a receivable. The already-approved/requested withdrawal has no required lien recheck and can still broadcast, defeating the lien. The inverse race is also possible: eligibility reads clear, an unwind commits, then the stale request holds cash. Separately, credit conversion, payouts, sells, or remedial credits can create user cash while a receivable remains open; deposit-only auto-collection neither collects it nor makes the plan's “converted cash withdrawable” statement true.

**Resolution.** Make lien enforcement a write-transaction role, not the existing lock-free view. Define one canonical per-user lock shared by withdrawal request/approve-to-send, unwind shortfall creation, receivable collection, and every cash-crediting path, with an explicit lock order. Recheck immediately before the irreversible send claim. Decide one atomic policy for unsent withdrawals when a receivable appears: cancel and release-then-collect in the same transaction, or debit the attributable Withheld amount directly to House while appending the linked collection movement; return only the remainder to the user. Define sent-but-unsettled withdrawals as already unavailable to the lien and let unwind open a receivable for that shortfall. Invoke a common collection routine on credit conversion and other cash inflows, or explicitly net the debt during withdrawal. Add both race orders and conversion-with-open-receivable to fake/Pg concurrency tests.

### B4 — Fail-closed deposit gating has no accounting path for money that arrives anyway

**Evidence.** D33 says deposits require a configured KYC tier and geo admission; D32 says the watcher credits only after confirmation depth. D35 simultaneously requires ledger External versus hot-wallet reconciliation where “a single micro of drift pages.” The spec requires the same continuous exact reconciliation (`docs/spec.md`, §10.1). A chain transfer to an already issued custodial address is not preventable by an HTTP geo/KYC gate.

**Failure.** A non-KYC, banned, or blocked-region user can directly send finalized USDC to their deposit address. If the watcher refuses to book it, the wallet gains money without an External ledger leg and every correct reconciler pages forever. If it books `External → User`, the supposedly fail-closed gate has opened. An asynchronous chain watcher or onramp callback also has no trustworthy “request IP” on which to run the proposed geo middleware.

**Resolution.** Separate observation from availability. Every finalized wallet inflow must atomically create an immutable deposit observation/payment fact and book `External → DepositSuspense` (or an equivalently named non-negative internal liability account), even when admission fails. A current, versioned compliance decision then moves `DepositSuspense → User`; a refund moves Suspense to External only through the same hardened outbound-send protocol as withdrawals. Capture geo at an authenticated deposit-initiation step; direct/unattributed deposits default to compliance hold, not credit or omission. Include suspense and refunds in the invariant and wallet-reconciliation equations and test direct deposits for none/basic/full KYC, blocked/unknown geo, banned users, and provider outage.

### B5 — The withdrawal authority cannot prove that each row owns exactly one hold and exactly one terminal disposition

**Evidence.** D31's schema has `hold_tx_id` and one nullable `release_tx_id`, but no settlement transaction id, payment-fact id, chain receipt/signature, send attempt, decision proposal, or state-dependent checks. The account is “one per currency,” so its balance is pooled. D31 then relies on global conservation. Phase 6 D27 already distinguishes global balance conservation from “payment facts ↔ external legs 1:1,” and the implemented sweep has separate checks for per-transaction balance, external/internal balance, payment-fact pairing, and job idempotency (`crates/application/src/integrity/invariant_sweep.rs`, lines 50–125).

**Failure.** Global double entry can pass while withdrawal attribution is wrong: a row can be marked settled without its external leg, two rows can point at one hold, a denial can reverse the wrong amount, or an orphan Withheld balance can be offset elsewhere. The single `release_tx_id` cannot distinguish `Withheld → User` from `Withheld → External`. The listed table also cannot store or schema-enforce the promised two principals/delay for a large approval. Thus the arithmetic described in the verdict is sound, but the proposed authority cannot establish its premises.

**Resolution.** Add an explicit transition table and schema constraints. At minimum: immutable canonical request fingerprint; unique `hold_tx_id`; separate `reversal_tx_id` and `settlement_tx_id`; durable approval proposal/confirmation rows; send-attempt and receipt/signature linkage; and state checks that allow exactly one of reversal or settlement. In the same transaction, couple each status transition to its ledger transaction, audit/event, and outbox notification. Extend the one-snapshot invariant suite with: (a) `balance(Withheld, usdc) = Σ amount` for all and only active unsent/sent-unsettled withdrawal liabilities; (b) every row has one exact `User → Withheld` hold; (c) every terminal row has exactly one exact inverse **or** one exact `Withheld → External` leg, never both; (d) every outgoing chain payment fact/signature pairs 1:1 with a withdrawal and its external leg; and (e) no duplicate terminal effect. Make these fake+Pg and chaos assertions, not only one happy-path conservation assertion.

### B6 — The “one owner per file” matrix is neither file-level nor conflict-free

**Evidence.** The matrix claims “one owner per file” but gives conceptual areas (`docs/plans/phase7-money-compliance.md`, §2). The current code shows required shared surfaces. Adding Withheld changes `crates/domain/src/ledger.rs` (`OwnerType`), `crates/application/src/model.rs` (`OwnerRef`), Pg owner mappings, fakes, and invariant readers; none is named in 7.0a. A new transaction factory must touch `crates/application/src/ports/core.rs`, while aliases currently live in `ports/market.rs` and exports in `ports/mod.rs`; 7.0a names only a new `ports/money.rs`. More seriously, W2 needs trade enforcement, W3 needs cash/credit funding plus wager settlement, and W4 needs per-market fee consumption, all in `crates/application/src/place_trade.rs` and its transaction adapters. W1/W2 share withdrawal admission/routes, W2/W3 share deposit admission/`credit_deposit.rs`, and all four need the same main composition, OpenAPI, e2e script, and web surfaces.

**Failure.** With no VCS and synchronization by file existence, these are direct overwrite/conflict risks. Even if workers coordinate informally, several required files have no owner, while others have three. “Unavailable placeholders for every new route” also necessarily touches route registration/OpenAPI/composition files not named in the matrix.

**Resolution.** Replace §2 with an exhaustive path-level matrix like Phase 6 §2, including colocated tests and generated artifacts. Put all shared compiling skeleton changes in 7.0a: domain/model vocabulary, every port export and `Store` factory, error/model types, Pg/fake placeholders, route/OpenAPI registration, main composition slots, config catalog skeleton, dependency rules, scripts, and frozen manifest. Then assign each existing implementation file exactly once. Either give one wave sole ownership of `place_trade.rs`/trade Pg+fake adapters and have it integrate W2–W4 requirements, or sequence those waves with explicit ownership transfer and a barrier; do the same for deposit, withdrawal routes, `web/`, and the e2e script. State a frozen-file ask protocol and stop-the-wave rule as Phase 6 did.

## Majors

### M1 — “Hold-first” lacks an executable transaction/lock protocol around remote compliance calls and concurrent limits

**Evidence.** D31 says the row insert and `User → Withheld` ledger transaction are atomic, while the same request must call eligibility, KYC/config gates, geo, sanctions, daily limits, and balance checks. Some of those are remote ports. Existing write contracts require `serialize_key` as the first transaction call (`crates/application/src/ports/market.rs`, `IdempotencyGuard`, lines 13–20), but D31 specifies only a unique idempotency column, not replay precedence or a fingerprint.

**Risk.** Calling vendors while holding DB/account locks creates long transactions and indeterminate failures. Calling them first is reasonable, but then all mutable local facts must be revalidated under locks. Concurrent requests can each see room under the daily limit unless active holds reserve the limit atomically. Reusing one key with a different amount or destination is also undefined.

**Resolution.** Pin the sequence: authenticate and obtain signed/dated remote decisions without DB locks; begin; `serialize_key(namespace,user,key)` first; on a hit compare a canonical fingerprint `(user, amount, canonical destination, policy-relevant fields)` and return the original receipt or 409; on a miss lock the user plus daily-limit authority and relevant receivable/status rows; revalidate current KYC/user/config and freshness of geo/sanctions results; reserve the limit; apply hold; insert row/events; commit. Choose whether a remote error creates a safely held request or rejects without a hold, and make that result idempotent. Specify rolling/calendar window, timezone, and whether requested/held/sent amounts reserve the daily limit.

### M2 — Unique keys alone do not close the onramp-webhook and settlement at-least-once seams

**Evidence.** D32 offers `unique(provider, event_id)` for onramp dedup and chain signature for deposit dedup. D31 says settlement is at-least-once and “idempotent under replay,” without its transaction ordering. The current deposit implementation returns on any duplicate chain signature before checking that user and amount match (`crates/application/src/credit_deposit.rs`, lines 66–77), illustrating why uniqueness is not a complete replay contract.

**Risk.** If an adapter commits the webhook inbox row and then crashes before the effect, later duplicates can be discarded forever; effect-first can double-apply. A provider can reuse an event id with a different payload, and silently returning the first result hides corruption or tampering. Concurrent settlement pollers can both emit notifications or attach one receipt to different withdrawals unless the receipt, ledger leg, row state, and outbox event share one locked transaction.

**Resolution.** Define an authenticated durable inbox with provider/event key, canonical payload hash, signature/timestamp verification result, processing status/lease, and effect id. Same key/same hash returns the original result; same key/different hash is a typed conflict and page. Commit a directly coupled effect in the same DB transaction where possible; otherwise use a leased inbox command whose terminal effect is independently idempotent. For deposits, bind signature to user/address/mint/amount/canonical block. For settlement, lock the withdrawal, verify a uniquely stored receipt, perform the exact ledger leg, status, withdrawal event, and notification outbox in one commit, with the ledger key `withdraw-settle:<id>`.

### M3 — The fail-closed KYC/geo/sanctions assertions do not define safe defaults or an executable result algebra

**Evidence.** D33 explicitly fails closed when KYC-tier config is unset and on geo resolver error, which is good. It uses `blocked_regions`, however, while D2 locks “USA-only at launch” and the spec requires state-level geofencing per counsel. D31/D33 describe sanctions as “passes” or “hit,” but not timeout, unavailable, stale, ambiguous match, or provider revocation. `users.kyc_tier` alone has no pending/failed/expired/revoked status or validity horizon.

**Risk.** A denylist does not by itself express USA-only: an unlisted foreign country or unknown region passes unless every caller adds unstated logic. Missing/malformed policy snapshot behavior is unspecified. Spoofed forwarding headers can select a region unless trusted-proxy handling is pinned. A previously full KYC tier can remain usable after provider expiry/revocation, and a sanctions timeout can accidentally be treated as “not a hit.”

**Resolution.** Use an explicit allow policy: country must equal US and state/territory must be in a versioned counsel-approved allowset; unknown, missing policy, missing client IP, untrusted proxy chain, or resolver uncertainty denies money mutations. Define result enums such as `Clear{checked_at,expires_at,policy_version}`, `Hit`, and `Indeterminate`; only fresh `Clear` can progress, while Hit/Indeterminate remain held. Persist KYC case/status/tier/validity and permit downgrade/revocation events. Authenticate and dedup provider webhooks before state changes. Re-screen at the irreversible send boundary when the request-time result has aged past a specified TTL.

### M4 — The dual-control perimeter has several single-principal or analogy-only holes

**Evidence.** D31 makes below-threshold manual approval a single finance capability and specifies the large-withdrawal protocol only as “D26/D30 ... verbatim,” with no roles or authority row. D32 says manual grants use D30 merely “as the template.” D34 lets one finance principal clear an AML flag, after which the decision pass can auto-approve. D36 gives live fee repricing to a single `ops` capability even though Phase 6 D24 treats fee changes as finance + two-principal sensitive changes. Ban is called dual-control but its proposer/confirmer roles, delay, expiry, idempotency, and unban path are not stated.

**Risk.** One ops token can change the fee economics of a live book. One finance token can clear a flag and indirectly cause a queued payout. Depending on interpretation, one finance token can manually approve any withdrawal below the threshold. Builders will also make different choices for two-finance versus finance/superadmin confirmation and whether proposals expire. Audits observe these transfers; they do not prevent them.

**Resolution.** Add an exhaustive money-affecting command matrix: operation, proposer role, confirmer role, distinct-token constraint, delay/TTL, caps, reason, two audit points, replay key, and exception rationale. Make per-market fee override finance+2P with bounded deltas/history. Require an explicit post-clear approval, and two-person compliance/finance clearance when an AML/sanctions flag is attached to a withdrawal. Spell out the full protocol for large withdrawal, manual grant, ban/unban, and any forced refund. If low-value single-finance approval and restorative denial are accepted exceptions, say so, cap them, and test the boundary rather than claiming blanket dual control.

### M5 — One lifetime wager counter cannot safely allocate multiple grants or prevent old/churned volume from qualifying new credits

**Evidence.** D32 proposes only `credit_wager_progress(user_id pk, wagered_micro)` and converts when “progress ≥ `credit_convert_multiple` × granted.” It supports signup, two referral legs, and later manual grants, so a user can have multiple grants at different times. “Settled trade notional” is not defined as buys/sells, gross/net, cash/credit-funded, reversed/unwound, or anti-wash eligible.

**Risk.** A user can accumulate qualifying volume, convert grant A, then receive grant B and have the old lifetime counter immediately qualify it. Concurrent grants/trades can double-consume the same progress. Gross buy/sell churn can manufacture notional; unwind can erase the economic wager after conversion. There is also no rule for partial conversion or what converts after bonus principal was itself spent on a trade.

**Resolution.** Introduce immutable grant lots (source, amount, granted_at, policy version, required wager, remaining/converted amount) and a consumed-progress or per-lot allocation authority. Each trade settlement contributes once via a unique trade id; conversion consumes progress and credit lots atomically. Define eligible notional, treatment of sells/round trips/credit-funded stake/fees/unwinds, allocation order, partial conversion, rounding, and referral grant timing. Add concurrency and replay tests plus a sequence with grant A → qualify → grant B to prove no historical-volume reuse.

### M6 — D35's “one micro pages” reconciliation will page on every normal sent-but-unsettled withdrawal

**Evidence.** D31 books `Withheld → External` only at `settled`, after the on-chain send. D35 compares ledger External against the devnet wallet and says one micro of drift pages. Therefore the wallet has already fallen while the ledger still carries the amount in Withheld for every `sent` row.

**Risk.** The exact amount of all sent-but-unsettled withdrawals appears as drift during healthy operation. Operators cannot distinguish expected in-flight movement from theft or a missing ledger leg. Genesis/treasury funding and refunds can create the same ambiguity unless the baseline and payment facts are named.

**Resolution.** Publish the reconciliation equation and snapshot boundary. If the external ledger leg remains settlement-time, expected wallet balance must subtract uniquely identified broadcast-but-unfinalized outbound receipts (and add observed-but-unbooked finalized inbound suspense facts); only the unexplained residual pages at one micro. Alternatively move the external leg to a rigorously defined broadcast fact and add a different liability/recovery model, but do not mix semantics. Pin opening wallet/treasury funding facts, wallet set, token mint, and any provider-held balances. Test reconciliation at requested, held, approved, broadcast, finalized, denied, definitive-failed, unknown, and refunded states.

### M7 — The new dynamic-config keys have no D24-grade catalog contract

**Evidence.** D31–D35 introduce withdrawal min/max/daily/auto/dual thresholds, KYC tiers, confirmation depth, region policy, AML windows and thresholds, shadow caps, grant amounts, conversion multiple, and stuck-withdrawal SLA. Section 7.0a promises migration “catalog seeds,” but Phase 6 D24 requires each key to have type/bounds, apply class, role/control grade, and max delta, enforced by the typed whole-snapshot validator. No Phase 7 decision extends that catalog or assigns the validator file.

**Risk.** Seeding unknown keys is not enough for the existing typed config path. Workers will either bypass validation, add incompatible keys in parallel, or choose unsafe defaults. Sensitive threshold relationships are also unstated: auto-approve could exceed dual-control threshold; per-tx max could exceed daily max; a zero conversion multiple could mint cash; a policy change while a withdrawal is held could be applied inconsistently.

**Resolution.** Add a complete D24 table for every new key: exact name/JSON type, closed bounds, safe seed, cross-key invariants, apply-to time, authorizing role, two-principal requirement, and max delta. At minimum assert `min ≤ auto ≤ dual ≤ per_tx_max ≤ daily`, positive confirmation depth/multiple/windows, and deny-all behavior for absent compliance policy. State whether held withdrawals use current policy or a request-stamped version at decision/send (prefer current stricter compliance and current kill switches). Give the typed catalog and its tests one owner in 7.0a or one wave.

### M8 — Chain finality, destination, amount, fee, and `failed` semantics are not specified

**Evidence.** D32 says credit at `deposit_confirmations` depth and asserts “Reorg before depth = no credit ever booked.” D31 does not state the commitment/finality level for withdrawal settlement and includes one generic `failed` status. The row stores raw `dest_address`. D8 says withdrawals are charged at network cost, but D31 has only one `amount_micro` and sends that amount.

**Risk.** Depth is meaningful only against a canonical block/slot and a recheck at commit. A post-threshold reorg needs a policy; asserting it cannot occur is not a recovery design. A receipt can report a transaction that used the wrong mint, destination token account, or amount unless settlement verifies all three. Builders cannot tell whether `amount_micro` is gross debit or net delivered, who funds ATA creation/gas, or when a failure is definitive enough to release the hold.

**Resolution.** Pin Solana cluster, USDC mint/decimals, commitment level (normally finalized for irreversible credit/settlement), canonical slot/block identity, and reorg response. Canonicalize/validate destination and include it in the request fingerprint; verify signature, mint, source, destination, exact token delta, and finality before settlement. Split pre-broadcast permanent failure, broadcast-unknown, confirmed-failed/expired, and finalized states. Define gross/net amount and network-fee ledger legs consistent with D8; note that SOL gas and USDC custody reconcile separately. Add wrong-mint/address/amount and late-reorg/late-landing tests.

### M9 — The release acceptance gate regresses from the required full 2,000-agent SLO run to smoke

**Evidence.** D35 says the e2e script's **full-swarm** leg runs with the SLO gate. Spec §9 makes the full 2,000-agent swarm at SLO targets the release gate, and §13.7 repeats that requirement (`docs/spec.md`, lines 372 and 454). Phase 7 exit criterion 9 instead says “smoke swarm passes with `--slo-gate` on.”

**Risk.** A 100-agent smoke can pass while the required close spikes, queue depths, and p99 resolution path fail at 2,000. The plan could claim its beta barrier complete without satisfying its authority document.

**Resolution.** Keep smoke-with-gate for the ordinary local/merge leg, but require one pinned 2,000-agent profile with the 10% full money-path slice, close spike, withdrawals, chaos-healed state, and all three SLO thresholds as the Phase 7 barrier/release artifact. State percentile/sample rules and exclusion rules so a worker cannot “fix” a red result by filtering failures.

### M10 — The plan omits software controls that §12 calls non-negotiable before real money

**Evidence.** Spec §12 lists “responsible-gaming controls (self-exclusion, deposit limits)” among the non-negotiables (`docs/spec.md`, line 436). Phase 7's In/Out lists include bans, shadow limits, and withdrawal limits, but no user self-exclusion, cooling-off period, or deposit-limit enforcement. These are software surfaces, not counsel sign-off or vendor contracts covered by the “external, user-owned gates” carve-out.

**Risk.** The phase can call the compliance software gate complete while omitting an explicit authority requirement. A ban is not self-exclusion: it is admin-controlled, punitive, and dual-controlled, whereas self-exclusion is user-initiated and ordinarily irreversible for a cooling-off interval.

**Resolution.** Add self-exclusion and deposit-limit models, authenticated commands, effective/expiry times, immutable audit/events, and enforcement on deposit initiation/credit availability/trade/withdraw as counsel directs; include a return-of-funds policy for excluded users. If intentionally deferred because all beta money is sandbox-only, say so explicitly and rename the barrier so it cannot be mistaken for the §12 real-money compliance gate.

## Minors

### m1 — The conservation identity is misnumbered and omits existing internal classes

**Evidence.** D31 says “identity 1 becomes deposits − settled withdrawals = user balances + pools + fees + withheld.” Phase 6 D27 calls identity 1 per-transaction sum-zero and identity 3 `−balance(External) = Σ(user ∪ pool ∪ fees ∪ house ∪ escrow)`, per currency. The implemented sweep uses those names.

**Resolution.** Do not replace D27 identity 1. Extend the `OwnerType`/identity-3 internal sum with Withheld and retain House and Escrow. If the product-facing deposits-minus-withdrawals equation is also desired, define its treatment of genesis/treasury funding, refunds, credits, and fees and name it separately. State every equation per currency.

### m2 — `Alerter` deduplication has no incident lifecycle

**Evidence.** D35 defines `page(severity, key, body)` “dedup by key” for invariant failures, AML flags, reconciliation drift, and stuck withdrawals.

**Risk.** Permanent dedup suppresses a later recurrence under the same key; marking a key before delivery can lose the only page; marking after delivery can duplicate under retry.

**Resolution.** Define a durable alert outbox and incident key including detector, subject, and episode/generation/window. Dedup within an open incident, retry delivery at least once, record acknowledgement/resolution, and allow a new page after recovery followed by recurrence. Make the recording fake exercise these transitions.

### m3 — `requested` versus `held` conflates ledger custody with risk-review state

**Evidence.** D31 says the request transaction already books the hold, but “every row enters `requested`” and only non-auto-approved rows become `held`. Exit criterion 3 then calls a large withdrawal “held (dual-control threshold).”

**Risk.** Builders can interpret `held` as “funds moved” even though funds moved for every status, and can disagree on whether `requested` rows are spendable, visible in the risk queue, or included in the Withheld invariant.

**Resolution.** Separate dimensions or rename states: the monetary invariant is `hold_tx_id != null` for every accepted request, while review state can be `screening | review_required | approval_proposed | approved`; send state can be `unsent | sending | broadcast | finalized | definitive_failed | unknown`. Publish the transition table and expected HTTP/notification behavior.
