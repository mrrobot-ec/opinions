/// Get-or-create against committed state (see module docs for the rollback
/// caveat).
fn account_in(
    state: &mut State,
    owner: OwnerRef,
    currency: Currency,
) -> Result<AccountId, StoreError> {
    if let Some(id) = state.accounts.get(&(owner, currency)) {
        return Ok(*id);
    }
    let id = AccountId(Uuid::new_v4());
    state
        .balances
        .open(id, owner_type(owner), currency)
        .map_err(StoreError::Ledger)?;
    state.accounts.insert((owner, currency), id);
    Ok(id)
}

impl InMemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a complete user/reputation fixture for application tests.
    #[must_use]
    pub fn add_user(&self, handle: &str, created_at: OffsetDateTime, tier: u8) -> UserId {
        let user = UserId(Uuid::new_v4());
        let mut state = self.shared.state.lock();
        state.users.insert(user.0, handle.to_string());
        state.user_created_at.insert(user.0, created_at);
        state.reputation.insert(user.0, (0, tier));
        user
    }

    fn tx(&self) -> InMemTx {
        InMemTx {
            shared: Arc::clone(&self.shared),
            key_guards: HashMap::new(),
            market_guards: HashMap::new(),
            pool_guards: HashMap::new(),
            position_guards: HashMap::new(),
            account_guards: BTreeMap::new(),
            seq_guards: HashMap::new(),
            user_guards: HashMap::new(),
            comment_guards: HashMap::new(),
            notifier_guard: None,
            lp_kill_guard: None,
            pending: Pending::default(),
        }
    }

    // ---- test-support fixtures (not ports; adapters seed via SQL) ----

    /// Creates a market with its pool reserves and the accounts the trade
    /// path touches (external, fees, escrow), so failure-path tests observe
    /// zero state drift.
    ///
    /// # Errors
    /// [`StoreError::Ledger`] / [`StoreError::Backend`] for invalid fixtures.
    pub fn add_market(
        &self,
        slug: &str,
        state: MarketState,
        closes_at: OffsetDateTime,
        tally_hidden_at: OffsetDateTime,
        reserves: MicroShares,
        fee: BasisPoints,
    ) -> Result<MarketRow, StoreError> {
        let pool =
            Pool::new(reserves, reserves, fee).map_err(|e| StoreError::Backend(e.to_string()))?;
        let row = MarketRow {
            id: MarketId(Uuid::new_v4()),
            slug: slug.to_string(),
            question: slug.to_string(),
            state,
            min_votes_to_resolve: 3,
            opens_at: OffsetDateTime::UNIX_EPOCH,
            closes_at,
            tally_hidden_at,
            yes_outcome: OutcomeId(Uuid::new_v4()),
            no_outcome: OutcomeId(Uuid::new_v4()),
            curator_flagged_at: None,
            integrity_due_at: None,
            poster_asset_url: None,
            video_asset_url: None,
        };
        let mut st = self.shared.state.lock();
        account_in(&mut st, OwnerRef::External, Currency::Usdc)?;
        account_in(&mut st, OwnerRef::Fees, Currency::Usdc)?;
        account_in(&mut st, OwnerRef::MarketEscrow(row.id), Currency::Usdc)?;
        account_in(&mut st, OwnerRef::MarketPool(row.id), Currency::Usdc)?;
        st.markets.insert(row.id.0, row.clone());
        st.pools.insert(row.id.0, pool);
        Ok(row)
    }

    /// Deposits `amount` into the user's USDC account from External, through
    /// the domain ledger (balanced, validated).
    ///
    /// # Errors
    /// [`StoreError::Ledger`] when the deposit is rejected by the domain.
    pub fn fund_user(&self, u: UserId, amount: MicroUsd) -> Result<(), StoreError> {
        let mut st = self.shared.state.lock();
        st.users
            .entry(u.0)
            .or_insert_with(|| format!("user-{}", &u.0.to_string()[..8]));
        st.user_created_at
            .entry(u.0)
            .or_insert_with(OffsetDateTime::now_utc);
        st.reputation.entry(u.0).or_insert((0, 0));
        let ext = account_in(&mut st, OwnerRef::External, Currency::Usdc)?;
        let user = account_in(&mut st, OwnerRef::User(u), Currency::Usdc)?;
        let txn = Transaction::new(
            TxnKind::Deposit,
            vec![
                Entry {
                    account: ext,
                    amount: MicroUsd(-amount.0),
                },
                Entry {
                    account: user,
                    amount,
                },
            ],
        )
        .map_err(StoreError::Ledger)?;
        st.balances.apply(&txn).map_err(StoreError::Ledger)?;
        Ok(())
    }

    /// Records a vote fixture (the D4 vote-gate reads it; Task 1.2 owns the
    /// real `CastVote` write path).
    pub fn record_vote(&self, u: UserId, m: MarketId, side: Side) {
        let mut st = self.shared.state.lock();
        if st.votes.insert((u.0, m.0)) {
            let tally = st.tallies.entry(m.0).or_insert(Tally {
                yes_votes: 0,
                no_votes: 0,
            });
            match side {
                Side::Yes => tally.yes_votes += 1,
                Side::No => tally.no_votes += 1,
            }
        }
    }

    #[cfg(test)]
    pub fn record_integrity_vote(
        &self,
        u: UserId,
        m: MarketId,
        side: Side,
        created_at: OffsetDateTime,
        cast_ip: Option<std::net::IpAddr>,
        device_hash: Option<String>,
    ) {
        let mut st = self.shared.state.lock();
        st.users
            .entry(u.0)
            .or_insert_with(|| format!("user-{}", &u.0.to_string()[..8]));
        st.user_created_at.insert(u.0, created_at);
        st.reputation.entry(u.0).or_insert((0, 0));
        st.votes.insert((u.0, m.0));
        let tally = st.tallies.entry(m.0).or_insert(Tally {
            yes_votes: 0,
            no_votes: 0,
        });
        match side {
            Side::Yes => tally.yes_votes += 1,
            Side::No => tally.no_votes += 1,
        }
        let id = VoteId(Uuid::new_v4());
        let seq = i64::try_from(st.vote_rows.len())
            .unwrap_or(i64::MAX)
            .saturating_add(1);
        st.vote_rows.insert(
            id.0,
            StoredVote {
                id,
                market: m,
                user: u,
                side,
                crowd_guess_pct: 50,
                seq,
                idempotency_key: format!("integrity-fixture-{}", id.0),
                score: None,
                score_created_at: None,
                created_at,
                cast_ip,
                device_hash,
            },
        );
    }

    /// Overwrites a market's lifecycle state (fixture for freeze/closed
    /// rejection tests; the real transition path is Task 1.2's).
    pub fn set_market_state(&self, m: MarketId, state: MarketState) {
        let mut st = self.shared.state.lock();
        let _ = st.markets.get_mut(&m.0).map(|row| row.state = state);
    }

    /// Links a messaging channel address to a user (fixture for
    /// [`MarketQueries::user_by_channel`]).
    pub fn link_user_channel(&self, channel: &str, address: &str, u: UserId) {
        let mut st = self.shared.state.lock();
        st.users
            .entry(u.0)
            .or_insert_with(|| format!("user-{}", &u.0.to_string()[..8]));
        st.user_created_at
            .entry(u.0)
            .or_insert(OffsetDateTime::UNIX_EPOCH);
        st.reputation.entry(u.0).or_insert((0, 0));
        st.channels
            .insert((channel.to_string(), address.to_string()), u.0);
    }

    pub fn set_user_created_at(&self, user: UserId, created_at: OffsetDateTime) {
        self.shared
            .state
            .lock()
            .user_created_at
            .insert(user.0, created_at);
    }

    pub fn set_user_rep(&self, user: UserId, rep_micro: i64, tier: u8) {
        self.shared
            .state
            .lock()
            .reputation
            .insert(user.0, (rep_micro, tier));
    }

    pub fn set_vote_guess(&self, user: UserId, market: MarketId, guess: u8) {
        let _ = self
            .shared
            .state
            .lock()
            .vote_rows
            .values_mut()
            .find(|vote| vote.user == user && vote.market == market)
            .map(|vote| vote.crowd_guess_pct = guess);
    }

    #[must_use]
    pub fn vote_metadata_for_user(
        &self,
        user: UserId,
        market: MarketId,
    ) -> Option<(Option<std::net::IpAddr>, Option<String>)> {
        self.shared
            .state
            .lock()
            .vote_rows
            .values()
            .find(|vote| vote.user == user && vote.market == market)
            .map(|vote| (vote.cast_ip, vote.device_hash.clone()))
    }

    /// Test-only fault injection for the scheduler's sweep-level query path.
    pub fn set_due_query_failure(&self, fail: bool) {
        self.shared.state.lock().due_query_fails = fail;
    }

    /// Balance of the account owned by (`owner`, `currency`), if it exists.
    #[must_use]
    pub fn balance_of(&self, owner: OwnerRef, currency: Currency) -> Option<MicroUsd> {
        let st = self.shared.state.lock();
        let id = st.accounts.get(&(owner, currency))?;
        st.balances.balance(*id)
    }

    /// The committed trade row for `id`, as an insert payload plus receipt.
    #[must_use]
    pub fn stored_trade(&self, id: TradeId) -> Option<NewTrade> {
        let st = self.shared.state.lock();
        st.trades.get(&id.0).map(|t| t.new.clone())
    }

    /// Committed outbox, in append order.
    #[must_use]
    pub fn outbox(&self) -> Vec<Event> {
        self.shared.state.lock().outbox.clone()
    }

    /// Committed lifecycle state of a market, if it exists.
    #[must_use]
    pub fn market_state(&self, m: MarketId) -> Option<MarketState> {
        self.shared.state.lock().markets.get(&m.0).map(|r| r.state)
    }

    /// Committed `(final_bps, redemption_micro)` for an outcome, if resolved.
    #[must_use]
    pub fn outcome_resolution(&self, o: OutcomeId) -> Option<(u16, i64)> {
        self.shared
            .state
            .lock()
            .outcome_resolutions
            .get(&o.0)
            .copied()
    }

    /// Immutable realization facts committed so far, in insertion order.
    #[must_use]
    pub fn realizations(&self) -> Vec<RealizationFact> {
        self.shared.state.lock().realizations.clone()
    }

    /// Committed LP result and settlement timestamp for a market.
    #[must_use]
    pub fn lp_result(&self, market: MarketId) -> Option<(MicroUsd, OffsetDateTime)> {
        self.shared.state.lock().lp_results.get(&market.0).copied()
    }

    /// Committed `collateral_at_close` fact stamped by resolution (D27
    /// identity 5), if the market has settled.
    #[must_use]
    pub fn collateral_at_close(&self, market: MarketId) -> Option<MicroUsd> {
        self.shared
            .state
            .lock()
            .collateral_at_close
            .get(&market.0)
            .copied()
    }

    /// Test fixture for a prior settled market used by the seed-time breaker.
    pub fn record_lp_result(&self, market: MarketId, pnl: MicroUsd, settled_at: OffsetDateTime) {
        self.shared
            .state
            .lock()
            .lp_results
            .insert(market.0, (pnl, settled_at));
    }

    /// Committed score of a vote, if the market resolved with scoring.
    #[must_use]
    pub fn vote_score(&self, vote_id: uuid::Uuid) -> Option<domain::scoring::VoteScore> {
        self.shared
            .state
            .lock()
            .vote_rows
            .get(&vote_id)
            .and_then(|v| v.score)
    }

    #[must_use]
    pub fn vote_score_for_user(
        &self,
        user: UserId,
        market: MarketId,
    ) -> Option<domain::scoring::VoteScore> {
        self.shared
            .state
            .lock()
            .vote_rows
            .values()
            .find(|vote| vote.user == user && vote.market == market)
            .and_then(|vote| vote.score)
    }

    /// Committed handle of a user, if it exists.
    #[must_use]
    pub fn user_handle(&self, u: UserId) -> Option<String> {
        self.shared.state.lock().users.get(&u.0).cloned()
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let st = self.shared.state.lock();
        Snapshot {
            balances: st
                .accounts
                .values()
                .filter_map(|id| {
                    // Zero-balance accounts are omitted: `account()` is
                    // durable get-or-create (like a sequence allocation), and
                    // an empty account is unobservable through the ports —
                    // snapshots must not flag it as drift.
                    let bal = st.balances.balance(*id).map_or(0, |b| b.0);
                    (bal != 0).then_some((id.0, bal))
                })
                .collect(),
            txn_keys: st.txn_keys.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            pools: st
                .pools
                .iter()
                .map(|(m, p)| (*m, (p.yes.0, p.no.0, p.fee.0)))
                .collect(),
            positions: st
                .positions
                .iter()
                .map(|(k, p)| (*k, (p.shares.0, p.cost.0, p.realized_pnl.0)))
                .collect(),
            trades: st
                .trades
                .values()
                .map(|t| (t.id.0, t.new.ledger_txn))
                .collect(),
            markets: st
                .markets
                .iter()
                .map(|(id, row)| (*id, format!("{row:?}")))
                .collect(),
            votes: st.votes.clone(),
            vote_rows: st
                .vote_rows
                .iter()
                .map(|(id, v)| (*id, format!("{v:?}")))
                .collect(),
            vote_seq: st.vote_seq.iter().map(|(m, s)| (*m, *s)).collect(),
            deposits: st
                .deposits
                .iter()
                .map(|(id, d)| (*id, format!("{d:?}")))
                .collect(),
            users: st.users.iter().map(|(id, h)| (*id, h.clone())).collect(),
            channels: st.channels.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            outcome_resolutions: st
                .outcome_resolutions
                .iter()
                .map(|(o, r)| (*o, *r))
                .collect(),
            lp_results: st
                .lp_results
                .iter()
                .map(|(market, (pnl, settled_at))| (*market, (pnl.0, *settled_at)))
                .collect(),
            collateral_at_close: st
                .collateral_at_close
                .iter()
                .map(|(market, collateral)| (*market, collateral.0))
                .collect(),
            realizations: st.realizations.clone(),
            drafts: st.phase5.drafts.clone(),
            video_jobs: st.phase5.video_jobs.clone(),
            moderation_jobs: st.phase5.moderation_jobs.clone(),
            moderation_cursor: st.phase5.moderation_cursor,
            slot_events: st.phase5.slot_events.clone(),
            outbox_len: st.outbox.len(),
        }
    }
}

/// One in-flight unit of work. Guards are released when the tx commits or is
/// dropped (rollback); pending writes apply atomically at commit.
struct InMemTx {
    shared: Arc<Shared>,
    key_guards: HashMap<String, OwnedMutexGuard<()>>,
    market_guards: HashMap<Uuid, OwnedMutexGuard<()>>,
    pool_guards: HashMap<Uuid, OwnedMutexGuard<()>>,
    position_guards: HashMap<(Uuid, Uuid), OwnedMutexGuard<()>>,
    account_guards: BTreeMap<Uuid, OwnedMutexGuard<()>>,
    seq_guards: HashMap<Uuid, OwnedMutexGuard<()>>,
    user_guards: HashMap<Uuid, OwnedMutexGuard<()>>,
    comment_guards: HashMap<Uuid, OwnedMutexGuard<()>>,
    notifier_guard: Option<OwnedMutexGuard<()>>,
    lp_kill_guard: Option<OwnedMutexGuard<()>>,
    pending: Pending,
}

impl InMemTx {
    async fn lock_comment_row(&mut self, id: Uuid) {
        if !self.comment_guards.contains_key(&id) {
            let handle = self.shared.comment_locks.handle(&id);
            let guard = handle.lock_owned().await;
            self.comment_guards.insert(id, guard);
        }
    }

    fn comment_view(&self, state: &State, id: CommentId) -> Option<CommentRow> {
        let mut row = self
            .pending
            .comments
            .iter()
            .find(|row| row.id == id)
            .map(|new| CommentRow {
                id: new.id,
                market: new.market,
                author: new.author,
                parent: new.parent,
                body: new.body.clone(),
                body_hash: Some(new.body_hash.clone()),
                score: 0,
                moderation_status: new.moderation_status,
                depth: new.depth,
                reply_count: 0,
                created_at: new.created_at,
            })
            .or_else(|| state.comments.get(&id.0).cloned())?;
        let pending_bumps = self
            .pending
            .comment_reply_bumps
            .iter()
            .filter(|parent| **parent == id.0)
            .fold(0_u32, |count, _| count.saturating_add(1));
        row.reply_count = row.reply_count.saturating_add(pending_bumps);
        if let Some((_, score)) = self
            .pending
            .comment_score_updates
            .iter()
            .rev()
            .find(|(comment, _)| *comment == id.0)
        {
            row.score = *score;
        }
        if let Some((_, status)) = self
            .pending
            .comment_status_updates
            .iter()
            .rev()
            .find(|(comment, _)| *comment == id.0)
        {
            row.moderation_status = *status;
        }
        Some(row)
    }

    async fn lock_pool_row(&mut self, m: Uuid) {
        if !self.pool_guards.contains_key(&m) {
            let handle = self.shared.pool_locks.handle(&m);
            let guard = handle.lock_owned().await;
            self.pool_guards.insert(m, guard);
        }
    }

    async fn lock_position_row(&mut self, key: (Uuid, Uuid)) {
        if !self.position_guards.contains_key(&key) {
            let handle = self.shared.position_locks.handle(&key);
            let guard = handle.lock_owned().await;
            self.position_guards.insert(key, guard);
        }
    }

    /// Acquires the touched accounts' locks in account-id order (codex B1),
    /// skipping locks this tx already holds.
    async fn lock_accounts(&mut self, entries: &[Entry]) {
        let mut wanted: Vec<Uuid> = entries.iter().map(|e| e.account.0).collect();
        wanted.sort_unstable();
        wanted.dedup();
        for id in wanted {
            if !self.account_guards.contains_key(&id) {
                let handle = self.shared.account_locks.handle(&id);
                let guard = handle.lock_owned().await;
                self.account_guards.insert(id, guard);
            }
        }
    }

    /// Validates `txn` against committed balances plus this tx's buffered
    /// ledger writes — the same read-your-writes a real DB transaction sees.
    fn validate_against_current(&self, txn: &Transaction) -> Result<(), StoreError> {
        let mut probe = self.shared.state.lock().balances.clone();
        for pending in &self.pending.txns {
            probe.apply(&pending.txn).map_err(StoreError::Ledger)?;
        }
        probe.apply(txn).map_err(StoreError::Ledger)?;
        Ok(())
    }

    /// Shared body for the `set_market_state` methods on `SettlementIo` and
    /// `MarketWriter` (same raw write, both guarded by `domain::transition`
    /// in the use cases).
    fn buffer_market_state(&mut self, m: MarketId, s: MarketState) -> Result<(), StoreError> {
        let known_committed = self.shared.state.lock().markets.contains_key(&m.0);
        let known_pending = self.pending.markets.iter().any(|row| row.id == m);
        if !known_committed && !known_pending {
            return Err(StoreError::NotFound("market"));
        }
        self.pending.market_states.push((m.0, s));
        Ok(())
    }

    /// The market row as this tx sees it: pending inserts and state changes
    /// layered over committed state.
    fn market_view(&self, st: &State, m: MarketId) -> Option<MarketRow> {
        let mut row = self
            .pending
            .markets
            .iter()
            .find(|r| r.id == m)
            .cloned()
            .or_else(|| st.markets.get(&m.0).cloned())?;
        if let Some((_, s)) = self
            .pending
            .market_states
            .iter()
            .rev()
            .find(|(id, _)| *id == m.0)
        {
            row.state = *s;
        }
        Some(row)
    }
}

#[async_trait]
impl Committable for InMemTx {
    #[allow(clippy::too_many_lines)]
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        let mut st = self.shared.state.lock();
        // Stage on a clone: commit is all-or-nothing even if re-validation
        // fails (it cannot under the held locks, but the fake keeps the
        // property structural rather than assumed).
        let mut next = st.clone();
        for pending in &self.pending.txns {
            if next.txn_keys.contains_key(&pending.key) {
                return Err(StoreError::DuplicateKey);
            }
            next.txn_keys.insert(pending.key.clone(), pending.id);
            next.balances
                .apply(&pending.txn)
                .map_err(StoreError::Ledger)?;
            next.ledger_txns.insert(pending.id, pending.clone());
        }
        for row in &self.pending.markets {
            if next.markets.contains_key(&row.id.0) {
                return Err(StoreError::Conflict("market id"));
            }
            next.markets.insert(row.id.0, row.clone());
        }
        for (m, pool) in &self.pending.reserves {
            next.pools.insert(*m, *pool);
        }
        for (m, seeded) in &self.pending.pool_seeded {
            next.pool_seeded.insert(*m, *seeded);
        }
        for (m, pnl, settled_at) in &self.pending.lp_results {
            next.lp_results.insert(*m, (*pnl, *settled_at));
        }
        for (m, collateral) in &self.pending.collateral_at_close {
            if !next.markets.contains_key(m) {
                return Err(StoreError::Invariant(
                    "collateral write for unknown market",
                ));
            }
            next.collateral_at_close.insert(*m, *collateral);
        }
        for (m, s) in &self.pending.market_states {
            let row = next
                .markets
                .get_mut(m)
                .ok_or(StoreError::Invariant("state write for unknown market"))?;
            row.state = *s;
        }
        for (market, flagged_at) in &self.pending.curator_flags {
            let row = next
                .markets
                .get_mut(market)
                .ok_or(StoreError::Invariant("curator flag for unknown market"))?;
            row.curator_flagged_at = *flagged_at;
        }
        for (market, due_at) in &self.pending.integrity_due_updates {
            let row = next.markets.get_mut(market).ok_or(StoreError::Invariant(
                "integrity due write for unknown market",
            ))?;
            row.integrity_due_at = *due_at;
        }
        for p in &self.pending.positions {
            next.positions.insert((p.user.0, p.outcome.0), *p);
        }
        for t in &self.pending.trades {
            next.trades_by_txn.insert(t.new.ledger_txn, t.id.0);
            next.trades.insert(t.id.0, t.clone());
        }
        for v in &self.pending.votes {
            // Re-check under the state lock: this mirrors the unique index a
            // racing transaction would hit at commit.
            if !next.votes.insert((v.user.0, v.market.0)) {
                return Err(StoreError::Conflict("vote"));
            }
            let tally = next.tallies.entry(v.market.0).or_insert(Tally {
                yes_votes: 0,
                no_votes: 0,
            });
            match v.side {
                Side::Yes => tally.yes_votes += 1,
                Side::No => tally.no_votes += 1,
            }
            next.vote_rows.insert(v.id.0, v.clone());
        }
        for (m, seq) in &self.pending.vote_seq {
            next.vote_seq.insert(*m, *seq);
        }
        for (vote_id, score, scored_at) in &self.pending.vote_scores {
            let row = next
                .vote_rows
                .get_mut(vote_id)
                .ok_or(StoreError::Invariant("score for unknown vote"))?;
            row.score = Some(*score);
            row.score_created_at = Some(*scored_at);
        }
        for d in &self.pending.deposits {
            if next.deposits_by_sig.contains_key(&d.new.chain_sig) {
                return Err(StoreError::Conflict("deposit chain signature"));
            }
            next.deposits_by_sig.insert(d.new.chain_sig.clone(), d.id.0);
            next.deposits.insert(d.id.0, d.clone());
        }
        for (id, handle) in &self.pending.users {
            next.users.insert(id.0, handle.clone());
        }
        for (id, created_at) in &self.pending.user_created_at {
            next.user_created_at.insert(id.0, *created_at);
        }
        for row in &self.pending.reputation {
            next.reputation
                .insert(row.user.0, (row.rep_micro, row.tier));
        }
        for (key, user) in &self.pending.channels {
            if next.channels.contains_key(key) {
                return Err(StoreError::Conflict("channel link"));
            }
            next.channels.insert(key.clone(), user.0);
        }
        for (outcome, bps, redemption) in &self.pending.outcome_resolutions {
            next.outcome_resolutions
                .insert(*outcome, (*bps, redemption.0));
        }
        for (key, command) in &self.pending.lifecycle_commands {
            if next.lifecycle_commands.contains_key(key) {
                return Err(StoreError::DuplicateKey);
            }
            next.lifecycle_commands.insert(key.clone(), *command);
        }
        for report in &self.pending.integrity_reports {
            next.integrity_reports
                .entry(report.market.0)
                .or_insert_with(|| report.clone());
        }
        for fact in &self.pending.realizations {
            let duplicate = next.realizations.iter().any(|row| {
                row.ledger_txn == fact.ledger_txn
                    && row.user == fact.user
                    && row.outcome == fact.outcome
            });
            if !duplicate {
                next.realizations.push(*fact);
            }
        }
        for comment in &self.pending.comments {
            if next.comments.contains_key(&comment.id.0) {
                return Err(StoreError::Conflict("comment"));
            }
            next.comments.insert(
                comment.id.0,
                CommentRow {
                    id: comment.id,
                    market: comment.market,
                    author: comment.author,
                    parent: comment.parent,
                    body: comment.body.clone(),
                    body_hash: Some(comment.body_hash.clone()),
                    score: 0,
                    moderation_status: comment.moderation_status,
                    depth: comment.depth,
                    reply_count: 0,
                    created_at: comment.created_at,
                },
            );
        }
        for parent in &self.pending.comment_reply_bumps {
            let row = next
                .comments
                .get_mut(parent)
                .ok_or(StoreError::NotFound("comment"))?;
            row.reply_count = row
                .reply_count
                .checked_add(1)
                .ok_or(StoreError::Invariant("comment reply count overflow"))?;
        }
        for (comment, user, _) in &self.pending.comment_votes {
            next.comment_votes.insert((*comment, *user));
        }
        for (comment, score) in &self.pending.comment_score_updates {
            next.comments
                .get_mut(comment)
                .ok_or(StoreError::NotFound("comment"))?
                .score = *score;
        }
        for comment in &self.pending.comment_report_resets {
            next.comment_reports.retain(|(id, _), _| id != comment);
        }
        for (comment, reporter, created_at) in &self.pending.comment_reports {
            next.comment_reports
                .insert((*comment, *reporter), *created_at);
        }
        for (comment, status) in &self.pending.comment_status_updates {
            next.comments
                .get_mut(comment)
                .ok_or(StoreError::NotFound("comment"))?
                .moderation_status = *status;
        }
        for notification in &self.pending.notifications {
            let duplicate = notification.source_seq.is_some_and(|source_seq| {
                next.notifications
                    .iter()
                    .any(|row| row.user == notification.user && row.source_seq == Some(source_seq))
            });
            if !duplicate {
                next.notifications.push(notification.clone());
            }
        }
        if let Some(cursor) = self.pending.notifier_cursor {
            next.notifier_cursor = cursor;
        }
        ops::apply_phase6(&mut next, &self.pending.phase6)?;
        apply_phase7(&self.pending.phase7, &mut next)?;
        next.outbox.extend(self.pending.events.iter().cloned());
        *st = next;
        Ok(())
    }
}

#[async_trait]
impl Store for InMemoryStore {
    async fn trade_tx(&self) -> Result<Box<dyn TradeTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn vote_tx(&self) -> Result<Box<dyn VoteTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn resolve_tx(&self) -> Result<Box<dyn ResolveTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn deposit_tx(&self) -> Result<Box<dyn DepositTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn seed_tx(&self) -> Result<Box<dyn SeedTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn advance_tx(&self) -> Result<Box<dyn AdvanceTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn bootstrap_tx(&self) -> Result<Box<dyn BootstrapTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn integrity_tx(&self) -> Result<Box<dyn IntegrityTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn comment_tx(&self) -> Result<Box<dyn CommentTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn notification_tx(&self) -> Result<Box<dyn NotificationTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn content_tx(&self) -> Result<Box<dyn ContentTx + '_>, StoreError> {
        content::open(self)
    }

    async fn video_tx(&self) -> Result<Box<dyn VideoTx + '_>, StoreError> {
        video::open(self)
    }

    async fn ops_config_tx(&self) -> Result<Box<dyn OpsConfigTx + '_>, StoreError> {
        Ok(Box::new(ops_config::FakeOpsConfigTx::open(Arc::clone(
            &self.shared,
        ))))
    }

    async fn ops_audit_tx(&self) -> Result<Box<dyn OpsAuditTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn invariant_read_tx(&self) -> Result<Box<dyn InvariantReadTx + '_>, StoreError> {
        Ok(Box::new(ops::open_invariants(self)))
    }

    async fn unwind_tx(&self) -> Result<Box<dyn UnwindTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn withdraw_tx(&self) -> Result<Box<dyn crate::ports::WithdrawTx + '_>, StoreError> {
        Err(StoreError::Unavailable("phase7:withdraw"))
    }

    async fn deposit_admission_tx(
        &self,
    ) -> Result<Box<dyn crate::ports::DepositAdmissionTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }

    async fn credit_convert_tx(
        &self,
    ) -> Result<Box<dyn crate::ports::CreditConvertTx + '_>, StoreError> {
        Ok(Box::new(self.tx()))
    }
}

#[async_trait]
impl crate::ports::WithdrawalEligibility for InMemoryStore {
    async fn withdrawal_eligibility(
        &self,
        user: UserId,
    ) -> Result<crate::model::WithdrawalEligibilityView, StoreError> {
        ops::withdrawal_eligibility_view(self, user)
    }
}

#[async_trait]
impl crate::ports::AuditWrite for InMemTx {
    async fn audit_insert(&mut self, action: crate::model::AdminAction) -> Result<(), StoreError> {
        // The real D26 sink (W2): buffered, applied atomically at commit.
        ops::tx_audit_insert(self, action)
    }
}

/// Settable test clock.
pub struct FakeClock {
    now: PlMutex<OffsetDateTime>,
}

impl FakeClock {
    #[must_use]
    pub fn at(t: OffsetDateTime) -> Self {
        Self {
            now: PlMutex::new(t),
        }
    }

    pub fn set(&self, t: OffsetDateTime) {
        *self.now.lock() = t;
    }

    pub fn advance(&self, d: time::Duration) {
        let mut now = self.now.lock();
        *now += d;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> OffsetDateTime {
        *self.now.lock()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn live_market(store: &InMemoryStore, slug: &str) -> MarketRow {
        store
            .add_market(
                slug,
                MarketState::Live,
                t0() + time::Duration::hours(2),
                t0() + time::Duration::hours(1),
                MicroShares(100_000_000),
                BasisPoints(100),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn phase7_store_factories_are_unavailable() {
        let store = InMemoryStore::new();
        assert!(matches!(
            store.withdraw_tx().await,
            Err(StoreError::Unavailable("phase7:withdraw"))
        ));
        assert!(store.deposit_admission_tx().await.is_ok());
        assert!(store.credit_convert_tx().await.is_ok());
    }

    #[tokio::test]
    async fn market_queries_resolve_by_slug_uuid_and_status() {
        let store = InMemoryStore::new();
        let live = live_market(&store, "live-market");
        let other = live_market(&store, "closed-market");
        store.set_market_state(other.id, MarketState::Closed);

        assert_eq!(
            store.market_by_ref("live-market").await.unwrap().id,
            live.id
        );
        assert_eq!(
            store
                .market_by_ref(&other.id.0.to_string())
                .await
                .unwrap()
                .id,
            other.id
        );
        assert_eq!(
            store.market_by_ref(&Uuid::new_v4().to_string()).await,
            Err(StoreError::NotFound("market"))
        );

        let all = store.list_markets(None).await.unwrap();
        assert_eq!(all.len(), 2);
        let closed = store.list_markets(Some("closed")).await.unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, other.id);
        assert!(store.list_markets(Some("voided")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn channel_links_resolve_users() {
        let store = InMemoryStore::new();
        let user = UserId(Uuid::new_v4());
        store.link_user_channel("imessage", "+15550100", user);
        assert_eq!(
            store
                .user_by_channel("imessage", "+15550100")
                .await
                .unwrap(),
            Some(user)
        );
        assert_eq!(
            store
                .user_by_channel("imessage", "+15550199")
                .await
                .unwrap(),
            None
        );
        assert!(!store
            .user_voted(user, MarketId(Uuid::new_v4()))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn tally_reads_vote_fixtures_in_tx() {
        let store = InMemoryStore::new();
        let market = live_market(&store, "tallied");
        let (yes, no) = (UserId(Uuid::new_v4()), UserId(Uuid::new_v4()));
        store.record_vote(yes, market.id, Side::Yes);
        store.record_vote(yes, market.id, Side::Yes); // duplicate is a no-op
        store.record_vote(no, market.id, Side::No);

        let mut tx = store.trade_tx().await.unwrap();
        let tally = tx.tally(market.id).await.unwrap();
        assert_eq!((tally.yes_votes, tally.no_votes), (1, 1));
        let empty = tx.tally(MarketId(Uuid::new_v4())).await.unwrap();
        assert_eq!(empty.total(), 0);
    }

    #[tokio::test]
    async fn balances_and_positions_views() {
        let store = InMemoryStore::new();
        assert_eq!(store.balance_of(OwnerRef::House, Currency::Usdc), None);
        let user = UserId(Uuid::new_v4());
        store.fund_user(user, MicroUsd(42)).unwrap();
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(42))
        );
        assert!(store.positions(user).await.unwrap().is_empty());
        assert_eq!(store.stored_trade(TradeId(Uuid::new_v4())), None);
        assert_eq!(
            store.pool(MarketId(Uuid::new_v4())).await,
            Err(StoreError::NotFound("pool"))
        );
    }

    #[test]
    fn fake_clock_sets_and_advances() {
        let clock = FakeClock::at(t0());
        clock.advance(time::Duration::minutes(5));
        assert_eq!(clock.now(), t0() + time::Duration::minutes(5));
        clock.set(t0());
        assert_eq!(clock.now(), t0());
    }

    #[test]
    fn state_names_and_vote_visibility_cover_every_terminal_state() {
        let expected = [
            (MarketState::Draft, "draft"),
            (MarketState::Scheduled, "scheduled"),
            (MarketState::Live, "live"),
            (MarketState::Closing, "closing"),
            (MarketState::Closed, "closed"),
            (MarketState::Resolving, "resolving"),
            (MarketState::Resolved, "resolved"),
            (MarketState::Paid, "paid"),
            (MarketState::Voided, "voided"),
        ];
        for (state, name) in expected {
            assert_eq!(state_name(state), name);
        }

        let vote = StoredVote {
            id: VoteId(Uuid::new_v4()),
            market: MarketId(Uuid::new_v4()),
            user: UserId(Uuid::new_v4()),
            side: Side::Yes,
            crowd_guess_pct: 61,
            seq: 9,
            idempotency_key: "visibility".to_string(),
            score: None,
            score_created_at: None,
            created_at: t0(),
            cast_ip: None,
            device_hash: None,
        };
        assert_eq!(vote.receipt(MarketState::Live, t0(), t0()).seq, None);
        for state in [
            MarketState::Resolved,
            MarketState::Paid,
            MarketState::Voided,
        ] {
            assert_eq!(vote.receipt(state, t0(), t0()).seq, Some(9));
        }
        assert_eq!(
            vote.receipt(MarketState::Live, t0() + time::Duration::seconds(1), t0())
                .seq,
            Some(9)
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn notification_ports_are_transactional_deduplicated_scoped_and_enriched() {
        let store = InMemoryStore::new();
        let market = live_market(&store, "notification-ports");
        let holder = store.add_user("Holder", t0(), 2);
        let voter = store.add_user("voter", t0(), 1);
        let parent_author = store.add_user("parent", t0(), 1);
        let ledger = Uuid::new_v4();
        let parent = CommentId(Uuid::new_v4());
        let child = CommentId(Uuid::new_v4());
        {
            let mut state = store.shared.state.lock();
            state.outbox.extend([
                Event {
                    event_type: "one",
                    aggregate_type: "market",
                    aggregate_id: market.id.0,
                    payload: serde_json::json!({"n":1}),
                },
                Event {
                    event_type: "two",
                    aggregate_type: "market",
                    aggregate_id: market.id.0,
                    payload: serde_json::json!({"n":2}),
                },
            ]);
            state.realizations.extend([
                RealizationFact {
                    user: holder,
                    market: market.id,
                    outcome: market.yes_outcome,
                    source: RealizationSource::Settlement,
                    realized_delta: MicroUsd(7),
                    payout: MicroUsd(10),
                    ledger_txn: ledger,
                    created_at: t0(),
                },
                RealizationFact {
                    user: holder,
                    market: market.id,
                    outcome: market.no_outcome,
                    source: RealizationSource::Settlement,
                    realized_delta: MicroUsd(-2),
                    payout: MicroUsd(3),
                    ledger_txn: ledger,
                    created_at: t0(),
                },
                RealizationFact {
                    user: voter,
                    market: market.id,
                    outcome: market.no_outcome,
                    source: RealizationSource::Void,
                    realized_delta: MicroUsd(0),
                    payout: MicroUsd(8),
                    ledger_txn: Uuid::new_v4(),
                    created_at: t0(),
                },
            ]);
            let vote_id = VoteId(Uuid::new_v4());
            state.vote_rows.insert(
                vote_id.0,
                StoredVote {
                    id: vote_id,
                    market: market.id,
                    user: voter,
                    side: Side::No,
                    crowd_guess_pct: 40,
                    seq: 1,
                    idempotency_key: "notify-vote".to_string(),
                    score: Some(domain::scoring::VoteScore {
                        accuracy_bp: 9_000,
                        majority_bp: 10_000,
                        score_bp: 9_250,
                    }),
                    score_created_at: Some(t0()),
                    created_at: t0(),
                    cast_ip: None,
                    device_hash: None,
                },
            );
            for (id, author, parent_id) in
                [(parent, parent_author, None), (child, holder, Some(parent))]
            {
                state.comments.insert(
                    id.0,
                    CommentRow {
                        id,
                        market: market.id,
                        author,
                        parent: parent_id,
                        body: "body".to_string(),
                        body_hash: Some("hash".to_string()),
                        score: 0,
                        moderation_status: ModerationStatus::Visible,
                        depth: u8::from(parent_id.is_some()),
                        reply_count: 0,
                        created_at: t0(),
                    },
                );
            }
        }

        let mut tx = store.notification_tx().await.unwrap();
        assert_eq!(tx.lock_notifier_cursor().await.unwrap(), 0);
        assert_eq!(tx.lock_notifier_cursor().await.unwrap(), 0);
        let events = tx.outbox_events_after(0, 1).await.unwrap();
        assert_eq!(
            (events.len(), events[0].seq, events[0].event_type.as_str()),
            (1, 1, "one")
        );
        assert!(tx.outbox_events_after(2, 10).await.unwrap().is_empty());
        let settled = tx.resolution_recipients(market.id, false).await.unwrap();
        assert_eq!(settled.len(), 2);
        let holder_row = settled.iter().find(|row| row.user == holder).unwrap();
        assert_eq!(
            (holder_row.payout_total, holder_row.realized_delta),
            (MicroUsd(13), MicroUsd(5))
        );
        let voter_row = settled.iter().find(|row| row.user == voter).unwrap();
        assert!(!voter_row.held);
        assert_eq!(
            (voter_row.side, voter_row.score_bp),
            (Some(Side::No), Some(9_250))
        );
        let voided = tx.resolution_recipients(market.id, true).await.unwrap();
        assert_eq!(
            voided
                .iter()
                .find(|row| row.user == voter)
                .unwrap()
                .payout_total,
            MicroUsd(8)
        );
        assert_eq!(tx.parent_author(child).await.unwrap(), Some(parent_author));
        assert_eq!(tx.parent_author(parent).await.unwrap(), None);
        assert_eq!(
            tx.users_by_handles(&["holder".to_string(), "missing".to_string()])
                .await
                .unwrap(),
            vec![holder]
        );
        let notification = NewNotification {
            user: holder,
            notification_type: "mention".to_string(),
            market: Some(market.id),
            payload: serde_json::json!({"ok":true}),
            source_seq: 1,
            created_at: t0(),
        };
        let second_notification = NewNotification {
            source_seq: 2,
            created_at: t0() + time::Duration::SECOND,
            ..notification.clone()
        };
        let inserted = tx
            .insert_notifications(&[notification.clone(), second_notification])
            .await
            .unwrap();
        assert_eq!(inserted.len(), 2);
        assert!(tx
            .insert_notifications(std::slice::from_ref(&notification))
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            tx.notification_count_since(holder, "mention", t0())
                .await
                .unwrap(),
            2
        );
        tx.advance_notifier_cursor(2).await.unwrap();
        tx.commit().await.unwrap();

        let mut replay = store.notification_tx().await.unwrap();
        assert_eq!(replay.lock_notifier_cursor().await.unwrap(), 2);
        assert!(replay
            .insert_notifications(&[notification])
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            replay
                .notification_count_since(holder, "mention", t0())
                .await
                .unwrap(),
            2
        );
        drop(replay);
        assert_eq!(store.unread_count(holder).await.unwrap(), 2);
        let rows = store.notifications(holder, 10, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        let before = store
            .notifications(holder, 10, Some(rows[0].id))
            .await
            .unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(
            store
                .mark_notifications_read(voter, &[rows[0].id], t0())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .mark_notifications_read(holder, &[rows[0].id], t0())
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.unread_count(holder).await.unwrap(), 1);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn social_query_models_order_scope_and_terminal_privacy() {
        let store = InMemoryStore::new();
        let market = live_market(&store, "social-query-models");
        let first = store.add_user("first", t0(), 1);
        let second = store.add_user("second", t0(), 2);
        let visible = CommentId(Uuid::new_v4());
        let shadow = CommentId(Uuid::new_v4());
        {
            let mut state = store.shared.state.lock();
            state.positions.insert(
                (first.0, market.yes_outcome.0),
                PositionRow {
                    user: first,
                    outcome: market.yes_outcome,
                    shares: MicroShares(10),
                    cost: MicroUsd(30),
                    realized_pnl: MicroUsd(0),
                },
            );
            state.positions.insert(
                (second.0, market.yes_outcome.0),
                PositionRow {
                    user: second,
                    outcome: market.yes_outcome,
                    shares: MicroShares(20),
                    cost: MicroUsd(30),
                    realized_pnl: MicroUsd(0),
                },
            );
            state.positions.insert(
                (second.0, market.no_outcome.0),
                PositionRow {
                    user: second,
                    outcome: market.no_outcome,
                    shares: MicroShares(1),
                    cost: MicroUsd(0),
                    realized_pnl: MicroUsd(0),
                },
            );
            state.trades.insert(
                Uuid::new_v4(),
                StoredTrade {
                    id: TradeId(Uuid::new_v4()),
                    new: NewTrade {
                        market: market.id,
                        user: first,
                        outcome: market.yes_outcome,
                        side: Side::Yes,
                        action: crate::model::TradeAction::Buy,
                        shares: MicroShares(2),
                        gross: MicroUsd(3),
                        fee: MicroUsd(1),
                        avg_price_micro: 500_000,
                        ledger_txn: Uuid::new_v4(),
                        run_id: None,
                        pending_action_id: None,
                    },
                    trade_seq: 1,
                    created_at: t0(),
                },
            );
            state.trades.insert(
                Uuid::new_v4(),
                StoredTrade {
                    id: TradeId(Uuid::new_v4()),
                    new: NewTrade {
                        market: market.id,
                        user: first,
                        outcome: market.no_outcome,
                        side: Side::No,
                        action: crate::model::TradeAction::Sell,
                        shares: MicroShares(1),
                        gross: MicroUsd(2),
                        fee: MicroUsd(0),
                        avg_price_micro: 500_000,
                        ledger_txn: Uuid::new_v4(),
                        run_id: None,
                        pending_action_id: None,
                    },
                    trade_seq: 2,
                    created_at: t0() + time::Duration::SECOND,
                },
            );
            let vote_id = VoteId(Uuid::new_v4());
            state.vote_rows.insert(
                vote_id.0,
                StoredVote {
                    id: vote_id,
                    market: market.id,
                    user: first,
                    side: Side::Yes,
                    crowd_guess_pct: 50,
                    seq: 1,
                    idempotency_key: "profile-vote".to_string(),
                    score: Some(domain::scoring::VoteScore {
                        accuracy_bp: 7_000,
                        majority_bp: 10_000,
                        score_bp: 7_750,
                    }),
                    score_created_at: Some(t0()),
                    created_at: t0(),
                    cast_ip: None,
                    device_hash: None,
                },
            );
            let second_vote = VoteId(Uuid::new_v4());
            state.vote_rows.insert(
                second_vote.0,
                StoredVote {
                    id: second_vote,
                    market: market.id,
                    user: first,
                    side: Side::No,
                    crowd_guess_pct: 40,
                    seq: 2,
                    idempotency_key: "profile-vote-two".to_string(),
                    score: None,
                    score_created_at: None,
                    created_at: t0() + time::Duration::SECOND,
                    cast_ip: None,
                    device_hash: None,
                },
            );
            state.realizations.push(RealizationFact {
                user: first,
                market: market.id,
                outcome: market.yes_outcome,
                source: RealizationSource::Sell,
                realized_delta: MicroUsd(9),
                payout: MicroUsd(12),
                ledger_txn: Uuid::new_v4(),
                created_at: t0(),
            });
            for (id, status, author, created_at) in [
                (visible, ModerationStatus::Visible, first, t0()),
                (
                    shadow,
                    ModerationStatus::Shadow,
                    second,
                    t0() + time::Duration::SECOND,
                ),
            ] {
                state.comments.insert(
                    id.0,
                    CommentRow {
                        id,
                        market: market.id,
                        author,
                        parent: None,
                        body: "comment".to_string(),
                        body_hash: Some("hash".to_string()),
                        score: 0,
                        moderation_status: status,
                        depth: 0,
                        reply_count: 0,
                        created_at,
                    },
                );
            }
            state.comment_reports.insert((visible.0, second.0), t0());
        }
        assert_eq!(
            store.comment_view(visible, None).await.unwrap().row.id,
            visible
        );
        assert_eq!(
            store
                .comment_view(shadow, Some(second))
                .await
                .unwrap()
                .row
                .id,
            shadow
        );
        assert_eq!(
            store.comment_view(shadow, None).await,
            Err(StoreError::NotFound("comment"))
        );
        let holders = store.holders(market.id, 1).await.unwrap();
        assert_eq!(holders.yes.len(), 1);
        let expected = if first.0 < second.0 { first } else { second };
        assert_eq!(holders.yes[0].user, expected);
        assert!(holders.no.is_empty());
        assert_eq!(
            store.holders(MarketId(Uuid::new_v4()), 1).await,
            Err(StoreError::NotFound("market"))
        );
        let live_profile = store.user_profile(first).await.unwrap();
        assert_eq!(live_profile.realized_pnl, MicroUsd(9));
        assert_eq!(live_profile.avg_score_bp, Some(7_750));
        assert_eq!(live_profile.recent_trades.len(), 2);
        assert_eq!(live_profile.recent_trades[0].trade_seq, 2);
        assert_eq!(live_profile.recent_votes.len(), 2);
        assert_eq!(live_profile.recent_votes[0].side, None);
        store.set_market_state(market.id, MarketState::Paid);
        let terminal = store.user_profile(first).await.unwrap();
        assert_eq!(terminal.recent_votes[0].side, Some(Side::No));
        let scored = terminal
            .recent_votes
            .iter()
            .find(|vote| vote.score_bp.is_some())
            .unwrap();
        assert_eq!(
            (scored.side, scored.score_bp),
            (Some(Side::Yes), Some(7_750))
        );
        assert_eq!(
            store.user_profile(UserId(Uuid::new_v4())).await,
            Err(StoreError::NotFound("user"))
        );
        let reported = store.reported_comments(1, 10).await.unwrap();
        assert_eq!(reported.len(), 2);
        assert_eq!(reported[0].comment.row.id, shadow);
        assert_eq!(reported[1].reporters, vec!["second".to_string()]);
        assert!(store.reported_comments(2, 1).await.unwrap()[0]
            .reporters
            .is_empty());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn one_transaction_reads_its_pending_writes_and_rejects_duplicates() {
        let store = InMemoryStore::new();
        let invalid_market = store
            .add_market(
                "invalid-pool",
                MarketState::Live,
                t0(),
                t0(),
                MicroShares(0),
                BasisPoints(0),
            )
            .unwrap_err();
        assert!(matches!(invalid_market, StoreError::Backend(_)));
        let committed = live_market(&store, "pending-reads");
        let mut tx = store.tx();

        let user = tx.insert_user("pending-user").await.unwrap();
        assert_eq!(tx.handle(user).await.unwrap(), "pending-user");
        assert!(tx.user_created_at(user).await.unwrap() <= OffsetDateTime::now_utc());
        tx.link_channel(user, "imessage", "+15550000001")
            .await
            .unwrap();
        assert!(tx.user_has_channel(user, "imessage").await.unwrap());
        assert_eq!(
            tx.link_channel(user, "imessage", "+15550000001").await,
            Err(StoreError::Conflict("channel link"))
        );

        let pending_market = MarketId(Uuid::new_v4());
        let new_market = NewMarket {
            id: pending_market,
            slug: "pending-market".to_string(),
            min_votes_to_resolve: 2,
            closes_at: t0() + time::Duration::hours(2),
            tally_hidden_at: t0() + time::Duration::hours(1),
        };
        assert_eq!(
            tx.insert_market(new_market.clone()).await.unwrap(),
            pending_market
        );
        assert_eq!(
            tx.insert_market(new_market).await,
            Err(StoreError::Conflict("market id"))
        );
        MarketWriter::set_market_state(&mut tx, pending_market, MarketState::Scheduled)
            .await
            .unwrap();
        assert_eq!(
            tx.market_for_update(pending_market).await.unwrap().state,
            MarketState::Scheduled
        );
        assert_eq!(
            MarketWriter::set_market_state(&mut tx, MarketId(Uuid::new_v4()), MarketState::Live,)
                .await,
            Err(StoreError::NotFound("market"))
        );
        let invalid_pool = tx
            .create_pool(pending_market, BasisPoints(0), MicroUsd(0))
            .await
            .unwrap_err();
        assert!(matches!(invalid_pool, StoreError::Backend(_)));
        tx.create_pool(pending_market, BasisPoints(0), MicroUsd(1_000_000))
            .await
            .unwrap();
        assert_eq!(
            tx.pool_for_update(pending_market).await.unwrap().pool.yes,
            MicroShares(1_000_000)
        );

        let command = LifecycleCommand {
            market: pending_market,
            event: domain::market::MarketEvent::Approve,
            resulting_state: MarketState::Scheduled,
        };
        tx.record_lifecycle_command("pending-command", command)
            .await
            .unwrap();
        assert_eq!(
            tx.lifecycle_command("pending-command").await.unwrap(),
            Some(command)
        );
        assert_eq!(
            tx.record_lifecycle_command("pending-command", command)
                .await,
            Err(StoreError::DuplicateKey)
        );

        let position = PositionRow {
            user,
            outcome: committed.yes_outcome,
            shares: MicroShares(5),
            cost: MicroUsd(3),
            realized_pnl: MicroUsd(1),
        };
        tx.save_position(position).await.unwrap();
        assert_eq!(
            tx.position_for_update(user, committed.yes_outcome)
                .await
                .unwrap(),
            Some(position)
        );

        let ledger_txn = Uuid::new_v4();
        let trade = NewTrade {
            market: committed.id,
            user,
            outcome: committed.yes_outcome,
            side: Side::Yes,
            action: crate::model::TradeAction::Buy,
            shares: MicroShares(10),
            gross: MicroUsd(6),
            fee: MicroUsd(1),
            avg_price_micro: 600_000,
            ledger_txn,
            run_id: None,
            pending_action_id: None,
        };
        let first = tx.insert_trade(trade.clone()).await.unwrap();
        let second = tx
            .insert_trade(NewTrade {
                ledger_txn: Uuid::new_v4(),
                ..trade
            })
            .await
            .unwrap();
        assert_eq!((first.trade_seq, second.trade_seq), (1, 2));
        assert_eq!(
            tx.trade_by_ledger_txn(ledger_txn)
                .await
                .unwrap()
                .unwrap()
                .trade_id,
            first.id
        );

        let vote = NewVote {
            market: committed.id,
            user,
            side: Side::Yes,
            crowd_guess_pct: 50,
            seq: 1,
            idempotency_key: "pending-vote".to_string(),
            created_at: t0(),
            cast_ip: None,
            device_hash: None,
        };
        tx.insert_vote(vote.clone()).await.unwrap();
        assert_eq!(tx.votes_count_since(user, t0()).await.unwrap(), 1);
        assert_eq!(
            tx.insert_vote(NewVote {
                idempotency_key: "different-key".to_string(),
                ..vote.clone()
            })
            .await,
            Err(StoreError::Conflict("vote"))
        );
        assert_eq!(
            tx.insert_vote(NewVote {
                user: UserId(Uuid::new_v4()),
                ..vote
            })
            .await,
            Err(StoreError::Conflict("vote idempotency key"))
        );
        SettlementIo::set_market_state(&mut tx, committed.id, MarketState::Resolving)
            .await
            .unwrap();
        assert!(tx.flag_curator_needed(committed.id).await.unwrap());
        assert!(!tx.flag_curator_needed(committed.id).await.unwrap());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn fake_query_projections_enforce_visibility_ordering_and_orphan_guards() {
        let store = InMemoryStore::new();
        let market = live_market(&store, "query-projections");
        let user = UserId(Uuid::new_v4());
        store.fund_user(user, MicroUsd(10)).unwrap();

        let visible = store.market_snapshot(market.id, t0()).await.unwrap();
        assert_eq!(
            visible.tally,
            Some(Tally {
                yes_votes: 0,
                no_votes: 0
            })
        );
        let hidden = store
            .market_snapshot(market.id, market.tally_hidden_at)
            .await
            .unwrap();
        assert_eq!(hidden.tally, None);
        assert_eq!(
            store.market_snapshot(MarketId(Uuid::new_v4()), t0()).await,
            Err(StoreError::NotFound("market"))
        );

        let yes_trade = StoredTrade {
            id: TradeId(Uuid::new_v4()),
            new: NewTrade {
                market: market.id,
                user,
                outcome: market.yes_outcome,
                side: Side::Yes,
                action: crate::model::TradeAction::Buy,
                shares: MicroShares(10),
                gross: MicroUsd(4),
                fee: MicroUsd(0),
                avg_price_micro: 400_000,
                ledger_txn: Uuid::new_v4(),
                run_id: None,
                pending_action_id: None,
            },
            trade_seq: 1,
            created_at: t0(),
        };
        let no_trade = StoredTrade {
            id: TradeId(Uuid::new_v4()),
            new: NewTrade {
                outcome: market.no_outcome,
                side: Side::No,
                shares: MicroShares(10),
                gross: MicroUsd(3),
                ledger_txn: Uuid::new_v4(),
                ..yes_trade.new.clone()
            },
            trade_seq: 2,
            created_at: t0() + time::Duration::seconds(30),
        };
        {
            let mut state = store.shared.state.lock();
            state.trades.insert(yes_trade.id.0, yes_trade.clone());
            state.trades.insert(no_trade.id.0, no_trade.clone());
        }
        let chart = store.price_history(market.id, 60, t0()).await.unwrap();
        assert_eq!(chart.len(), 1);
        assert_eq!(chart[0].trades, 2);
        assert_eq!(chart[0].volume_micro, 7);
        assert!(chart[0].avg_price_micro > 400_000);
        assert_eq!(
            store.price_history(market.id, 0, t0()).await,
            Err(StoreError::Invariant("invalid chart bucket"))
        );
        assert_eq!(
            store
                .price_history(MarketId(Uuid::new_v4()), 60, t0())
                .await,
            Err(StoreError::NotFound("market"))
        );

        let tape = store.tape(market.id, 1).await.unwrap();
        assert_eq!(tape.len(), 1);
        assert_eq!(tape[0].trade_seq, 2);
        assert_eq!(tape[0].handle, store.user_handle(user).unwrap());
        store.shared.state.lock().users.remove(&user.0);
        assert_eq!(
            store.tape(market.id, 200).await,
            Err(StoreError::NotFound("user"))
        );

        let orphan = PositionRow {
            user,
            outcome: OutcomeId(Uuid::new_v4()),
            shares: MicroShares(1),
            cost: MicroUsd(1),
            realized_pnl: MicroUsd(0),
        };
        store
            .shared
            .state
            .lock()
            .positions
            .insert((user.0, orphan.outcome.0), orphan);
        assert_eq!(
            store.positions(user).await,
            Err(StoreError::Invariant("position references unknown outcome"))
        );
        {
            let mut state = store.shared.state.lock();
            state.positions.clear();
            state.positions.insert(
                (user.0, market.no_outcome.0),
                PositionRow {
                    outcome: market.no_outcome,
                    ..orphan
                },
            );
        }
        let positions = store.positions(user).await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].side, Side::No);
    }

    #[test]
    fn vote_score_reader_distinguishes_absent_and_scored_votes() {
        let store = InMemoryStore::new();
        let vote_id = Uuid::new_v4();
        let market = MarketId(Uuid::new_v4());
        let user = UserId(Uuid::new_v4());
        assert_eq!(store.vote_score(vote_id), None);
        let vote_score = domain::scoring::VoteScore {
            accuracy_bp: 9_000,
            majority_bp: 10_000,
            score_bp: 9_250,
        };
        store.shared.state.lock().vote_rows.insert(
            vote_id,
            StoredVote {
                id: VoteId(vote_id),
                market,
                user,
                side: Side::Yes,
                crowd_guess_pct: 50,
                seq: 1,
                idempotency_key: "scored".to_string(),
                score: Some(vote_score),
                score_created_at: Some(t0()),
                created_at: t0(),
                cast_ip: Some("2001:db8::1".parse().unwrap()),
                device_hash: Some("device-hash".to_string()),
            },
        );
        assert_eq!(store.vote_score(vote_id), Some(vote_score));
        assert_eq!(
            store.vote_metadata_for_user(user, market),
            Some((
                Some("2001:db8::1".parse().unwrap()),
                Some("device-hash".to_string())
            ))
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn economy_queries_use_facts_and_half_open_windows() {
        let store = InMemoryStore::new();
        let alice = UserId(Uuid::new_v4());
        let bob = UserId(Uuid::new_v4());
        let charlie = UserId(Uuid::new_v4());
        let dave = UserId(Uuid::new_v4());
        let tier_zero = UserId(Uuid::new_v4());
        for (user, handle, tier) in [
            (alice, "alice", 2),
            (bob, "bob", 1),
            (charlie, "charlie", 1),
            (dave, "dave", 1),
            (tier_zero, "newbie", 0),
        ] {
            store.link_user_channel("test", handle, user);
            store
                .shared
                .state
                .lock()
                .users
                .insert(user.0, handle.into());
            store.set_user_rep(user, 500_000, tier);
        }
        let market = MarketId(Uuid::new_v4());
        let outcome = OutcomeId(Uuid::new_v4());
        {
            let mut st = store.shared.state.lock();
            for (user, delta, second) in [
                (alice, 30, 0),
                (alice, -10, 1),
                (bob, 15, 2),
                (charlie, 15, 2),
                (bob, 999, 3),
            ] {
                st.realizations.push(RealizationFact {
                    user,
                    market,
                    outcome,
                    source: crate::model::RealizationSource::Sell,
                    realized_delta: MicroUsd(delta),
                    payout: MicroUsd(delta.max(0)),
                    ledger_txn: Uuid::new_v4(),
                    created_at: t0() + time::Duration::seconds(second),
                });
            }
            for (index, (user, score)) in [
                (alice, 8_000),
                (alice, 9_001),
                (bob, 7_000),
                (charlie, 7_000),
                (dave, 7_000),
                (tier_zero, 10_000),
            ]
            .into_iter()
            .enumerate()
            {
                let id = Uuid::new_v4();
                st.vote_rows.insert(
                    id,
                    StoredVote {
                        id: VoteId(id),
                        market: MarketId(Uuid::new_v4()),
                        user,
                        side: Side::Yes,
                        crowd_guess_pct: 50,
                        seq: i64::try_from(index).unwrap(),
                        idempotency_key: format!("economy-{index}"),
                        score: Some(domain::scoring::VoteScore {
                            accuracy_bp: score,
                            majority_bp: score,
                            score_bp: score,
                        }),
                        score_created_at: Some(t0() + time::Duration::seconds(1)),
                        created_at: t0(),
                        cast_ip: None,
                        device_hash: None,
                    },
                );
            }
            let fees = account_in(&mut st, OwnerRef::Fees, Currency::Usdc).unwrap();
            let external = account_in(&mut st, OwnerRef::External, Currency::Usdc).unwrap();
            for (kind, amount) in [
                (TxnKind::Trade, 10),
                (TxnKind::Payout, 3),
                (TxnKind::Seed, 7),
            ] {
                let id = Uuid::new_v4();
                st.ledger_txns.insert(
                    id,
                    PendingTxn {
                        key: format!("fee-{id}"),
                        id,
                        txn: Transaction::new(
                            kind,
                            vec![
                                Entry {
                                    account: fees,
                                    amount: MicroUsd(amount),
                                },
                                Entry {
                                    account: external,
                                    amount: MicroUsd(-amount),
                                },
                            ],
                        )
                        .unwrap(),
                        created_at: t0() + time::Duration::seconds(1),
                    },
                );
            }
        }

        let until = t0() + time::Duration::seconds(3);
        assert_eq!(
            store.top_traders(t0(), until, 10).await.unwrap(),
            vec![
                TraderRow {
                    handle: "alice".into(),
                    realized_pnl_micro: 20,
                    realizations: 2
                },
                TraderRow {
                    handle: "bob".into(),
                    realized_pnl_micro: 15,
                    realizations: 1
                },
                TraderRow {
                    handle: "charlie".into(),
                    realized_pnl_micro: 15,
                    realizations: 1
                },
            ]
        );
        assert_eq!(
            store.top_voters(t0(), until, 10, 2).await.unwrap(),
            vec![VoterRow {
                handle: "alice".into(),
                avg_score_bp: 8_501,
                markets_scored: 2,
                tier: 2,
            }]
        );
        assert_eq!(
            store.top_voters(t0(), until, 10, 1).await.unwrap(),
            vec![
                VoterRow {
                    handle: "alice".into(),
                    avg_score_bp: 8_501,
                    markets_scored: 2,
                    tier: 2,
                },
                VoterRow {
                    handle: "bob".into(),
                    avg_score_bp: 7_000,
                    markets_scored: 1,
                    tier: 1,
                },
                VoterRow {
                    handle: "charlie".into(),
                    avg_score_bp: 7_000,
                    markets_scored: 1,
                    tier: 1,
                },
                VoterRow {
                    handle: "dave".into(),
                    avg_score_bp: 7_000,
                    markets_scored: 1,
                    tier: 1,
                },
            ]
        );
        assert!(store
            .top_voters(t0(), until, 10, 3)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store.fee_summary(t0(), until).await.unwrap(),
            vec![DailyFeeRow {
                day: t0().date(),
                trade_fee_micro: 10,
                payout_dust_micro: 3,
                total_micro: 13,
            }]
        );
        assert_eq!(
            store
                .top_traders(until, until + time::Duration::seconds(1), 10)
                .await
                .unwrap(),
            vec![TraderRow {
                handle: "bob".into(),
                realized_pnl_micro: 999,
                realizations: 1,
            }]
        );
    }

    #[tokio::test]
    async fn realization_insert_is_idempotent_before_and_across_commits() {
        let store = InMemoryStore::new();
        let fact = RealizationFact {
            user: UserId(Uuid::new_v4()),
            market: MarketId(Uuid::new_v4()),
            outcome: OutcomeId(Uuid::new_v4()),
            source: crate::model::RealizationSource::Sell,
            realized_delta: MicroUsd(4),
            payout: MicroUsd(9),
            ledger_txn: Uuid::new_v4(),
            created_at: t0(),
        };
        let mut first = store.trade_tx().await.unwrap();
        first.serialize_key("realization-first").await.unwrap();
        assert!(first.insert_realization(&fact).await.unwrap());
        assert!(!first.insert_realization(&fact).await.unwrap());

        let mut racer = store.trade_tx().await.unwrap();
        racer.serialize_key("realization-racer").await.unwrap();
        assert!(racer.insert_realization(&fact).await.unwrap());
        first.commit().await.unwrap();
        racer.commit().await.unwrap();
        assert_eq!(store.realizations(), vec![fact]);

        let mut replay = store.trade_tx().await.unwrap();
        replay.serialize_key("realization-replay").await.unwrap();
        assert!(!replay.insert_realization(&fact).await.unwrap());
    }

    #[tokio::test]
    async fn pending_comment_duplicate_is_rejected_before_commit() {
        let store = InMemoryStore::new();
        let id = CommentId(Uuid::new_v4());
        let comment = NewComment {
            id,
            market: MarketId(Uuid::new_v4()),
            author: UserId(Uuid::new_v4()),
            parent: None,
            body: "body".to_string(),
            body_hash: "hash".to_string(),
            moderation_status: ModerationStatus::Visible,
            depth: 0,
            created_at: t0(),
        };
        let mut tx = store.tx();
        tx.insert_comment(comment.clone()).await.unwrap();
        assert_eq!(
            tx.insert_comment(comment).await,
            Err(StoreError::Conflict("comment"))
        );
    }
}
