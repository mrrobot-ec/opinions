//! In-memory W1 store: hold-first withdrawals, outbound lineage, proposals.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use domain::ledger::{AccountId, Balances, Currency, Entry, OwnerType, Transaction, TxnKind};
use domain::money::MicroUsd;
use parking_lot::Mutex;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::model::{AdminAction, Event, NewNotification, UserId, UserStatus, WithdrawalStatus};
use crate::ports::{
    canonicalize_dest, Combo, DestWarmth, LandingState, MoneyProposal, OpenReceivable,
    OutboundAttemptRow, OutboundIo, OutboundPaymentRow, OutboundSubject, ScreenVerdict,
    UserMoneyView, WithdrawIo, WithdrawLimits, WithdrawStore, WithdrawTx, WithdrawalId,
    WithdrawalReceipt, WithdrawalRow,
};
use crate::ports::{
    AuditWrite, Committable, IdempotencyGuard, OutboxWriter, RailIdentity, UserLockGuard,
};

#[derive(Clone)]
struct UserRec {
    status: UserStatus,
    kyc_tier: i32,
    self_excluded_until: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum FakeOwner {
    User(Uuid),
    Withheld,
    External,
    House,
}

#[derive(Clone)]
struct ScreeningFact {
    user: Uuid,
    context: String,
    verdict: ScreenVerdict,
}

#[derive(Clone)]
struct ReceivableRec {
    id: Uuid,
    user: Uuid,
    outstanding: i64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum FakeTxFault {
    #[default]
    None,
    LockUser,
    Commit,
}

#[derive(Clone, Default)]
struct FakeFaults {
    suppress_lock_free_fingerprint: bool,
    force_cas_miss: bool,
    tx_fault: FakeTxFault,
    fail_settle: bool,
}

#[derive(Clone)]
pub struct FakeState {
    users: HashMap<Uuid, UserRec>,
    accounts: HashMap<(FakeOwner, Currency), AccountId>,
    balances: Balances,
    txn_keys: HashMap<String, Uuid>,
    fingerprints: HashMap<String, WithdrawalReceipt>,
    idempotency: HashMap<String, String>,
    withdrawals: BTreeMap<Uuid, WithdrawalRow>,
    events: Vec<(WithdrawalId, String, String, serde_json::Value)>,
    screenings: Vec<ScreeningFact>,
    aml_open: HashSet<Uuid>,
    proposals: HashMap<Uuid, MoneyProposal>,
    payments: HashMap<Uuid, OutboundPaymentRow>,
    attempts: HashMap<Uuid, OutboundAttemptRow>,
    outbox: Vec<Event>,
    notifications: Vec<NewNotification>,
    audits: Vec<AdminAction>,
    receivables: Vec<ReceivableRec>,
    finance_approves: Vec<(OffsetDateTime, i64)>,
    limits: WithdrawLimits,
    refund_dests: HashSet<String>,
    dest_distinct_users: HashMap<String, u32>,
    observation_sources: HashSet<(Uuid, String)>,
    lazy_conversion_calls: Vec<Uuid>,
    strict_withdraw_lock_order: bool,
    withdrawal_user_overrides: HashMap<Uuid, UserId>,
    faults: FakeFaults,
    now: OffsetDateTime,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            users: HashMap::new(),
            accounts: HashMap::new(),
            balances: Balances::default(),
            txn_keys: HashMap::new(),
            fingerprints: HashMap::new(),
            idempotency: HashMap::new(),
            withdrawals: BTreeMap::new(),
            events: Vec::new(),
            screenings: Vec::new(),
            aml_open: HashSet::new(),
            proposals: HashMap::new(),
            payments: HashMap::new(),
            attempts: HashMap::new(),
            outbox: Vec::new(),
            notifications: Vec::new(),
            audits: Vec::new(),
            receivables: Vec::new(),
            finance_approves: Vec::new(),
            limits: WithdrawLimits::seed(),
            refund_dests: HashSet::new(),
            dest_distinct_users: HashMap::new(),
            observation_sources: HashSet::new(),
            lazy_conversion_calls: Vec::new(),
            strict_withdraw_lock_order: false,
            withdrawal_user_overrides: HashMap::new(),
            faults: FakeFaults::default(),
            now: OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
        }
    }
}

/// Standalone withdrawal store used by W1 unit tests.
pub struct FakeWithdrawStore {
    state: Arc<Mutex<FakeState>>,
}

impl Default for FakeWithdrawStore {
    fn default() -> Self {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let mut state = FakeState {
            limits: WithdrawLimits::seed(),
            now,
            ..FakeState::default()
        };
        open_owner(&mut state, FakeOwner::Withheld, Currency::Usdc);
        open_owner(&mut state, FakeOwner::External, Currency::Usdc);
        open_owner(&mut state, FakeOwner::House, Currency::Usdc);
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }
}

impl FakeWithdrawStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make row-lock acquisition fail unless the row's user lock was taken
    /// first. Tests use this to model the global D31 lock order.
    pub fn enforce_withdraw_lock_order(&self) {
        self.state.lock().strict_withdraw_lock_order = true;
    }

    /// Model a lock-free miss followed by a fingerprint hit under the write
    /// lock, as happens when another request commits between the two reads.
    pub fn suppress_lock_free_fingerprint(&self) {
        self.state.lock().faults.suppress_lock_free_fingerprint = true;
    }

    /// Corrupt the lock-free owner projection without changing the locked
    /// withdrawal row, exercising the post-lock owner invariant.
    pub fn override_withdrawal_user(&self, id: WithdrawalId, user: UserId) {
        self.state
            .lock()
            .withdrawal_user_overrides
            .insert(id.0, user);
    }

    /// Make the next transaction-local withdrawal CAS lose its race.
    pub fn force_next_cas_miss(&self) {
        self.state.lock().faults.force_cas_miss = true;
    }

    /// Make transactions opened from this fake fail user-lock acquisition.
    pub fn fail_lock_user(&self) {
        self.state.lock().faults.tx_fault = FakeTxFault::LockUser;
    }

    /// Make transactions opened from this fake fail at commit.
    pub fn fail_commit(&self) {
        self.state.lock().faults.tx_fault = FakeTxFault::Commit;
    }

    /// Make the next transaction-local settle ledger write fail as a backend
    /// error, so callers prove that they do not CAS the row afterward.
    pub fn fail_next_settle(&self) {
        self.state.lock().faults.fail_settle = true;
    }

    #[must_use]
    pub fn seed_user(&self, status: UserStatus, kyc_tier: i32) -> UserId {
        let user = UserId(Uuid::new_v4());
        let mut state = self.state.lock();
        state.users.insert(
            user.0,
            UserRec {
                status,
                kyc_tier,
                self_excluded_until: None,
            },
        );
        open_owner(&mut state, FakeOwner::User(user.0), Currency::Usdc);
        user
    }

    pub fn credit(&self, user: UserId, amount: i64) {
        let mut state = self.state.lock();
        let external = *state
            .accounts
            .get(&(FakeOwner::External, Currency::Usdc))
            .unwrap_or(&AccountId(Uuid::nil()));
        let user_acct = *state
            .accounts
            .get(&(FakeOwner::User(user.0), Currency::Usdc))
            .unwrap_or(&AccountId(Uuid::nil()));
        let key = format!("seed-credit:{}:{amount}", user.0);
        apply_txn(
            &mut state,
            TxnKind::Deposit,
            &key,
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-amount),
                },
                Entry {
                    account: user_acct,
                    amount: MicroUsd(amount),
                },
            ],
        )
        .ok();
    }

    pub fn set_self_excluded(&self, user: UserId, until: OffsetDateTime) {
        if let Some(rec) = self.state.lock().users.get_mut(&user.0) {
            rec.self_excluded_until = Some(until);
        }
    }

    pub fn set_status(&self, user: UserId, status: UserStatus) {
        if let Some(rec) = self.state.lock().users.get_mut(&user.0) {
            rec.status = status;
        }
    }

    pub fn set_kyc(&self, user: UserId, kyc_tier: i32) {
        if let Some(rec) = self.state.lock().users.get_mut(&user.0) {
            rec.kyc_tier = kyc_tier;
        }
    }

    pub fn set_pause(&self, paused: bool) {
        self.state.lock().limits.pause_withdrawals = paused;
    }

    pub fn set_auto_approve(&self, micro: i64) {
        self.state.lock().limits.auto_approve_micro = micro;
    }

    pub fn set_dual(&self, micro: i64) {
        self.state.lock().limits.dual_control_micro = micro;
    }

    pub fn set_approve_daily_cap(&self, micro: i64) {
        self.state.lock().limits.approve_daily_cap_micro = micro;
    }

    pub fn mark_refund_dest(&self, dest: &str) {
        self.state.lock().refund_dests.insert(dest.to_string());
    }

    pub fn set_dest_distinct_users(&self, dest: &str, users: u32) {
        self.state
            .lock()
            .dest_distinct_users
            .insert(dest.to_string(), users);
    }

    pub fn mark_observation_source(&self, user: UserId, dest: &str) {
        let mut state = self.state.lock();
        state.refund_dests.insert(dest.to_string());
        state.observation_sources.insert((user.0, dest.to_string()));
    }

    pub fn open_aml(&self, user: UserId) {
        self.state.lock().aml_open.insert(user.0);
    }

    pub fn clear_aml(&self, user: UserId) {
        self.state.lock().aml_open.remove(&user.0);
    }

    #[must_use]
    pub fn add_receivable(&self, user: UserId, amount: i64) -> Uuid {
        let id = Uuid::new_v4();
        self.state.lock().receivables.push(ReceivableRec {
            id,
            user: user.0,
            outstanding: amount,
        });
        id
    }

    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        self.state.lock().now
    }

    pub fn advance(&self, hours: i64) {
        let mut state = self.state.lock();
        state.now += time::Duration::hours(hours);
    }

    pub fn set_now(&self, now: OffsetDateTime) {
        self.state.lock().now = now;
    }

    #[must_use]
    pub fn lazy_conversion_calls(&self) -> usize {
        self.state.lock().lazy_conversion_calls.len()
    }

    #[must_use]
    pub fn withheld(&self) -> i64 {
        balance_of(&self.state.lock(), FakeOwner::Withheld)
    }

    #[must_use]
    pub fn notifications(&self) -> Vec<NewNotification> {
        self.state.lock().notifications.clone()
    }

    #[must_use]
    pub fn withdrawals(&self) -> Vec<WithdrawalRow> {
        self.state.lock().withdrawals.values().cloned().collect()
    }

    #[must_use]
    pub fn attempts(&self) -> Vec<OutboundAttemptRow> {
        self.state.lock().attempts.values().cloned().collect()
    }

    /// Corrupt a persisted outbound intent for invariant-path tests.
    #[must_use]
    pub fn corrupt_outbound_dest(&self, subject_id: Uuid, dest: &str) -> bool {
        let mut state = self.state.lock();
        let Some(payment) = state
            .payments
            .values_mut()
            .find(|payment| payment.subject_id == subject_id)
        else {
            return false;
        };
        payment.dest = dest.to_string();
        true
    }
}

fn open_owner(state: &mut FakeState, owner: FakeOwner, currency: Currency) -> AccountId {
    if let Some(id) = state.accounts.get(&(owner, currency)) {
        return *id;
    }
    let id = AccountId(Uuid::new_v4());
    let kind = match owner {
        FakeOwner::User(_) => OwnerType::User,
        FakeOwner::Withheld => OwnerType::Withheld,
        FakeOwner::External => OwnerType::External,
        FakeOwner::House => OwnerType::House,
    };
    state.balances.open(id, kind, currency).ok();
    state.accounts.insert((owner, currency), id);
    id
}

fn balance_of(state: &FakeState, owner: FakeOwner) -> i64 {
    let Some(id) = state.accounts.get(&(owner, Currency::Usdc)) else {
        return 0;
    };
    state.balances.balance(*id).map_or(0, |amount| amount.0)
}

fn apply_txn(
    state: &mut FakeState,
    kind: TxnKind,
    key: &str,
    entries: &[Entry],
) -> Result<Uuid, StoreError> {
    if state.txn_keys.contains_key(key) {
        return Err(StoreError::DuplicateKey);
    }
    let txn = Transaction::new(kind, entries.to_vec()).map_err(StoreError::Ledger)?;
    state.balances.apply(&txn).map_err(StoreError::Ledger)?;
    let id = Uuid::new_v4();
    state.txn_keys.insert(key.to_string(), id);
    Ok(id)
}

struct FakeWithdrawTx {
    shared: Arc<Mutex<FakeState>>,
    state: FakeState,
    committed: bool,
    locked_users: HashSet<Uuid>,
}

#[async_trait]
impl IdempotencyGuard for FakeWithdrawTx {
    async fn serialize_key(&mut self, _key: &str) -> Result<(), StoreError> {
        Ok(())
    }
}

#[async_trait]
impl UserLockGuard for FakeWithdrawTx {
    async fn lock_user(&mut self, user: UserId) -> Result<(), StoreError> {
        if self.state.faults.tx_fault == FakeTxFault::LockUser {
            return Err(StoreError::Backend("forced user-lock failure".into()));
        }
        self.locked_users.insert(user.0);
        Ok(())
    }
}

#[async_trait]
impl crate::money::LazyCreditConversion for FakeWithdrawTx {
    async fn convert_then_collect(
        &mut self,
        user: UserId,
        _now: OffsetDateTime,
        key: &str,
    ) -> Result<crate::money::ConvertCollectReceipt, AppError> {
        if !self.locked_users.contains(&user.0) {
            return Err(StoreError::Invariant("credit conversion before user lock").into());
        }
        self.state.lazy_conversion_calls.push(user.0);
        let owed = self
            .state
            .receivables
            .iter()
            .filter(|row| row.user == user.0 && row.outstanding > 0)
            .try_fold(0_i64, |sum, row| {
                sum.checked_add(row.outstanding).ok_or(AppError::Overflow)
            })?;
        let collect = owed.min(balance_of(&self.state, FakeOwner::User(user.0)).max(0));
        if collect > 0 {
            let user_account = open_owner(&mut self.state, FakeOwner::User(user.0), Currency::Usdc);
            let house = open_owner(&mut self.state, FakeOwner::House, Currency::Usdc);
            apply_txn(
                &mut self.state,
                TxnKind::Reversal,
                &format!("recv-collect:{key}"),
                &[
                    Entry {
                        account: user_account,
                        amount: MicroUsd(-collect),
                    },
                    Entry {
                        account: house,
                        amount: MicroUsd(collect),
                    },
                ],
            )?;
            let mut remaining = collect;
            for row in self
                .state
                .receivables
                .iter_mut()
                .filter(|row| row.user == user.0 && row.outstanding > 0)
            {
                if remaining == 0 {
                    break;
                }
                let take = remaining.min(row.outstanding);
                row.outstanding -= take;
                remaining -= take;
            }
        }
        Ok(crate::money::ConvertCollectReceipt {
            collected_micro: collect,
            ..crate::money::ConvertCollectReceipt::default()
        })
    }
}

#[async_trait]
impl OutboxWriter for FakeWithdrawTx {
    async fn append(&mut self, event: Event) -> Result<(), StoreError> {
        self.state.outbox.push(event);
        Ok(())
    }
    async fn append_batch(&mut self, events: &[Event]) -> Result<(), StoreError> {
        self.state.outbox.extend(events.iter().cloned());
        Ok(())
    }
}

#[async_trait]
impl AuditWrite for FakeWithdrawTx {
    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError> {
        self.state.audits.push(action);
        Ok(())
    }
}

#[async_trait]
impl Committable for FakeWithdrawTx {
    async fn commit(mut self: Box<Self>) -> Result<(), StoreError> {
        if self.state.faults.tx_fault == FakeTxFault::Commit {
            return Err(StoreError::Backend("forced commit failure".into()));
        }
        self.committed = true;
        *self.shared.lock() = self.state.clone();
        Ok(())
    }
}

#[async_trait]
impl WithdrawIo for FakeWithdrawTx {
    async fn withheld_balance(&mut self) -> Result<i64, StoreError> {
        Ok(balance_of(&self.state, FakeOwner::Withheld))
    }

    async fn user_cash(&mut self, user: UserId) -> Result<i64, StoreError> {
        Ok(balance_of(&self.state, FakeOwner::User(user.0)).max(0))
    }

    async fn user_money_view(&mut self, user: UserId) -> Result<UserMoneyView, StoreError> {
        let rec = self
            .state
            .users
            .get(&user.0)
            .ok_or(StoreError::NotFound("user"))?;
        Ok(UserMoneyView {
            status: rec.status,
            kyc_tier: rec.kyc_tier,
            self_excluded_until: rec.self_excluded_until,
        })
    }

    async fn limits(&mut self) -> Result<WithdrawLimits, StoreError> {
        Ok(self.state.limits)
    }

    async fn persist_screening(
        &mut self,
        user: UserId,
        context: &str,
        verdict: &ScreenVerdict,
    ) -> Result<(), StoreError> {
        self.state.screenings.push(ScreeningFact {
            user: user.0,
            context: context.to_string(),
            verdict: verdict.clone(),
        });
        Ok(())
    }

    async fn latest_screening(
        &mut self,
        user: UserId,
        context: &str,
    ) -> Result<Option<ScreenVerdict>, StoreError> {
        Ok(self
            .state
            .screenings
            .iter()
            .rev()
            .find(|fact| fact.user == user.0 && fact.context == context)
            .map(|fact| fact.verdict.clone()))
    }

    async fn open_aml_flag_count(&mut self, user: UserId) -> Result<u32, StoreError> {
        if self
            .state
            .screenings
            .iter()
            .rev()
            .find(|fact| fact.user == user.0 && fact.context == "withdraw")
            .is_some_and(|fact| matches!(fact.verdict, ScreenVerdict::Hit))
        {
            self.state.aml_open.insert(user.0);
        }
        Ok(u32::from(self.state.aml_open.contains(&user.0)))
    }

    async fn evaluate_withdraw_aml_candidate(
        &mut self,
        id: WithdrawalId,
        user: UserId,
        dest: &str,
        amount_micro: i64,
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let policy = crate::money::AmlPolicy::seed();
        let since = at - policy.window;
        let existing = self
            .state
            .withdrawals
            .values()
            .filter(|row| {
                row.requested_at >= since && crate::ports::counts_in_daily_window(row.combo)
            })
            .map(|row| crate::money::AmlLeg {
                id: row.id.0,
                user: row.user,
                dest: row.dest.clone(),
                amount_micro: row.amount_micro,
                at: row.requested_at,
                direction: crate::money::AmlDirection::Withdrawal,
            })
            .collect::<Vec<_>>();
        let candidate = crate::money::AmlLeg {
            id: id.0,
            user,
            dest: dest.to_string(),
            amount_micro,
            at,
            direction: crate::money::AmlDirection::Withdrawal,
        };
        if !crate::money::evaluate_aml(&existing, &candidate, &policy)
            .flags
            .is_empty()
        {
            self.state.aml_open.insert(user.0);
        }
        Ok(())
    }

    async fn persist_intent(&mut self, receipt: &WithdrawalReceipt) -> Result<(), StoreError> {
        let dest = receipt.dest.clone();
        let fingerprint =
            crate::ports::intent_fingerprint(receipt.user, receipt.amount_micro, &dest);
        self.state.fingerprints.insert(fingerprint, receipt.clone());
        Ok(())
    }

    async fn lookup_fingerprint_tx(
        &mut self,
        fingerprint: &str,
    ) -> Result<Option<WithdrawalReceipt>, StoreError> {
        Ok(self.state.fingerprints.get(fingerprint).cloned())
    }

    async fn lookup_idempotency(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.state.idempotency.get(key).cloned())
    }

    async fn persist_idempotency(
        &mut self,
        key: &str,
        fingerprint: &str,
    ) -> Result<(), StoreError> {
        self.state
            .idempotency
            .insert(key.to_string(), fingerprint.to_string());
        Ok(())
    }

    async fn insert_withdrawal(&mut self, row: &WithdrawalRow) -> Result<(), StoreError> {
        self.state.withdrawals.insert(row.id.0, row.clone());
        Ok(())
    }

    async fn withdrawal_for_update(
        &mut self,
        id: WithdrawalId,
    ) -> Result<WithdrawalRow, StoreError> {
        let row = self
            .state
            .withdrawals
            .get(&id.0)
            .cloned()
            .ok_or(StoreError::NotFound("withdrawal"))?;
        if self.state.strict_withdraw_lock_order && !self.locked_users.contains(&row.user.0) {
            return Err(StoreError::Invariant(
                "withdrawal row lock acquired before user lock",
            ));
        }
        Ok(row)
    }

    async fn cas_withdrawal(
        &mut self,
        id: WithdrawalId,
        expected: Combo,
        next: &WithdrawalRow,
    ) -> Result<bool, StoreError> {
        if self.state.faults.force_cas_miss {
            self.state.faults.force_cas_miss = false;
            return Ok(false);
        }
        let Some(current) = self.state.withdrawals.get(&id.0) else {
            return Err(StoreError::NotFound("withdrawal"));
        };
        if current.combo != expected {
            return Ok(false);
        }
        if !next.combo.is_legal() {
            return Err(StoreError::Invariant("illegal withdrawal combination"));
        }
        self.state.withdrawals.insert(id.0, next.clone());
        Ok(true)
    }

    async fn append_withdrawal_event(
        &mut self,
        withdrawal: WithdrawalId,
        kind: &str,
        actor: &str,
        payload: serde_json::Value,
    ) -> Result<(), StoreError> {
        if kind == "finance-approve" {
            if let Some(amount) = payload
                .get("amount_micro")
                .and_then(serde_json::Value::as_i64)
            {
                self.state.finance_approves.push((self.state.now, amount));
            }
        }
        self.state
            .events
            .push((withdrawal, kind.to_string(), actor.to_string(), payload));
        Ok(())
    }

    async fn list_withdrawals(&mut self) -> Result<Vec<WithdrawalRow>, StoreError> {
        Ok(self.state.withdrawals.values().cloned().collect())
    }

    async fn window_sum_user(
        &mut self,
        user: UserId,
        since: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        Ok(self
            .state
            .withdrawals
            .values()
            .filter(|row| {
                row.user == user
                    && row.requested_at >= since
                    && crate::ports::counts_in_daily_window(row.combo)
            })
            .map(|row| row.amount_micro)
            .sum())
    }

    async fn window_sum_dest(
        &mut self,
        dest: &str,
        since: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        Ok(self
            .state
            .withdrawals
            .values()
            .filter(|row| {
                row.dest == dest
                    && row.requested_at >= since
                    && crate::ports::counts_in_daily_window(row.combo)
            })
            .map(|row| row.amount_micro)
            .sum())
    }

    async fn window_sum_hot(&mut self, since: OffsetDateTime) -> Result<i64, StoreError> {
        Ok(self
            .state
            .withdrawals
            .values()
            .filter(|row| {
                row.requested_at >= since && crate::ports::counts_in_daily_window(row.combo)
            })
            .map(|row| row.amount_micro)
            .sum())
    }

    async fn dest_warmth(&mut self, dest: &str) -> Result<DestWarmth, StoreError> {
        let limits = self.state.limits;
        let mut settled_micro = 0_i64;
        let mut first_settled_at = None;
        let mut users = HashSet::new();
        for row in self.state.withdrawals.values() {
            if row.dest != dest {
                continue;
            }
            if row.combo.status == WithdrawalStatus::Settled && row.amount_micro >= limits.min_micro
            {
                users.insert(row.user.0);
                settled_micro = settled_micro.saturating_add(row.amount_micro);
                if let Some(at) = row.settled_at {
                    first_settled_at =
                        Some(first_settled_at.map_or(at, |prev: OffsetDateTime| prev.min(at)));
                }
            }
        }
        Ok(DestWarmth {
            settled_micro,
            first_settled_at,
            distinct_users: self
                .state
                .dest_distinct_users
                .get(dest)
                .copied()
                .unwrap_or_else(|| u32::try_from(users.len()).unwrap_or(u32::MAX)),
            is_refund_dest: self.state.refund_dests.contains(dest),
        })
    }

    async fn dest_was_settled_for_user(
        &mut self,
        user: UserId,
        dest: &str,
    ) -> Result<bool, StoreError> {
        Ok(self.state.withdrawals.values().any(|row| {
            row.user == user && row.dest == dest && row.combo.status == WithdrawalStatus::Settled
        }))
    }

    async fn dest_is_observation_source_for_user(
        &mut self,
        user: UserId,
        dest: &str,
    ) -> Result<bool, StoreError> {
        Ok(self
            .state
            .observation_sources
            .contains(&(user.0, dest.to_string())))
    }

    async fn apply_hold(
        &mut self,
        user: UserId,
        amount_micro: i64,
        key: &str,
    ) -> Result<Uuid, StoreError> {
        if !self.locked_users.contains(&user.0) {
            return Err(StoreError::Invariant("withdrawal user lock required"));
        }
        let user_acct = open_owner(&mut self.state, FakeOwner::User(user.0), Currency::Usdc);
        let withheld = open_owner(&mut self.state, FakeOwner::Withheld, Currency::Usdc);
        apply_txn(
            &mut self.state,
            TxnKind::Withdrawal,
            key,
            &[
                Entry {
                    account: user_acct,
                    amount: MicroUsd(-amount_micro),
                },
                Entry {
                    account: withheld,
                    amount: MicroUsd(amount_micro),
                },
            ],
        )
    }

    async fn apply_release(
        &mut self,
        user: UserId,
        amount_micro: i64,
        key: &str,
    ) -> Result<Uuid, StoreError> {
        if !self.locked_users.contains(&user.0) {
            return Err(StoreError::Invariant("withdrawal user lock required"));
        }
        let user_acct = open_owner(&mut self.state, FakeOwner::User(user.0), Currency::Usdc);
        let withheld = open_owner(&mut self.state, FakeOwner::Withheld, Currency::Usdc);
        apply_txn(
            &mut self.state,
            TxnKind::Reversal,
            key,
            &[
                Entry {
                    account: withheld,
                    amount: MicroUsd(-amount_micro),
                },
                Entry {
                    account: user_acct,
                    amount: MicroUsd(amount_micro),
                },
            ],
        )
    }

    async fn apply_settle(&mut self, amount_micro: i64, key: &str) -> Result<Uuid, StoreError> {
        if self.state.faults.fail_settle {
            self.state.faults.fail_settle = false;
            return Err(StoreError::Backend("forced settle failure".into()));
        }
        let withheld = open_owner(&mut self.state, FakeOwner::Withheld, Currency::Usdc);
        let external = open_owner(&mut self.state, FakeOwner::External, Currency::Usdc);
        apply_txn(
            &mut self.state,
            TxnKind::Withdrawal,
            key,
            &[
                Entry {
                    account: withheld,
                    amount: MicroUsd(-amount_micro),
                },
                Entry {
                    account: external,
                    amount: MicroUsd(amount_micro),
                },
            ],
        )
    }

    async fn open_receivables(&mut self, user: UserId) -> Result<Vec<OpenReceivable>, StoreError> {
        Ok(self
            .state
            .receivables
            .iter()
            .filter(|row| row.user == user.0 && row.outstanding > 0)
            .map(|row| OpenReceivable {
                id: row.id,
                outstanding_micro: row.outstanding,
            })
            .collect())
    }

    async fn insert_proposal(&mut self, proposal: &MoneyProposal) -> Result<(), StoreError> {
        self.state.proposals.insert(proposal.id, proposal.clone());
        Ok(())
    }

    async fn proposal_by_replay(&mut self, key: &str) -> Result<Option<MoneyProposal>, StoreError> {
        Ok(self
            .state
            .proposals
            .values()
            .find(|proposal| proposal.replay_key == key)
            .cloned())
    }

    async fn open_proposal_for(
        &mut self,
        subject: Uuid,
        kind: &str,
    ) -> Result<Option<MoneyProposal>, StoreError> {
        Ok(self
            .state
            .proposals
            .values()
            .find(|proposal| {
                proposal.subject_id == subject
                    && proposal.kind == kind
                    && proposal.status == crate::model::ProposalStatus::Pending
            })
            .cloned())
    }

    async fn save_proposal(&mut self, proposal: &MoneyProposal) -> Result<(), StoreError> {
        self.state.proposals.insert(proposal.id, proposal.clone());
        Ok(())
    }

    async fn finance_approve_sum_since(
        &mut self,
        since: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        Ok(self
            .state
            .finance_approves
            .iter()
            .filter(|(at, _)| *at >= since)
            .map(|(_, amount)| *amount)
            .sum())
    }

    async fn record_finance_approve(&mut self, amount_micro: i64) -> Result<(), StoreError> {
        let now = self.state.now;
        self.state.finance_approves.push((now, amount_micro));
        Ok(())
    }

    async fn record_event(&mut self, event: Event) -> Result<i64, StoreError> {
        self.state.outbox.push(event);
        Ok(i64::try_from(self.state.outbox.len()).unwrap_or(i64::MAX))
    }

    async fn insert_notification(
        &mut self,
        notification: NewNotification,
    ) -> Result<(), StoreError> {
        self.state.notifications.push(notification);
        Ok(())
    }

    async fn lock_cap(&mut self, _name: &str) -> Result<(), StoreError> {
        Ok(())
    }
}

#[async_trait]
impl OutboundIo for FakeWithdrawTx {
    async fn insert_outbound_payment(
        &mut self,
        payment: &OutboundPaymentRow,
    ) -> Result<(), StoreError> {
        if self
            .state
            .payments
            .values()
            .any(|row| row.subject == payment.subject && row.subject_id == payment.subject_id)
        {
            return Err(StoreError::Conflict("outbound payment"));
        }
        self.state.payments.insert(payment.id, payment.clone());
        Ok(())
    }

    async fn outbound_by_subject(
        &mut self,
        subject: OutboundSubject,
        subject_id: Uuid,
    ) -> Result<Option<OutboundPaymentRow>, StoreError> {
        Ok(self
            .state
            .payments
            .values()
            .find(|row| row.subject == subject && row.subject_id == subject_id)
            .cloned())
    }

    async fn insert_attempt(&mut self, attempt: &OutboundAttemptRow) -> Result<(), StoreError> {
        let live = self
            .state
            .attempts
            .values()
            .any(|row| row.payment_id == attempt.payment_id && row.landing_state.is_live());
        let finalized = self.state.attempts.values().any(|row| {
            row.payment_id == attempt.payment_id && row.landing_state == LandingState::Finalized
        });
        let duplicate_number = self.state.attempts.values().any(|row| {
            row.payment_id == attempt.payment_id && row.attempt_number == attempt.attempt_number
        });
        if (attempt.landing_state.is_live() && live)
            || (attempt.landing_state == LandingState::Finalized && finalized)
            || duplicate_number
        {
            return Err(StoreError::Conflict("live outbound attempt"));
        }
        self.state.attempts.insert(attempt.id, attempt.clone());
        Ok(())
    }

    async fn live_attempt(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Option<OutboundAttemptRow>, StoreError> {
        Ok(self
            .state
            .attempts
            .values()
            .find(|row| row.payment_id == payment_id && row.landing_state.is_live())
            .cloned())
    }

    async fn save_attempt(&mut self, attempt: &OutboundAttemptRow) -> Result<(), StoreError> {
        if !self.state.attempts.contains_key(&attempt.id) {
            return Err(StoreError::NotFound("outbound attempt"));
        }
        let conflicts = self.state.attempts.values().any(|row| {
            row.id != attempt.id
                && row.payment_id == attempt.payment_id
                && ((row.landing_state.is_live() && attempt.landing_state.is_live())
                    || (row.landing_state == LandingState::Finalized
                        && attempt.landing_state == LandingState::Finalized))
        });
        if conflicts {
            return Err(StoreError::Conflict("outbound attempt state"));
        }
        self.state.attempts.insert(attempt.id, attempt.clone());
        Ok(())
    }

    async fn attempts_for(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Vec<OutboundAttemptRow>, StoreError> {
        let mut rows: Vec<_> = self
            .state
            .attempts
            .values()
            .filter(|row| row.payment_id == payment_id)
            .cloned()
            .collect();
        rows.sort_by_key(|row| row.attempt_number);
        Ok(rows)
    }
}

// The core in-memory store carries no withdrawal authority: HTTP-router
// tests that never touch money satisfy the `WithdrawStore` bound through this
// fail-closed impl, while real withdrawal tests use `FakeWithdrawStore`.
#[async_trait]
impl WithdrawStore for crate::fakes::InMemoryStore {
    async fn withdraw_tx(&self) -> Result<Box<dyn crate::ports::WithdrawTx + '_>, StoreError> {
        Err(StoreError::Unavailable("phase7:withdraw"))
    }
    async fn withdrawal_user(&self, _id: WithdrawalId) -> Result<UserId, StoreError> {
        Err(StoreError::Unavailable("phase7:withdraw"))
    }
    async fn lookup_fingerprint(
        &self,
        _fingerprint: &str,
    ) -> Result<Option<WithdrawalReceipt>, StoreError> {
        Err(StoreError::Unavailable("phase7:withdraw"))
    }
}

#[async_trait]
impl WithdrawStore for FakeWithdrawStore {
    async fn withdraw_tx(&self) -> Result<Box<dyn WithdrawTx + '_>, StoreError> {
        let snapshot = self.state.lock().clone();
        Ok(Box::new(FakeWithdrawTx {
            shared: Arc::clone(&self.state),
            state: snapshot,
            committed: false,
            locked_users: HashSet::new(),
        }))
    }

    async fn withdrawal_user(&self, id: WithdrawalId) -> Result<UserId, StoreError> {
        if let Some(user) = self.state.lock().withdrawal_user_overrides.get(&id.0) {
            return Ok(*user);
        }
        self.state
            .lock()
            .withdrawals
            .get(&id.0)
            .map(|row| row.user)
            .ok_or(StoreError::NotFound("withdrawal"))
    }

    async fn lookup_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<WithdrawalReceipt>, StoreError> {
        if self.state.lock().faults.suppress_lock_free_fingerprint {
            return Ok(None);
        }
        Ok(self.state.lock().fingerprints.get(fingerprint).cloned())
    }
}

/// Controllable screening port for request-protocol tests.
#[derive(Debug, Clone)]
pub struct FakeScreen {
    pub verdict: ScreenVerdict,
    pub fail: bool,
}

#[async_trait]
impl crate::ports::GeoResolver for FakeScreen {
    async fn resolve(&self, _ip: std::net::IpAddr) -> Result<ScreenVerdict, StoreError> {
        if self.fail {
            return Err(StoreError::Backend("geo unavailable".into()));
        }
        Ok(self.verdict.clone())
    }
}

#[async_trait]
impl crate::ports::SanctionsScreen for FakeScreen {
    async fn screen(&self, _user: UserId, _context: &str) -> Result<ScreenVerdict, StoreError> {
        if self.fail {
            return Err(StoreError::Backend("sanctions unavailable".into()));
        }
        Ok(self.verdict.clone())
    }
}

/// Controllable rail used by send / reconcile tests.
#[derive(Default)]
pub struct FakeRails {
    pub persisted: Mutex<HashMap<Uuid, (Vec<u8>, String)>>,
    pub broadcast: Mutex<HashSet<Uuid>>,
    pub on_chain: Mutex<HashSet<String>>,
    pub fail_persist: Mutex<bool>,
    pub fail_broadcast: Mutex<bool>,
    pub heights: Mutex<HashMap<String, i64>>,
}

#[async_trait]
impl crate::ports::OutboundRails for FakeRails {
    async fn persist_signed(
        &self,
        payment_id: Uuid,
        bytes: &[u8],
        signature: &str,
    ) -> Result<(), StoreError> {
        if *self.fail_persist.lock() {
            return Err(StoreError::Backend("persist failed".into()));
        }
        self.persisted
            .lock()
            .insert(payment_id, (bytes.to_vec(), signature.to_string()));
        Ok(())
    }

    async fn broadcast(&self, payment_id: Uuid) -> Result<(), StoreError> {
        if *self.fail_broadcast.lock() {
            return Err(StoreError::Backend("broadcast failed".into()));
        }
        self.broadcast.lock().insert(payment_id);
        if let Some((_, signature)) = self.persisted.lock().get(&payment_id) {
            self.on_chain.lock().insert(signature.clone());
        }
        Ok(())
    }
}

impl FakeRails {
    #[must_use]
    pub fn persisted_bytes(&self, payment_id: Uuid) -> Option<Vec<u8>> {
        self.persisted
            .lock()
            .get(&payment_id)
            .map(|(bytes, _)| bytes.clone())
    }

    pub fn mark_on_chain(&self, signature: &str) {
        self.on_chain.lock().insert(signature.to_string());
    }

    pub fn clear_on_chain(&self, signature: &str) {
        self.on_chain.lock().remove(signature);
    }

    #[must_use]
    pub fn signature_on_chain(&self, signature: &str) -> bool {
        self.on_chain.lock().contains(signature)
    }
}

/// Default rail identity for tests.
#[must_use]
pub fn test_rail() -> RailIdentity {
    RailIdentity {
        genesis_hash: "gen".into(),
        rpc_endpoints: vec!["a".into(), "b".into(), "c".into()],
        usdc_mint: "mint".into(),
        decimals: 6,
        treasury_owner: "owner".into(),
        treasury_token_account: "ata".into(),
        commitment: "finalized".into(),
    }
}

/// Pinned 32-byte dest (system program).
#[must_use]
pub fn dest_a() -> String {
    canonicalize_dest("11111111111111111111111111111111").unwrap_or_default()
}

/// Second pinned dest (Token program).
#[must_use]
pub fn dest_b() -> String {
    canonicalize_dest("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::model::ProposalStatus;

    #[tokio::test]
    async fn fake_hold_release_and_settle_conserve() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 10_000_000);
        let mut tx = store.withdraw_tx().await.unwrap();
        tx.lock_user(user).await.unwrap();
        let hold = tx.apply_hold(user, 5_000_000, "hold-1").await.unwrap();
        assert_ne!(hold, Uuid::nil());
        assert_eq!(tx.user_cash(user).await.unwrap(), 5_000_000);
        assert_eq!(tx.withheld_balance().await.unwrap(), 5_000_000);
        tx.apply_release(user, 5_000_000, "rel-1").await.unwrap();
        assert_eq!(tx.withheld_balance().await.unwrap(), 0);
        tx.apply_hold(user, 5_000_000, "hold-2").await.unwrap();
        tx.apply_settle(5_000_000, "settle-1").await.unwrap();
        assert_eq!(tx.withheld_balance().await.unwrap(), 0);
        tx.commit().await.unwrap();
        assert_eq!(store.withheld(), 0);
    }

    #[tokio::test]
    async fn fake_uncommitted_hold_rolls_back() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 10_000_000);
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            tx.lock_user(user).await.unwrap();
            tx.apply_hold(user, 5_000_000, "hold-x").await.unwrap();
        }
        assert_eq!(store.withheld(), 0);
    }

    #[tokio::test]
    async fn fake_transaction_faults_fail_closed() {
        let lock_store = FakeWithdrawStore::new();
        let user = lock_store.seed_user(UserStatus::Active, 2);
        lock_store.fail_lock_user();
        let mut lock_tx = lock_store.withdraw_tx().await.unwrap();
        assert!(lock_tx.lock_user(user).await.is_err());

        let commit_store = FakeWithdrawStore::new();
        commit_store.fail_commit();
        let commit_tx = commit_store.withdraw_tx().await.unwrap();
        assert!(commit_tx.commit().await.is_err());
    }

    #[tokio::test]
    async fn core_in_memory_store_has_no_withdrawal_authority() {
        let store = crate::fakes::InMemoryStore::new();
        assert!(matches!(
            WithdrawStore::withdraw_tx(&store).await,
            Err(StoreError::Unavailable("phase7:withdraw"))
        ));
        assert!(matches!(
            WithdrawStore::withdrawal_user(&store, WithdrawalId(Uuid::new_v4())).await,
            Err(StoreError::Unavailable("phase7:withdraw"))
        ));
        assert!(matches!(
            WithdrawStore::lookup_fingerprint(&store, "request-fingerprint").await,
            Err(StoreError::Unavailable("phase7:withdraw"))
        ));
    }

    #[tokio::test]
    async fn fake_release_requires_the_matching_user_lock() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 10_000_000);
        let mut hold = store.withdraw_tx().await.unwrap();
        hold.lock_user(user).await.unwrap();
        hold.apply_hold(user, 5_000_000, "locked-hold")
            .await
            .unwrap();
        hold.commit().await.unwrap();

        let mut release = store.withdraw_tx().await.unwrap();
        assert!(release
            .apply_release(user, 5_000_000, "unlocked-release")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fake_proposal_and_payment_uniqueness() {
        let store = FakeWithdrawStore::new();
        let mut tx = store.withdraw_tx().await.unwrap();
        let proposal = MoneyProposal {
            id: Uuid::new_v4(),
            kind: "approve_withdrawal".into(),
            subject_id: Uuid::new_v4(),
            payload_hash: "h".into(),
            proposer_token_id: "a".into(),
            confirmer_token_id: None,
            reason: "r".into(),
            status: ProposalStatus::Pending,
            confirm_not_before: store.now(),
            expires_at: store.now() + time::Duration::minutes(15),
            replay_key: "rk".into(),
        };
        tx.insert_proposal(&proposal).await.unwrap();
        let unrelated = MoneyProposal {
            id: Uuid::new_v4(),
            kind: "frozen_funds_license".into(),
            replay_key: "rk-other".into(),
            ..proposal.clone()
        };
        tx.insert_proposal(&unrelated).await.unwrap();
        assert!(tx.proposal_by_replay("rk").await.unwrap().is_some());
        assert_eq!(
            tx.open_proposal_for(proposal.subject_id, "approve_withdrawal")
                .await
                .unwrap()
                .unwrap()
                .id,
            proposal.id
        );
        let payment = OutboundPaymentRow {
            id: Uuid::new_v4(),
            subject: OutboundSubject::Withdrawal,
            subject_id: proposal.subject_id,
            dest: dest_a(),
            amount_micro: 5,
            rail_fingerprint: "rf".into(),
        };
        tx.insert_outbound_payment(&payment).await.unwrap();
        let dup = OutboundPaymentRow {
            id: Uuid::new_v4(),
            ..payment.clone()
        };
        assert!(matches!(
            tx.insert_outbound_payment(&dup).await,
            Err(StoreError::Conflict(_))
        ));
        let attempt = OutboundAttemptRow {
            id: Uuid::new_v4(),
            payment_id: payment.id,
            attempt_number: 1,
            replaces_attempt_id: None,
            signed_tx_bytes: vec![1, 2, 3],
            signature: "sig".into(),
            last_valid_block_height: 9,
            landing_state: LandingState::Prepared,
            lease_expires_at: None,
            evidence: None,
        };
        tx.insert_attempt(&attempt).await.unwrap();
        let live2 = OutboundAttemptRow {
            id: Uuid::new_v4(),
            attempt_number: 2,
            ..attempt.clone()
        };
        assert!(matches!(
            tx.insert_attempt(&live2).await,
            Err(StoreError::Conflict(_))
        ));

        let mut finalized = attempt;
        finalized.landing_state = LandingState::Finalized;
        // Move the original live row out of the live set first.
        tx.save_attempt(&finalized).await.unwrap();
        let duplicate_final = OutboundAttemptRow {
            id: Uuid::new_v4(),
            attempt_number: 2,
            signature: "final-2".into(),
            ..finalized
        };
        assert!(tx.insert_attempt(&duplicate_final).await.is_err());
        tx.insert_attempt(&live2).await.unwrap();
        let mut live2_final = live2;
        live2_final.landing_state = LandingState::Finalized;
        assert!(matches!(
            tx.save_attempt(&live2_final).await,
            Err(StoreError::Conflict("outbound attempt state"))
        ));
    }

    #[test]
    fn dest_fixtures_are_32_byte_pubkeys() {
        assert_eq!(dest_a().len(), 32);
        assert!(!dest_b().is_empty());
        assert_ne!(dest_a(), dest_b());
    }

    #[tokio::test]
    async fn fake_control_and_corruption_edges_are_observable() {
        assert_eq!(balance_of(&FakeState::default(), FakeOwner::Withheld), 0);

        let store = FakeWithdrawStore::new();
        assert!(!store.corrupt_outbound_dest(Uuid::new_v4(), &dest_a()));
        let user = store.seed_user(UserStatus::Active, 2);
        store.set_status(user, UserStatus::ShadowLimited);
        store.set_kyc(user, 3);
        store.set_self_excluded(user, store.now() + time::Duration::hours(1));
        store.open_aml(user);
        store.clear_aml(user);
        let missing = UserId(Uuid::new_v4());
        store.set_status(missing, UserStatus::Banned);
        store.set_kyc(missing, 0);
        store.set_self_excluded(missing, store.now());
        store.set_now(store.now() + time::Duration::minutes(1));
        store.credit(user, 5_000_000);
        store.credit(user, 5_000_000);

        let mut tx = store.withdraw_tx().await.unwrap();
        tx.append(Event {
            event_type: "one",
            aggregate_type: "withdrawal",
            aggregate_id: Uuid::new_v4(),
            payload: serde_json::Value::Null,
        })
        .await
        .unwrap();
        tx.append_batch(&[Event {
            event_type: "two",
            aggregate_type: "withdrawal",
            aggregate_id: Uuid::new_v4(),
            payload: serde_json::Value::Null,
        }])
        .await
        .unwrap();
        tx.record_finance_approve(1).await.unwrap();
        assert!(!tx.dest_was_settled_for_user(user, &dest_a()).await.unwrap());

        tx.lock_user(user).await.unwrap();
        let hold = tx.apply_hold(user, 5_000_000, "edge-hold").await.unwrap();
        let settle = tx.apply_settle(5_000_000, "edge-settle").await.unwrap();
        let row = WithdrawalRow {
            id: WithdrawalId(Uuid::new_v4()),
            user,
            dest: dest_a(),
            amount_micro: 5_000_000,
            combo: Combo::W10,
            hold_tx_id: hold,
            release_tx_id: None,
            settle_tx_id: Some(settle),
            request_fingerprint: "edge".into(),
            risk_reasons: Vec::new(),
            requested_at: store.now(),
            decided_at: Some(store.now()),
            sent_at: Some(store.now()),
            settled_at: Some(store.now()),
        };
        tx.insert_withdrawal(&row).await.unwrap();
        let earlier = WithdrawalRow {
            id: WithdrawalId(Uuid::new_v4()),
            requested_at: store.now() - time::Duration::hours(2),
            settled_at: Some(store.now() - time::Duration::hours(2)),
            ..row.clone()
        };
        tx.insert_withdrawal(&earlier).await.unwrap();
        assert_eq!(
            tx.dest_warmth(&dest_a()).await.unwrap().first_settled_at,
            earlier.settled_at
        );
        assert!(tx.dest_was_settled_for_user(user, &dest_a()).await.unwrap());
        tx.commit().await.unwrap();

        let rails = FakeRails::default();
        rails.mark_on_chain("edge-sig");
        assert!(rails.signature_on_chain("edge-sig"));
        rails.clear_on_chain("edge-sig");
        assert!(!rails.signature_on_chain("edge-sig"));
    }
}
