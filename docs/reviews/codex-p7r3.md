# Phase 7 plan — round 3 adversarial review

## Verdict: FIX-FIRST

Revision 3 is much closer, but it is not yet buildable. Of the **10 round-1 findings that were still partial/unresolved in round 2, 2 are now resolved and 8 remain partial**. Of the **18 NEW round-2 findings, 9 are resolved and 9 remain partial**. The residual defects collapse into **6 blockers, 10 majors, and 3 minors** below.

The minimal pre-wave delta is concrete: choose whether `Paid` is unwindable; reserve cash for every cash-promised lot at grant time; make the seeded config snapshot satisfy its own validator; replace the command-table shorthand with roles and durable proposal authorities the D26 code can represent; publish the withdrawal combination table before 0011 freezes it; give payout the same stable user-lock protection as every other cash-producing path; and specify how existing direct-credit deposit rows cross the suspense migration. The remaining majors are bounded specifications, not a redesign.

## Round-2 disposition verification

### Round-1 findings still open after round 2

| Finding | R3 status | Verification against revision 3 |
|---|---|---|
| **R1-B2 — send exactly-once** | **PARTIAL** | Immutable signed bytes before broadcast, same-byte rebroadcast, monotone attempts, replacement lineage, and a finalized-height condition are now present (`phase7-money-compliance.md:15`). The supposed proof still depends on an unidentified “pinned quorum source”; neither quorum size nor provider/history semantics is pinned. See M2. |
| **R1-B3 — lien and new-cash races** | **PARTIAL** | Withdrawal, deposit, conversion, trade, remedial, and unwind intentions are materially improved (`:16,21,25–26,101,103`). The payout path remains unresolved: the plan orders collection hooks into `resolve_market.rs` but adds `UserLockGuard` only to `UnwindTx`; the real resolver creates user cash while holding the market and `ResolveTx` has no user-lock role. See B5. |
| **R1-B5 — one hold / one terminal disposition** | **PARTIAL** | Explicit `review_state`, `send_state`, transaction ids, legacy-column treatment, and outbound-attempt lineage are now named (`:14–15,20`). The impossible-combination/transition table is still not published: W1 is told to write it only after the coordinator has frozen the migration that must encode its constraints. See M1. |
| **R1-B6 — path-level ownership** | **PARTIAL** | Many formerly missed paths are now named (`:96–104`). Critical paths remain unowned, wildcarded, or simultaneously frozen and wave-owned, including the fail-closed admin capability matrix and the inbound-chain/phone adapters. See M6. |
| **R1-M1 — executable hold-first protocol** | **RESOLVED** | A persisted replay lookup now precedes remote calls; immutable client intent is the fingerprint; a miss performs lock-free screening, guarded key recheck, user lock, locked revalidation, collection, limit reservation, and atomic hold (`:16`). Replay-stable remote refusals are explicit. |
| **R1-M4 — dual-control perimeter** | **PARTIAL** | The promised command table now exists (`:76–94`), but several principals do not exist in D26, proposal authorities are absent from the exhaustive migration list, the Phase-6 row is blank, and the blanket distinct-principal/audit rule contradicts single-principal rows. See B4. |
| **R1-M6 — in-flight reconciliation** | **PARTIAL** | Revision 3 finally writes a signed formula and a common cut (`:39`). It subtracts every broadcast-but-unfinalized outbound even when the transaction has not landed, so an ordinary dropped/unknown attempt becomes false wallet drift. See M4. |
| **R1-M7 — full D24 catalog** | **PARTIAL** | The actual table is a major improvement (`:45–74`), but its shipped seeds violate its cross-key invariant, several required policies/caps remain unnamed, and an existing direct config key conflicts with the new two-phase rule. See B3, M7, M8, and m1. |
| **R1-M8 — chain finality/mint/destination/fee semantics** | **PARTIAL** | Full-USDC delivery with platform-paid SOL gas and receipt field checks are now clear (`:19`). The cluster identity, accepted USDC mint, token decimals, treasury/source token account, and one deposit/withdrawal finality model are still not pinned. See M3. |
| **R1-m3 — money state vs review state** | **RESOLVED** | Coarse, review, and send dimensions are now separately persisted and the alerts/limits use their names (`:14,17,40`). The missing legal combination table is the narrower B5/M3 residual recorded as M1. |

### Findings introduced in round 2

| Round-2 finding | R3 status | Verification against revision 3 |
|---|---|---|
| **NEW-B1 — conversion from reversible fees** | **PARTIAL** | Only fees whose market reached `Paid` can now convert (`:26,111–112`), closing the premature conversion. The required post-conversion unwind is impossible under unchanged D30 and current code, both of which forbid `Paid` unwind. See B1. |
| **NEW-B2 — House is not a bonus reserve** | **PARTIAL** | `BonusReserve` is now a segregated non-negative singleton and only conversion debits it (`:26,98`). Coverage counts only already-qualified lots, however, so an unqualified cash-promised grant can be issued without backing and become unpayable at fee finalization. See B2. |
| **NEW-B3 — remote-first replay** | **RESOLVED** | Persisted hit/fingerprint lookup is first; policy versions are separate screening facts; provider failures and policy bumps cannot pre-empt a committed replay (`:16`). |
| **NEW-B4 — settled withdrawals escape the daily cap** | **RESOLVED** | Every request in the rolling window counts except denied and `send_state=definitive_failed`; settled rows still count (`:17`). |
| **NEW-B5 — new account classes violate 0003** | **RESOLVED** | 0011 is expressly required to replace `ledger_accounts_owner_shape`, extend singleton uniqueness for all three classes, and make decoding total (`:98,101`). |
| **NEW-B6 — unwind market→user deadlock** | **RESOLVED** | The lock-free enumeration → sorted user locks → market lock → re-enumeration/retry algorithm is explicit and `UnwindTx` gains `UserLockGuard` (`:21,98,103`). B5 below concerns payout, not this unwind algorithm. |
| **NEW-M1 — DepositSuspense authority/state/invariant** | **PARTIAL** | The existing `deposits` table is now canonical, has a CAS state machine and XOR ids, uses generalized outbound payments, and has an exact suspense liability invariant (`:15,25`). Legacy direct-credit rows have no backfill rule and `compliance_hold` has no path back to admission. See B6 and M9. |
| **NEW-M2 — non-landing proof and attempt lineage** | **PARTIAL** | Attempt number, replacement link, last-valid height, active-attempt bound, and 1:N/≤1-finalized invariants are present (`:15`). “Pinned quorum source” remains a placeholder, not a proof contract. See M2. |
| **NEW-M3 — persisted withdrawal dimensions/legacy authority** | **PARTIAL** | Columns and legacy authority are now named (`:14`), but the transition/combination table that migration constraints and workers need is deferred to W1. See M1. |
| **NEW-M4 — ownership gaps** | **PARTIAL** | The matrix names most files called out in round 2, but misses newly mandatory integration files and contains pre-wave/frozen versus wave ownership collisions (`:98–103`). See M6. |
| **NEW-M5 — hidden→Paid timer** | **RESOLVED** | The timer is Closed→committed Paid, subtracting only the named hold; `tally_hidden_at` is explicitly excluded and a deterministic trace is required (`:38`). |
| **NEW-M6 — USDC/SOL network-fee dimensionality** | **RESOLVED** | The platform absorbs SOL gas and the user receives/settles the full held USDC amount (`:19`). |
| **NEW-M7 — aggregate-only fee progress** | **PARTIAL** | An append-only allocation table and deterministic split now exist (`:26,98`). The `allocated|finalized|reversed` event algebra, source uniqueness, and lock order are not defined tightly enough to prevent duplicate finalization/reversal. See M5. |
| **NEW-M8 — OTP is not a system** | **PARTIAL** | A `PhoneVerification` port, durable table, sandbox intention, and grant gate are present (`:27,98–100`). The plan omits the verified-number uniqueness constraint and exact sandbox adapter/route ownership. See M6 and M10. |
| **NEW-M9 — bonus-structure transition** | **RESOLVED** | Lots stamp an immutable grant class/promise; changes affect future lots only and preview/refusal protects outstanding liability (`:26,66`). |
| **NEW-m1 — reconciliation signs/cut** | **PARTIAL** | Signs, a baseline, and one cut are now stated (`:39`), but the outbound term selects the wrong chain state. See M4. |
| **NEW-m2 — stale withdrawal names** | **RESOLVED** | Limits and alerts now name persisted states/dimensions (`:17,40`). |
| **NEW-m3 — machine admission as admin audit** | **RESOLVED** | Machine admission writes a compliance decision event; only manual overrides also write `admin_actions` with `AdminContext` (`:25`). |

## Remaining and newly introduced findings

### B1 — Revision 3 requires a `Paid` unwind that D30 and the code categorically prohibit

**Evidence.** Credit conversion is intentionally delayed until the fee's market reaches `Paid`, then D32 requires an unwind of that Paid market to reverse allocations/conversion (`docs/plans/phase7-money-compliance.md:26`). Exit criterion 5 mandates `grant→convert→void→unwind` (`:112`). The command matrix simultaneously says “Phase 6 D30 unchanged” (`:92`). D30 says true unwind is **never** legal from `Paid` (`docs/plans/phase6-simswarm-ops.md:59–62`), and the real implementation rejects it at proposal time (`crates/application/src/ops/unwind_market.rs:1–2,56–68,400–408`) with an explicit Paid-refusal test (`:876–907`).

**Failure.** No worker can make both exit criterion 5 and D30 pass. If it preserves D30, the conversion-reversal branch is dead code and the required test cannot create its precondition. If it silently widens unwind legality, it invalidates Phase 6's security boundary, transition model, audits, tests, and command matrix.

**Resolution.** Pick one rule before the wave. The minimal rule is to preserve “Paid is final,” delete the Paid-unwind conversion branch/test, and state that only a new separately authorized correction command may compensate a Paid market. If Paid unwind is genuinely required, explicitly amend D30/decisions, publish the Paid→correction transition and proposal roles, update `check_unwindable`, enumerate payout/fee/conversion reversal legs and receivables, and replace the matrix's “unchanged” row.

### B2 — `BonusReserve` can still be empty when an issued cash promise becomes qualified

**Evidence.** D32's invariant is `BonusReserve ≥ Σ outstanding finalized-qualified unconverted value`, yet the same sentence says grant issuance refuses rather than unbacking (`phase7-money-compliance.md:26`). A new lot has no finalized allocations, so it contributes zero to that sum even when its immutable `grant_class + redemption promise` promises cash.

**Failure.** With reserve 0, the system may issue a $10 real-money lot while the stated invariant remains green. After the user pays $10 of fees and the market commits Paid, qualification increases the liability by $10. The system must then either reject an immutable fee-finalization fact after the user earned it, violate reserve coverage, or fail the promised conversion. Segregating the account fixed round 2's account-class bug but not solvency at the promise boundary.

**Resolution.** Reserve at grant, not qualification: `BonusReserve ≥ Σ remaining cash redemption promise of every unconverted real_money lot` (or persist an equal per-lot encumbrance). Issue the lot and reserve capacity atomically; release capacity only on conversion, expiry/cancellation allowed by the stamped promise, or an exact correction. Keep qualified liability as a reporting subset, not the solvency floor. Add grant∥top-up∥fee-finalization races with a zero/near-zero reserve.

### B3 — The catalog's shipped seed snapshot violates its own cross-key invariant

**Evidence.** Seeds are `withdraw_max=1_000_000_000`, `withdraw_daily=2_000_000_000`, and `dest_daily=1_000_000_000` (`phase7-money-compliance.md:52–53,58`). The validator-enforced chain requires `max ≤ daily ≤ dest_daily` (`:74`), so `2_000_000_000 ≤ 1_000_000_000` is false. The existing config implementation validates a prospective whole snapshot and rejects a bad cross-key snapshot rather than tolerating it (`crates/application/src/ops/config.rs:644–697`).

**Failure.** 0011 is assigned both those seeds and catalog tests (`phase7-money-compliance.md:98`). A correct validator makes the seed test/startup fail; accepting the seed means the advertised invariant is not enforced. The ordering is also likely conceptually wrong: an aggregate mule-destination cap may legitimately be below one user's daily cap.

**Resolution.** Define independent branches instead of an arbitrary total order: `min ≤ auto ≤ dual ≤ max`, `max ≤ user_daily ≤ hot_wallet_daily`, and `max ≤ dest_daily ≤ hot_wallet_daily` (or document a different risk rule), then choose seeds satisfying it. Add a test that loads the exact migration seed snapshot through the production validator before the coordinator freezes 0011.

### B4 — The “actual” command matrix cannot be represented by D26 or by the listed schema

**Evidence.** The table assigns commands to a `compliance` principal (`phase7-money-compliance.md:83,86,89–90`). D26 has exactly four roles, no hierarchy (`docs/plans/phase6-simswarm-ops.md:36–38`); the real `AdminRole` and startup parser accept only curator/ops/finance/superadmin (`crates/application/src/model.rs:1048–1067`; `crates/adapters/src/http/middleware.rs:47–54`). New `/admin/**` routes without an explicit capability row fail closed (`crates/adapters/src/http/middleware.rs:236–239`), but that middleware file has no Phase-7 owner. The exhaustive 0011 list includes only withdrawal approval proposals, not authorities for ban/unban, manual grants, reserve top-up, manual deposit admission/refund, or frozen-funds licenses (`phase7-money-compliance.md:25,98`). The unwind/write-off/remedial row is blank (`:92`), while “every row” is said to require two audits and distinct tokens even for rows whose confirmer is `—` (`:80,82,85,94`). Named “daily approve cap” and reserve-top-up “daily cap” have no value or catalog key (`:81,88`).

**Failure.** Literal implementation either rejects every compliance command at authentication, invents a fifth role outside D26, or weakens a dual-control command to an audit-only mutation. There is no durable row on which to enforce distinct principal, delay, TTL, payload immutability, replay, or two audit points for several value transfers. A frozen-funds license is an exit-criterion path but has no authority model at all. The blanket rule is impossible for the deliberately single-principal exceptions.

**Resolution.** Use D26 capabilities granted to the existing roles, or explicitly amend D26 and every parser/audit schema for a fifth role. Add a generic typed `money_command_proposals` authority or named per-command tables to 0011 with subject/payload hash, proposer/confirmer token ids, `confirm_not_before`, `expires_at`, caps, reason, status, and replay key. Publish exact capability rows and give `middleware.rs` an owner. Qualify the footer: proposal rows get two audits/distinct tokens/TTL; named single-principal exceptions get one atomic audit. Fill the Phase-6 row by reference to exact roles/delay and pin every cap. Define expiry as a window after `confirm_not_before`, so a 15-minute delay plus 15-minute TTL is not a zero-width confirmation window.

### B5 — Payout still creates withdrawable cash without a stable user lock/receivable collection protocol

**Evidence.** The matrix assigns the coordinator to add “collection hooks” to `resolve_market.rs` (`phase7-money-compliance.md:103`), but 7.0a adds `UserLockGuard` only to `UnwindTx` (`:98`). In the real resolver, recipient holdings are read after the market row is locked and payout entries credit user accounts (`crates/application/src/resolve_market.rs:151–164,257–271`). `ResolveTx` does not compose `UserLockGuard` (`crates/application/src/ports/market.rs:390–417`). Withdrawal, by contrast, locks the user before checking/collecting/holding (`phase7-money-compliance.md:16`).

**Failure.** Leaving payout unchanged lets a concurrent withdrawal pass the receivable/lien check and then receive new payout cash uncollected. Adding `lock_user` after `market_for_update` creates the same market→user / user→market cycle revision 3 correctly removed from unwind. A vague “hook” cannot satisfy both atomic collection and lock ordering.

**Resolution.** Publish a payout-recipient algorithm before editing the port: lock-free holder/user enumeration → sorted user locks → market lock → holder re-enumeration/retry, then payout plus oldest-first collections in the same transaction. Add `UserLockGuard` to `ResolveTx`, name its Pg/fake effects, and race payout against PlaceTrade, withdrawal request, deposit admission, remedial credit, and unwind. If that lock cost cannot meet the 1-second SLO, keep payout cash encumbered in a per-user authority and release/collect later; do not expose it unlocked.

### B6 — 0011 cannot map existing direct-credit deposits into the new suspense authority as written

**Evidence.** The current `deposits` table has `seen|confirmed|credited` plus one generic `txn_id` (`migrations/0001_init.sql:299–307`). The actual use case writes one direct `External → User` transaction and stores it on the deposit (`crates/application/src/credit_deposit.rs:79–104`). Revision 3 ALTERs that table into an observation→admission/refund machine requiring `suspense_tx_id` and terminal `admit_tx_id XOR refund_tx_id` (`phase7-money-compliance.md:25`), but—unlike withdrawals—states no deposit backfill, quarantine, legacy `txn_id` authority, or empty-table precondition (`:14,98`).

**Failure.** A previously credited row has no `External → DepositSuspense` transaction to reference and cannot truthfully satisfy the new observation-leg identity. Relabeling its original direct transaction as `suspense_tx_id` misstates its accounts; relabeling it `admit_tx_id` leaves no suspense observation. Reversing/rebooking history can require debiting a user who already spent the cash. Coordinator and W3 therefore cannot independently choose compatible constraints/invariants.

**Resolution.** Pin one migration rule: either abort 0011 unless the sandbox deposit table is empty and require a documented fresh-DB cut, or grandfather rows as `admitted_legacy` with an explicit identity-4 carve-out and `txn_id→admit_tx_id` mapping. If history must be normalized, specify non-negativity-safe compensating legs/receivables. State whether `user_id` may be null for unmatched finalized inflows, and replace/retire generic `txn_id`. Add upgrade tests containing credited, confirmed, and seen rows.

### M1 — The withdrawal transition table is still a post-freeze deliverable

**Evidence.** D31 promises a table of every CAS and coupled ledger/audit/event/outbox effect in `docs/copy/ops.md`, owned by W1 (`phase7-money-compliance.md:14,99`). The current file contains no `review_state` or `send_state` table. Yet the coordinator must first implement and freeze 0011's impossible-combination constraints (`:98`).

**Risk.** The migration author must invent legal combinations before the table's owner writes them. W1 can then document a different graph but cannot change the frozen CHECKs. This keeps R1-B5/NEW-M3 partial despite the welcome explicit columns.

**Resolution.** Put the complete product-state × review-state × send-state table in this plan or make its finalized draft a 7.0a input before migration work. Include hold/release/settle nullability, outbound-payment existence, attempt state, each CAS authority, failure/retry edges, and legacy mappings; derive CHECK tests from the same table.

### M2 — “Pinned quorum source” is still not a non-landing proof

**Evidence.** D31 requires finalized height beyond `last_valid_block_height` plus signature absence from “the pinned quorum source” (`phase7-money-compliance.md:15`). No catalog row, immutable environment contract, provider count, agreement threshold, historical-search requirement, or owner names that source.

**Risk.** One worker may treat one lagging RPC's `not found` as proof while another expects 2-of-3 archival RPCs. The former can replace a transaction whose older signature can still appear; the latter cannot be configured or tested. Attempt lineage does not make an unsound failure oracle safe.

**Resolution.** Pin an executable predicate: cluster/genesis identity; `Q of N` independent endpoints; finalized block-height reads; signature-history/archive lookup (including pruned-history behavior); disagreement/timeout→`unknown`; and the evidence persisted on the attempt. Add named config/env validation and two-node-lag/one-node-partition tests.

### M3 — The rail verifies a mint that the plan never identifies

**Evidence.** Withdrawal receipt verification names signature, mint, source, destination token account, amount, and finalized commitment (`phase7-money-compliance.md:19`), while deposits use configurable confirmation depth (`:25,60`). The document never pins the Solana cluster identity/genesis hash, accepted devnet USDC mint pubkey, decimals, source/hot-wallet token account, or why deposit “32 confirmations” and withdrawal `finalized` are the same finality contract. This is the exact non-gas portion left open in R1-M8.

**Risk.** A correct signature for the wrong token/cluster can satisfy a structurally correct adapter; amount decoding can differ by 10^6; deposit and withdrawal workers can use inconsistent finality. “Devnet/sandbox” is an environment class, not an identity.

**Resolution.** Add immutable rail configuration validated at startup and fingerprinted into observations/payments: cluster genesis hash, RPC set, mint pubkey, decimals, treasury owner/token account, commitment, and deposit-depth interpretation. Include wrong-cluster/mint/decimals/source tests.

### M4 — The reconciliation formula treats mere broadcast as a wallet debit

**Evidence.** D35 states `wallet = −External − Σ(broadcast-unfinalized outbound) + Σ(finalized inbound not booked)` (`phase7-money-compliance.md:39`). D31 explicitly permits an `unknown` attempt that may never have landed and retains the encumbrance (`:15`).

**Risk.** If an outbound was broadcast but dropped, the finalized wallet still contains A while the formula subtracts A, generating an A-micro residual until expiry/non-landing proof. If it landed only at processed/confirmed commitment, the answer depends on which wallet commitment the balance reader used. That contradicts “only residual pages” and “in-flight does not.”

**Resolution.** Compare at one pinned finalized chain cut. Subtract **finalized-on-chain outbound not yet ledger-settled**, not every broadcast/unknown payment; add finalized inbound not yet ledger-booked. Report broadcast/unknown exposure separately without calling it conservation drift. Persist the observation slot/status used in each term and test dropped, confirmed-only, finalized-before-DB, and replacement states numerically.

### M5 — Allocation event kinds do not yet define a unique state transition algebra

**Evidence.** D32 lists positive-amount facts with `kind allocated|finalized|reversed` and a `source_allocation_id`, plus aggregate net bounds (`phase7-money-compliance.md:26`). It does not say whether `finalized` adds to or consumes provisional allocation, whether both `finalized` and `reversed` may reference one source, which fact a post-Paid reversal references, or what uniqueness constraints enforce those answers. It also says “lot row lock” and “same `lock_user` tx” without pinning user→lot order.

**Risk.** Counting allocated+finalized double-counts one fee; treating kinds only as labels permits two finalizations or finalize-after-reverse; a generic reversal can over-reverse across lots/trades. Concurrent conversion can deadlock a path that takes user then lot if conversion takes lot then user.

**Resolution.** Publish the signed materialized views and transition constraints. For example, one `allocated` fact per `(trade,lot,split_seq)`, at most one terminal child per source; finalization moves the same amount from provisional to finalized rather than adding economic progress; reversal names the exact live source and is unique. Lock `user → sorted lots → BonusReserve/accounts`, then write allocation facts, both ledger txns, and `converted_at`. Add duplicate-finalize, finalize-vs-reverse, split, and convert-vs-unwind races.

### M6 — The ownership matrix still has fail-closed gaps and frozen-file collisions

**Evidence.** New compliance admin routes require edits to `crates/adapters/src/http/middleware.rs`, whose fail-closed matrix rejects unknown admin routes (`middleware.rs:236–239`), but §2 assigns no owner. No exact inbound-chain watcher/indexer, PhoneVerification sandbox adapter, or phone challenge route is owned. W2 says “their fakes/dtos/tests,” W1 says “withdrawal fakes,” and W4 says “`web/` money surfaces + Playwright,” despite the “one owner per exact path” heading (`phase7-money-compliance.md:99–102`). More seriously, 7.0a is “then frozen” and includes `application/src/ops/config.rs`/PgStore placeholders, while W3 is told to edit `ops/config.rs` and `pg/store.rs` (`:98,101`).

**Risk.** Compliance routes ship as 403, the 10% inbound money path has no producer, phone verification has no callable sandbox flow, and W3 must either violate the freeze or omit fee/config integration. This is precisely the no-VCS conflict the matrix is meant to prevent.

**Resolution.** Add exact rows for middleware capability additions, inbound watcher/transport, PhoneVerification provider and public challenge/verify routes, every fake/test/web path, and generated outputs. Mark explicit 7.0a→W3 ownership transfers for `ops/config.rs`, `pg/store.rs`, and any fake file, or have 7.0a finish those edits. Do not label transferred files frozen. Give payout/unwind Pg race tests named owners.

### M7 — An absent fee override cannot be reverted through the existing config authority

**Evidence.** The catalog seeds `fee_bps_override:{market}` as absent and the command matrix promises write/**revert** (`phase7-money-compliance.md:72,91`). Existing `config_entries.value` and `config_changes.new` are non-null (`migrations/0008_ops.sql:9–12,34–39`); the application treats patches only as key→new-value upserts and explicitly rejects reverting a key's creation (`crates/application/src/ops/proposals.rs:145–178,850–855`). The plan does not assign 0011 a deletion/tombstone change.

**Risk.** The first override can be written but can never return to “use the pool stamp.” Writing the pool fee as an override is not equivalent: it leaves an override fact, changes drift/replay semantics, and can become stale relative to the immutable market base. The 20bp delta also lacks a defined baseline when the key is absent.

**Resolution.** Define an explicit unset operation. Alter history to represent `new=None`/tombstone, delete the live entry atomically, and make replay/revert/staleness consume that fact; or use a typed enum `inherit|override(bps)` with seed `inherit`. For the first write, compare against the market's immutable pool fee. Add set→unset→replay and old-preview drift tests.

### M8 — The catalog still omits policies referenced by the design and contradicts the existing referral key

**Evidence.** D32 changes `feature_referrals` to two-phase (`phase7-money-compliance.md:27`), but the Phase-7 catalog has no row for it; the current catalog makes it direct Ops/immediate (`crates/application/src/ops/config.rs:525–530`). `region_allowset_version` is listed without the allowset contents (`phase7-money-compliance.md:71`), although R1-M7 explicitly called that out. D34 promises pinned structuring N/window/**threshold**, but only N/window and broad velocity amounts appear (`:33,67–68`). The command table also names daily large-withdrawal and BonusReserve top-up caps without catalog entries (`:81,88`).

**Risk.** A correct unknown-key validator cannot implement the unstated policies, while the existing referral switch remains a one-principal bypass of the new precondition. Workers will hard-code different thresholds/caps or silently keep Phase 6 behavior.

**Resolution.** Add exact rows for `feature_referrals` (bool, false, two-phase role/apply/precondition), the actual region allowset plus monotonically increasing version, structuring amount/bucket semantics, approval daily cap, and reserve top-up daily cap. State whether per-user and per-destination AML share thresholds. Test the complete catalog against every key referenced by D31–D36 and the command matrix.

### M9 — `compliance_hold` has no recovery path to admission

**Evidence.** The deposit graph is written as `observed_finalized → admission_pending → admitted | compliance_hold → refund_approved → refund_sending → refunded` (`phase7-money-compliance.md:25`). There is no `compliance_hold → admission_pending/admitted` edge even though KYC, geo, sanctions indeterminacy, deposit limits, and manual overrides can later change; the same paragraph says manual overrides can cause admission.

**Risk.** A temporary provider outage or KYC renewal permanently forces a source refund, while an implementer who adds a hidden release edge will disagree with migration CHECKs, CASs, audit semantics, and the suspense invariant.

**Resolution.** Publish separate reevaluation edges: hold→admission_pending on fresh facts, then admitted on a machine decision; manual hold→admitted only through the command-matrix authority and both compliance/admin records. Preserve hold reason/version history and race each release against refund approval/send.

### M10 — Phone verification still cannot prove one grant per verified phone

**Evidence.** D32 names a number HMAC and `verified_at` but does not require uniqueness on the normalized-number HMAC (`phase7-money-compliance.md:27`). The ownership matrix names the application use case but no concrete sandbox provider adapter or public challenge/verify route (`:100`). The existing `user_channels` table only makes `(channel,address)` unique and contains no verification fact (`migrations/0003_accounts_identity.sql:26–32`).

**Risk.** Multiple verification rows/accounts can reuse one phone and each satisfy the grant gate; alternatively W2 can build an application port with no route/adapter that W3 cannot exercise. That leaves the D21 launch gate and round-2 acceptance test nominal rather than enforceable.

**Resolution.** Require a unique verified normalized-number HMAC (with explicit key-version/rotation treatment), one active challenge per number/account, atomic attempt/rate-limit consumption, and a durable account bind. Name the provider adapter, HTTP DTO/routes, composition, fakes, and tests. Race two accounts verifying the same number and prove only one can become grant-eligible.

### m1 — Zero-valued settings make the catalog's multiplicative deltas ambiguous

**Evidence.** `withdraw_auto_approve_micro`, credit amounts/caps, and shadow caps allow zero while advertising `4×`/`10×` max deltas (`phase7-money-compliance.md:54,62–65,69`). The cross-key rule simultaneously makes `withdraw_auto_approve=0` impossible because `withdraw_min≥1_000_000` and `min≤auto` (`:51,74`). Existing multiplicative validation bypasses the ratio when prior is zero (`crates/application/src/ops/config.rs:108–115`).

**Resolution.** Define zero as a disable sentinel and specify a bounded re-enable baseline, or remove zero from bounds. Make the auto-approval invariant conditional when disabled. Add zero→enabled and enabled→zero tests for every multiplicative key.

### m2 — One `N_MIN_*` constant set is ambiguous across smoke and 2,000-agent profiles

**Evidence.** D35 derives `N_MIN_TRADE` from the convoy count and requires both gated smoke and the pinned 2,000-agent profile to pass the same library gates (`phase7-money-compliance.md:38,116`). Trade sample counts scale with the roster, but no per-profile constants or exact integers are given.

**Resolution.** Publish exact minimums for smoke and full profiles (or a manifest-derived expected-series contract) and assert `observed ≥ minimum` for each. Make the ≥50%-convoy assertion separate from the latency-series thinness gate.

### m3 — Revision 3 relies on an overwritten revision instead of restating authority

**Evidence.** D31 says identities “(a)–(e) as in rev 2”; D33, parts of D34/D35, and D36 say “as rev 2” (`phase7-money-compliance.md:20,29–30,34,40,42–43`). The plan path now contains revision 3; there is no revision-2 plan artifact, only review/disposition summaries.

**Resolution.** Restate the exact identities, result/TTL/default algebra, responsible-gaming transitions, incident lifecycle, and fee-override protocol in revision 3 or link an immutable retained artifact. Build workers should not reconstruct normative text from review quotes.
