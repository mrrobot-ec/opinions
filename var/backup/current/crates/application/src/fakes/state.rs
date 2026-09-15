/// One named async lock per key: the fake's stand-in for advisory/row locks.
struct LockMap<K> {
    locks: PlMutex<HashMap<K, Arc<AsyncMutex<()>>>>,
}

impl<K: Eq + Hash + Clone> LockMap<K> {
    fn new() -> Self {
        Self {
            locks: PlMutex::new(HashMap::new()),
        }
    }

    fn handle(&self, k: &K) -> Arc<AsyncMutex<()>> {
        self.locks
            .lock()
            .entry(k.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct StoredTrade {
    id: TradeId,
    new: NewTrade,
    trade_seq: i64,
    created_at: OffsetDateTime,
}

impl StoredTrade {
    fn receipt(&self) -> TradeReceipt {
        TradeReceipt {
            trade_id: self.id,
            ledger_txn: self.new.ledger_txn,
            side: self.new.side,
            action: self.new.action,
            shares: self.new.shares,
            gross: self.new.gross,
            fee: self.new.fee,
            avg_price_micro: self.new.avg_price_micro,
            replayed: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct StoredVote {
    id: VoteId,
    market: MarketId,
    user: UserId,
    side: Side,
    crowd_guess_pct: u8,
    seq: i64,
    idempotency_key: String,
    score: Option<domain::scoring::VoteScore>,
    score_created_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    cast_ip: Option<std::net::IpAddr>,
    device_hash: Option<String>,
}

impl StoredVote {
    fn receipt(
        &self,
        state: MarketState,
        tally_hidden_at: OffsetDateTime,
        now: OffsetDateTime,
    ) -> VoteReceipt {
        let hidden = now >= tally_hidden_at
            && !matches!(
                state,
                MarketState::Resolved | MarketState::Paid | MarketState::Voided
            );
        VoteReceipt {
            vote_id: self.id,
            market: self.market,
            user: self.user,
            side: self.side,
            crowd_guess_pct: self.crowd_guess_pct,
            seq: (!hidden).then_some(self.seq),
            replayed: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct StoredDeposit {
    id: DepositId,
    new: NewDeposit,
}

/// Committed state. Everything a transaction writes lands here atomically.
#[derive(Default, Clone)]
struct State {
    balances: Balances,
    accounts: HashMap<(OwnerRef, Currency), AccountId>,
    txn_keys: HashMap<String, Uuid>,
    ledger_txns: HashMap<Uuid, PendingTxn>,
    markets: HashMap<Uuid, MarketRow>,
    pools: HashMap<Uuid, Pool>,
    pool_seeded: HashMap<Uuid, MicroUsd>,
    lp_results: HashMap<Uuid, (MicroUsd, OffsetDateTime)>,
    collateral_at_close: HashMap<Uuid, MicroUsd>,
    positions: HashMap<(Uuid, Uuid), PositionRow>,
    trades: HashMap<Uuid, StoredTrade>,
    trades_by_txn: HashMap<Uuid, Uuid>,
    votes: BTreeSet<(Uuid, Uuid)>,
    vote_rows: HashMap<Uuid, StoredVote>,
    vote_seq: HashMap<Uuid, i64>,
    tallies: HashMap<Uuid, Tally>,
    deposits: HashMap<Uuid, StoredDeposit>,
    deposits_by_sig: HashMap<String, Uuid>,
    users: HashMap<Uuid, String>,
    user_created_at: HashMap<Uuid, OffsetDateTime>,
    reputation: HashMap<Uuid, (i64, u8)>,
    channels: HashMap<(String, String), Uuid>,
    outcome_resolutions: HashMap<Uuid, (u16, i64)>,
    lifecycle_commands: HashMap<String, LifecycleCommand>,
    integrity_reports: HashMap<Uuid, IntegrityReportRow>,
    invariant_account_balances_override: Option<Vec<crate::model::AccountBalanceRow>>,
    realizations: Vec<RealizationFact>,
    comments: HashMap<Uuid, CommentRow>,
    comment_votes: BTreeSet<(Uuid, Uuid)>,
    comment_reports: HashMap<(Uuid, Uuid), OffsetDateTime>,
    notifier_cursor: i64,
    notifications: Vec<NotificationRow>,
    outbox: Vec<Event>,
    due_query_fails: bool,
    phase5: Phase5State,
    phase6: ops::Phase6Committed,
    phase7: Phase7Committed,
}

/// Phase 7 money facts committed with the rest of the fake.
#[derive(Default, Clone)]
pub(super) struct Phase7Committed {
    pub lots: Vec<crate::money::CreditLotRow>,
    pub lot_keys: HashMap<String, uuid::Uuid>,
    pub allocations: Vec<crate::money::AllocationFact>,
    pub deposits: HashMap<uuid::Uuid, crate::money::DepositMachineRow>,
    pub deposit_sigs: HashMap<String, uuid::Uuid>,
    pub user_status: HashMap<uuid::Uuid, String>,
    pub kyc_tier: HashMap<uuid::Uuid, i32>,
    pub screens: Vec<(uuid::Uuid, String, crate::ports::ScreenVerdict)>,
    pub exclusions: HashMap<uuid::Uuid, time::OffsetDateTime>,
    pub deposit_limits: HashMap<uuid::Uuid, i64>,
    pub phone_verified: HashMap<uuid::Uuid, bool>,
    pub referral_codes: HashMap<uuid::Uuid, String>,
    pub referral_binds: Vec<(uuid::Uuid, crate::model::UserId, crate::model::UserId, String)>,
    pub outbound_payments: Vec<crate::ports::OutboundPaymentRow>,
    pub outbound_attempts: Vec<crate::ports::OutboundAttemptRow>,
    pub decisions: Vec<crate::money::ComplianceDecision>,
    pub proposals: Vec<crate::money::MoneyProposal>,
    pub deposit_cas_successes_before_miss: Option<u32>,
    pub fail_next_fresh_clear: bool,
    pub force_next_live_attempt_state: Option<crate::ports::LandingState>,
    pub referral_party_reads_before_corruption: Option<u32>,
    pub flags: HashMap<String, bool>,
    pub ints: HashMap<String, i64>,
    pub strings: HashMap<String, String>,
}

#[derive(Default, Clone)]
pub(super) struct Phase7Pending {
    pub lots: Vec<crate::money::CreditLotRow>,
    pub lot_keys: Vec<(String, uuid::Uuid)>,
    pub allocations: Vec<crate::money::AllocationFact>,
    pub deposits: Vec<crate::money::DepositMachineRow>,
    pub converted: Vec<(uuid::Uuid, time::OffsetDateTime)>,
    pub status_cas: Vec<(uuid::Uuid, crate::model::DepositMachineStatus)>,
    pub admits: Vec<(uuid::Uuid, uuid::Uuid)>,
    pub refunds: Vec<(uuid::Uuid, uuid::Uuid)>,
    pub outbound_payments: Vec<crate::ports::OutboundPaymentRow>,
    pub outbound_attempts: Vec<crate::ports::OutboundAttemptRow>,
    pub binds: Vec<(uuid::Uuid, crate::model::UserId, crate::model::UserId, String)>,
    pub referral_codes: Vec<(crate::model::UserId, String)>,
    pub decisions: Vec<crate::money::ComplianceDecision>,
    pub proposals: Vec<crate::money::MoneyProposal>,
}

#[allow(clippy::too_many_lines)]
fn apply_phase7(pending: &Phase7Pending, next: &mut State) -> Result<(), StoreError> {
    for lot in &pending.lots {
        if next.phase7.lots.iter().any(|row| row.id == lot.id) {
            return Err(StoreError::Conflict("credit lot"));
        }
        next.phase7.lots.push(lot.clone());
    }
    for (key, id) in &pending.lot_keys {
        next.phase7.lot_keys.insert(key.clone(), *id);
    }
    for fact in &pending.allocations {
        if next
            .phase7
            .allocations
            .iter()
            .any(|row| row.idempotency_key == fact.idempotency_key)
        {
            return Err(StoreError::Conflict("credit allocation"));
        }
        if fact.kind == crate::money::AllocationKind::Allocated
            && next.phase7.allocations.iter().any(|row| {
                row.kind == crate::money::AllocationKind::Allocated
                    && row.trade_id == fact.trade_id
                    && row.lot_id == fact.lot_id
                    && row.split_seq == fact.split_seq
            })
        {
            return Err(StoreError::Conflict("credit allocation split"));
        }
        if let Some(source) = fact.source_allocation_id {
            if next
                .phase7
                .allocations
                .iter()
                .any(|row| row.source_allocation_id == Some(source))
            {
                return Err(StoreError::Conflict("credit allocation terminal"));
            }
        }
        next.phase7.allocations.push(fact.clone());
    }
    for row in &pending.deposits {
        if next.phase7.deposit_sigs.contains_key(&row.chain_sig) {
            return Err(StoreError::Conflict("deposit chain signature"));
        }
        next.phase7.deposit_sigs.insert(row.chain_sig.clone(), row.id.0);
        next.phase7.deposits.insert(row.id.0, row.clone());
    }
    for (id, at) in &pending.converted {
        if let Some(lot) = next.phase7.lots.iter_mut().find(|lot| lot.id == *id) {
            lot.converted_at = Some(*at);
        }
    }
    for (id, status) in &pending.status_cas {
        if let Some(row) = next.phase7.deposits.get_mut(id) {
            row.status = *status;
        }
    }
    for (id, txn) in &pending.admits {
        if let Some(row) = next.phase7.deposits.get_mut(id) {
            row.admit_tx_id = Some(*txn);
        }
    }
    for (id, txn) in &pending.refunds {
        if let Some(row) = next.phase7.deposits.get_mut(id) {
            row.refund_tx_id = Some(*txn);
        }
    }
    for (user, code) in &pending.referral_codes {
        if next.phase7.referral_codes.contains_key(&user.0)
            || next.phase7.referral_codes.values().any(|existing| existing == code)
        {
            return Err(StoreError::Conflict("referral code"));
        }
        next.phase7.referral_codes.insert(user.0, code.clone());
    }
    next.phase7
        .outbound_payments
        .extend(pending.outbound_payments.iter().cloned());
    for attempt in &pending.outbound_attempts {
        if let Some(existing) = next
            .phase7
            .outbound_attempts
            .iter_mut()
            .find(|row| row.id == attempt.id)
        {
            *existing = attempt.clone();
        } else {
            next.phase7.outbound_attempts.push(attempt.clone());
        }
    }
    for bind in &pending.binds {
        if next
            .phase7
            .referral_binds
            .iter()
            .any(|row| row.2 == bind.2 || row.3 == bind.3)
        {
            return Err(StoreError::Conflict("referral bind"));
        }
        next.phase7.referral_binds.push(bind.clone());
    }
    next.phase7.decisions.extend(pending.decisions.iter().cloned());
    next.phase7.proposals.extend(pending.proposals.iter().cloned());
    Ok(())
}

/// Shared phase-5 backing data. Area-specific fake transactions own their
/// buffering/commit logic without changing the frozen core fake machinery.
#[derive(Default, Clone)]
pub(super) struct Phase5State {
    pub(super) drafts: BTreeMap<Uuid, DraftRow>,
    pub(super) video_jobs: BTreeMap<Uuid, VideoJobRow>,
    pub(super) moderation_jobs: BTreeMap<Uuid, ModerationJobRow>,
    pub(super) moderation_cursor: i64,
    pub(super) slot_events: BTreeSet<i64>,
}

struct Shared {
    state: PlMutex<State>,
    key_locks: LockMap<String>,
    market_locks: LockMap<Uuid>,
    pool_locks: LockMap<Uuid>,
    position_locks: LockMap<(Uuid, Uuid)>,
    account_locks: LockMap<Uuid>,
    seq_locks: LockMap<Uuid>,
    user_locks: LockMap<Uuid>,
    comment_locks: LockMap<Uuid>,
    notifier_lock: Arc<AsyncMutex<()>>,
    lp_kill_lock: Arc<AsyncMutex<()>>,
    pub(crate) ops: ops_config::OpsShared,
}

#[derive(Debug, Clone)]
struct PendingTxn {
    key: String,
    id: Uuid,
    txn: Transaction,
    created_at: OffsetDateTime,
}

#[derive(Default)]
struct Pending {
    txns: Vec<PendingTxn>,
    reserves: Vec<(Uuid, Pool)>,
    pool_seeded: Vec<(Uuid, MicroUsd)>,
    lp_results: Vec<(Uuid, MicroUsd, OffsetDateTime)>,
    collateral_at_close: Vec<(Uuid, MicroUsd)>,
    positions: Vec<PositionRow>,
    trades: Vec<StoredTrade>,
    markets: Vec<MarketRow>,
    market_states: Vec<(Uuid, MarketState)>,
    votes: Vec<StoredVote>,
    vote_seq: HashMap<Uuid, i64>,
    vote_scores: Vec<(Uuid, domain::scoring::VoteScore, OffsetDateTime)>,
    deposits: Vec<StoredDeposit>,
    users: Vec<(UserId, String)>,
    user_created_at: Vec<(UserId, OffsetDateTime)>,
    reputation: Vec<ReputationRow>,
    channels: Vec<((String, String), UserId)>,
    outcome_resolutions: Vec<(Uuid, u16, MicroUsd)>,
    lifecycle_commands: Vec<(String, LifecycleCommand)>,
    curator_flags: Vec<(Uuid, Option<OffsetDateTime>)>,
    integrity_due_updates: Vec<(Uuid, Option<OffsetDateTime>)>,
    integrity_reports: Vec<IntegrityReportRow>,
    realizations: Vec<RealizationFact>,
    comments: Vec<NewComment>,
    comment_reply_bumps: Vec<Uuid>,
    comment_votes: Vec<(Uuid, Uuid, i16)>,
    comment_score_updates: Vec<(Uuid, i32)>,
    comment_reports: Vec<(Uuid, Uuid, OffsetDateTime)>,
    comment_status_updates: Vec<(Uuid, ModerationStatus)>,
    comment_report_resets: BTreeSet<Uuid>,
    notifications: Vec<NotificationRow>,
    notifier_cursor: Option<i64>,
    events: Vec<Event>,
    phase6: ops::Phase6Pending,
    phase7: Phase7Pending,
}

/// Deep-comparable view of committed state; equality of two snapshots proves
/// "nothing observable changed" on failure/replay paths.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    balances: BTreeMap<Uuid, i64>,
    txn_keys: BTreeMap<String, Uuid>,
    pools: BTreeMap<Uuid, (i64, i64, u16)>,
    positions: BTreeMap<(Uuid, Uuid), (i64, i64, i64)>,
    trades: BTreeMap<Uuid, Uuid>,
    markets: BTreeMap<Uuid, String>,
    votes: BTreeSet<(Uuid, Uuid)>,
    vote_rows: BTreeMap<Uuid, String>,
    vote_seq: BTreeMap<Uuid, i64>,
    deposits: BTreeMap<Uuid, String>,
    users: BTreeMap<Uuid, String>,
    channels: BTreeMap<(String, String), Uuid>,
    outcome_resolutions: BTreeMap<Uuid, (u16, i64)>,
    lp_results: BTreeMap<Uuid, (i64, OffsetDateTime)>,
    collateral_at_close: BTreeMap<Uuid, i64>,
    realizations: Vec<RealizationFact>,
    drafts: BTreeMap<Uuid, DraftRow>,
    video_jobs: BTreeMap<Uuid, VideoJobRow>,
    moderation_jobs: BTreeMap<Uuid, ModerationJobRow>,
    moderation_cursor: i64,
    slot_events: BTreeSet<i64>,
    outbox_len: usize,
}

/// In-memory [`Store`] + [`MarketQueries`] used by use-case tests, the
/// contract suites, and downstream crates' tests.
#[derive(Clone)]
pub struct InMemoryStore {
    shared: Arc<Shared>,
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: PlMutex::new(State::default()),
                key_locks: LockMap::new(),
                market_locks: LockMap::new(),
                pool_locks: LockMap::new(),
                position_locks: LockMap::new(),
                account_locks: LockMap::new(),
                seq_locks: LockMap::new(),
                user_locks: LockMap::new(),
                comment_locks: LockMap::new(),
                notifier_lock: Arc::new(AsyncMutex::new(())),
                lp_kill_lock: Arc::new(AsyncMutex::new(())),
                ops: ops_config::OpsShared::default(),
            }),
        }
    }
}

fn owner_type(owner: OwnerRef) -> OwnerType {
    match owner {
        OwnerRef::User(_) => OwnerType::User,
        OwnerRef::MarketEscrow(_) => OwnerType::Escrow,
        OwnerRef::MarketPool(_) => OwnerType::Pool,
        OwnerRef::Fees => OwnerType::Fees,
        OwnerRef::House => OwnerType::House,
        OwnerRef::External => OwnerType::External,
        OwnerRef::Withheld => OwnerType::Withheld,
        OwnerRef::DepositSuspense => OwnerType::DepositSuspense,
        OwnerRef::BonusReserve => OwnerType::BonusReserve,
    }
}

fn state_name(s: MarketState) -> &'static str {
    match s {
        MarketState::Draft => "draft",
        MarketState::Scheduled => "scheduled",
        MarketState::Live => "live",
        MarketState::Closing => "closing",
        MarketState::Closed => "closed",
        MarketState::Resolving => "resolving",
        MarketState::Resolved => "resolved",
        MarketState::Paid => "paid",
        MarketState::Voided => "voided",
    }
}

#[cfg(test)]
mod phase7_commit_semantics_tests {
    use super::*;
    use crate::money::{AllocationFact, AllocationKind, CreditLotRow, DepositMachineRow, GrantClass};

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn lot(id_value: u128) -> CreditLotRow {
        CreditLotRow {
            id: id(id_value),
            user: UserId(id(1)),
            source: "commit-race".into(),
            amount_micro: 5,
            granted_at: OffsetDateTime::UNIX_EPOCH,
            grant_class: GrantClass::RealMoney,
            policy_version: "v1".into(),
            converted_at: None,
        }
    }

    fn allocation(
        id_value: u128,
        key: &str,
        kind: AllocationKind,
        source: Option<Uuid>,
    ) -> AllocationFact {
        AllocationFact {
            id: id(id_value),
            trade_id: id(10),
            lot_id: id(11),
            split_seq: 0,
            amount_micro: 5,
            kind,
            source_allocation_id: source,
            idempotency_key: key.into(),
        }
    }

    fn deposit(id_value: u128, signature: &str) -> DepositMachineRow {
        DepositMachineRow {
            id: DepositId(id(id_value)),
            user: Some(UserId(id(1))),
            amount: MicroUsd(5),
            chain_sig: signature.into(),
            source_address: "source".into(),
            dest_address: "destination".into(),
            mint: "USDC".into(),
            rail_fingerprint: "rail".into(),
            slot: 1,
            status: crate::model::DepositMachineStatus::ObservedFinalized,
            suspense_tx_id: Some(id(50)),
            admit_tx_id: None,
            refund_tx_id: None,
        }
    }

    #[test]
    fn concurrent_credit_commits_preserve_lot_allocation_and_terminal_identities() {
        let duplicate_lot = lot(20);
        let mut state = State::default();
        state.phase7.lots.push(duplicate_lot.clone());
        assert_eq!(
            apply_phase7(
                &Phase7Pending {
                    lots: vec![duplicate_lot],
                    ..Phase7Pending::default()
                },
                &mut state,
            ),
            Err(StoreError::Conflict("credit lot"))
        );

        let existing = allocation(21, "same-key", AllocationKind::Allocated, None);
        let mut state = State::default();
        state.phase7.allocations.push(existing);
        assert_eq!(
            apply_phase7(
                &Phase7Pending {
                    allocations: vec![allocation(
                        22,
                        "same-key",
                        AllocationKind::Allocated,
                        None,
                    )],
                    ..Phase7Pending::default()
                },
                &mut state,
            ),
            Err(StoreError::Conflict("credit allocation"))
        );

        let mut state = State::default();
        state
            .phase7
            .allocations
            .push(allocation(23, "first-split", AllocationKind::Allocated, None));
        assert_eq!(
            apply_phase7(
                &Phase7Pending {
                    allocations: vec![allocation(
                        24,
                        "second-split",
                        AllocationKind::Allocated,
                        None,
                    )],
                    ..Phase7Pending::default()
                },
                &mut state,
            ),
            Err(StoreError::Conflict("credit allocation split"))
        );

        let source = allocation(25, "source", AllocationKind::Allocated, None);
        let mut state = State::default();
        state.phase7.allocations.extend([
            source.clone(),
            allocation(26, "terminal-one", AllocationKind::Finalized, Some(source.id)),
        ]);
        assert_eq!(
            apply_phase7(
                &Phase7Pending {
                    allocations: vec![allocation(
                        27,
                        "terminal-two",
                        AllocationKind::Reversed,
                        Some(source.id),
                    )],
                    ..Phase7Pending::default()
                },
                &mut state,
            ),
            Err(StoreError::Conflict("credit allocation terminal"))
        );
    }

    #[test]
    fn concurrent_deposit_and_referral_commits_preserve_unique_identities() {
        let mut state = State::default();
        state.phase7.deposit_sigs.insert("same-signature".into(), id(28));
        assert_eq!(
            apply_phase7(
                &Phase7Pending {
                    deposits: vec![deposit(29, "same-signature")],
                    ..Phase7Pending::default()
                },
                &mut state,
            ),
            Err(StoreError::Conflict("deposit chain signature"))
        );

        let mut state = State::default();
        state.phase7.referral_codes.insert(id(30), "SAMECODE".into());
        assert_eq!(
            apply_phase7(
                &Phase7Pending {
                    referral_codes: vec![(UserId(id(31)), "SAMECODE".into())],
                    ..Phase7Pending::default()
                },
                &mut state,
            ),
            Err(StoreError::Conflict("referral code"))
        );

        let mut state = State::default();
        state.phase7.referral_binds.push((
            id(32),
            UserId(id(33)),
            UserId(id(34)),
            "first-bind".into(),
        ));
        assert_eq!(
            apply_phase7(
                &Phase7Pending {
                    binds: vec![(
                        id(35),
                        UserId(id(36)),
                        UserId(id(34)),
                        "second-bind".into(),
                    )],
                    ..Phase7Pending::default()
                },
                &mut state,
            ),
            Err(StoreError::Conflict("referral bind"))
        );
    }
}
