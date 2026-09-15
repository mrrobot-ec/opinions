#[async_trait]
impl IdempotencyGuard for InMemTx {
    async fn serialize_key(&mut self, key: &str) -> Result<(), StoreError> {
        if !self.key_guards.contains_key(key) {
            let handle = self.shared.key_locks.handle(&key.to_string());
            let guard = handle.lock_owned().await;
            self.key_guards.insert(key.to_string(), guard);
        }
        Ok(())
    }
}

#[async_trait]
impl UserLockGuard for InMemTx {
    async fn lock_user(&mut self, user: UserId) -> Result<(), StoreError> {
        if !self.user_guards.contains_key(&user.0) {
            let handle = self.shared.user_locks.handle(&user.0);
            let guard = handle.lock_owned().await;
            self.user_guards.insert(user.0, guard);
        }
        Ok(())
    }
}

#[async_trait]
impl MarketReader for InMemTx {
    async fn market_for_update(&mut self, m: MarketId) -> Result<MarketRow, StoreError> {
        if !self.market_guards.contains_key(&m.0) {
            let handle = self.shared.market_locks.handle(&m.0);
            let guard = handle.lock_owned().await;
            self.market_guards.insert(m.0, guard);
        }
        let st = self.shared.state.lock();
        self.market_view(&st, m)
            .ok_or(StoreError::NotFound("market"))
    }
}

#[async_trait]
impl VoteReader for InMemTx {
    async fn user_voted(&mut self, u: UserId, m: MarketId) -> Result<bool, StoreError> {
        Ok(self.shared.state.lock().votes.contains(&(u.0, m.0)))
    }

    async fn tally(&mut self, m: MarketId) -> Result<Tally, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .tallies
            .get(&m.0)
            .copied()
            .unwrap_or(Tally {
                yes_votes: 0,
                no_votes: 0,
            }))
    }

    async fn votes_count_since(
        &mut self,
        user: UserId,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let committed = self
            .shared
            .state
            .lock()
            .vote_rows
            .values()
            .filter(|vote| vote.user == user && vote.created_at >= since)
            .count();
        let pending = self
            .pending
            .votes
            .iter()
            .filter(|vote| vote.user == user && vote.created_at >= since)
            .count();
        u32::try_from(committed + pending).map_err(|_| StoreError::Invariant("vote count overflow"))
    }

    async fn user_created_at(&mut self, user: UserId) -> Result<OffsetDateTime, StoreError> {
        self.pending
            .user_created_at
            .iter()
            .find(|(id, _)| *id == user)
            .map(|(_, created)| *created)
            .or_else(|| {
                self.shared
                    .state
                    .lock()
                    .user_created_at
                    .get(&user.0)
                    .copied()
            })
            .ok_or(StoreError::NotFound("user"))
    }

    async fn user_has_channel(&mut self, user: UserId, channel: &str) -> Result<bool, StoreError> {
        let pending = self
            .pending
            .channels
            .iter()
            .any(|((kind, _), id)| kind == channel && *id == user);
        Ok(pending
            || self
                .shared
                .state
                .lock()
                .channels
                .iter()
                .any(|((kind, _), id)| kind == channel && *id == user.0))
    }
}

#[async_trait]
impl PoolWriter for InMemTx {
    async fn pool_for_update(&mut self, m: MarketId) -> Result<PoolRow, StoreError> {
        self.lock_pool_row(m.0).await;
        if let Some((_, pool)) = self
            .pending
            .reserves
            .iter()
            .rev()
            .find(|(id, _)| *id == m.0)
        {
            return Ok(PoolRow {
                market: m,
                pool: *pool,
            });
        }
        let st = self.shared.state.lock();
        st.pools
            .get(&m.0)
            .map(|pool| PoolRow {
                market: m,
                pool: *pool,
            })
            .ok_or(StoreError::NotFound("pool"))
    }

    async fn save_reserves(&mut self, m: MarketId, pool: &Pool) -> Result<(), StoreError> {
        self.lock_pool_row(m.0).await;
        self.pending.reserves.push((m.0, *pool));
        Ok(())
    }
}

#[async_trait]
impl LedgerWriter for InMemTx {
    async fn txn_by_key(&mut self, key: &str) -> Result<Option<uuid::Uuid>, StoreError> {
        if let Some(pending) = self.pending.txns.iter().find(|t| t.key == key) {
            return Ok(Some(pending.id));
        }
        Ok(self.shared.state.lock().txn_keys.get(key).copied())
    }

    async fn account(
        &mut self,
        owner: OwnerRef,
        currency: Currency,
    ) -> Result<AccountId, StoreError> {
        let mut st = self.shared.state.lock();
        account_in(&mut st, owner, currency)
    }

    async fn ledger_apply(
        &mut self,
        kind: TxnKind,
        key: &str,
        entries: &[Entry],
    ) -> Result<uuid::Uuid, StoreError> {
        if self.pending.txns.iter().any(|t| t.key == key)
            || self.shared.state.lock().txn_keys.contains_key(key)
        {
            return Err(StoreError::DuplicateKey);
        }
        let txn = Transaction::new(kind, entries.to_vec()).map_err(StoreError::Ledger)?;
        self.lock_accounts(entries).await;
        self.validate_against_current(&txn)?;
        let id = Uuid::new_v4();
        self.pending.txns.push(PendingTxn {
            key: key.to_string(),
            id,
            txn,
            created_at: OffsetDateTime::now_utc(),
        });
        Ok(id)
    }
}

#[async_trait]
impl TradeWriter for InMemTx {
    async fn insert_trade(&mut self, t: NewTrade) -> Result<InsertedTrade, StoreError> {
        let id = TradeId(Uuid::new_v4());
        let market = t.market;
        let committed = self
            .shared
            .state
            .lock()
            .trades
            .values()
            .filter(|row| row.new.market == market)
            .map(|row| row.trade_seq)
            .max()
            .unwrap_or(0);
        let trade_seq = self
            .pending
            .trades
            .iter()
            .filter(|row| row.new.market == market)
            .map(|row| row.trade_seq)
            .max()
            .unwrap_or(committed)
            .checked_add(1)
            .ok_or(StoreError::Invariant("trade sequence overflow"))?;
        let created_at = OffsetDateTime::now_utc();
        self.pending.trades.push(StoredTrade {
            id,
            new: t,
            trade_seq,
            created_at,
        });
        Ok(InsertedTrade {
            id,
            trade_seq,
            created_at,
        })
    }

    async fn trade_by_ledger_txn(
        &mut self,
        txn: uuid::Uuid,
    ) -> Result<Option<TradeReceipt>, StoreError> {
        if let Some(t) = self.pending.trades.iter().find(|t| t.new.ledger_txn == txn) {
            return Ok(Some(t.receipt()));
        }
        let st = self.shared.state.lock();
        Ok(st
            .trades_by_txn
            .get(&txn)
            .and_then(|id| st.trades.get(id))
            .map(StoredTrade::receipt))
    }
}

#[async_trait]
impl UserReader for InMemTx {
    async fn handle(&mut self, user: UserId) -> Result<String, StoreError> {
        if let Some((_, handle)) = self.pending.users.iter().find(|(id, _)| *id == user) {
            return Ok(handle.clone());
        }
        self.shared
            .state
            .lock()
            .users
            .get(&user.0)
            .cloned()
            .ok_or(StoreError::NotFound("user"))
    }

    async fn user_by_handle(&mut self, handle: &str) -> Result<Option<UserId>, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .users
            .iter()
            .find(|(_, candidate)| candidate.eq_ignore_ascii_case(handle))
            .map(|(id, _)| UserId(*id)))
    }

    async fn user_tier(&mut self, user: UserId) -> Result<u8, StoreError> {
        self.shared
            .state
            .lock()
            .reputation
            .get(&user.0)
            .map(|(_, tier)| *tier)
            .ok_or(StoreError::NotFound("reputation"))
    }
}

#[async_trait]
impl PositionWriter for InMemTx {
    async fn position_for_update(
        &mut self,
        u: UserId,
        o: OutcomeId,
    ) -> Result<Option<PositionRow>, StoreError> {
        self.lock_position_row((u.0, o.0)).await;
        if let Some(p) = self
            .pending
            .positions
            .iter()
            .rev()
            .find(|p| p.user == u && p.outcome == o)
        {
            return Ok(Some(*p));
        }
        Ok(self.shared.state.lock().positions.get(&(u.0, o.0)).copied())
    }

    async fn save_position(&mut self, p: PositionRow) -> Result<(), StoreError> {
        self.lock_position_row((p.user.0, p.outcome.0)).await;
        self.pending.positions.push(p);
        Ok(())
    }
}

#[async_trait]
impl TradeEconomyReader for InMemTx {
    async fn user_rep(&mut self, u: UserId) -> Result<ReputationRow, StoreError> {
        let (rep_micro, tier) = self
            .shared
            .state
            .lock()
            .reputation
            .get(&u.0)
            .copied()
            .ok_or(StoreError::NotFound("reputation"))?;
        Ok(ReputationRow {
            user: u,
            rep_micro,
            tier,
        })
    }

    async fn last_buy_at(
        &mut self,
        u: UserId,
        o: OutcomeId,
    ) -> Result<Option<OffsetDateTime>, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .trades
            .values()
            .filter(|trade| {
                trade.new.user == u
                    && trade.new.outcome == o
                    && trade.new.action == crate::model::TradeAction::Buy
            })
            .map(|trade| trade.created_at)
            .max())
    }

    async fn market_position_cost(
        &mut self,
        u: UserId,
        m: MarketId,
    ) -> Result<MicroUsd, StoreError> {
        let st = self.shared.state.lock();
        let market = st.markets.get(&m.0).ok_or(StoreError::NotFound("market"))?;
        let mut positions = st.positions.clone();
        for row in &self.pending.positions {
            positions.insert((row.user.0, row.outcome.0), *row);
        }
        let total = positions
            .values()
            .filter(|row| {
                row.user == u
                    && (row.outcome == market.yes_outcome || row.outcome == market.no_outcome)
            })
            .try_fold(0_i64, |sum, row| sum.checked_add(row.cost.0))
            .ok_or(StoreError::Invariant("position cost overflow"))?;
        Ok(MicroUsd(total))
    }
}

#[async_trait]
impl OutboxWriter for InMemTx {
    async fn append(&mut self, e: Event) -> Result<(), StoreError> {
        self.pending.events.push(e);
        Ok(())
    }

    async fn append_batch(&mut self, events: &[Event]) -> Result<(), StoreError> {
        self.pending.events.extend(events.iter().cloned());
        Ok(())
    }
}

#[async_trait]
impl VoteWriter for InMemTx {
    async fn allocate_vote_seq(&mut self, m: MarketId) -> Result<i64, StoreError> {
        // Locked counter: the per-market seq lock is held to tx end, so
        // allocations never interleave and committed seqs strictly increase.
        if !self.seq_guards.contains_key(&m.0) {
            let handle = self.shared.seq_locks.handle(&m.0);
            let guard = handle.lock_owned().await;
            self.seq_guards.insert(m.0, guard);
        }
        let committed = *self.shared.state.lock().vote_seq.get(&m.0).unwrap_or(&0);
        let next = self
            .pending
            .vote_seq
            .get(&m.0)
            .copied()
            .unwrap_or(committed)
            .checked_add(1)
            .ok_or(StoreError::Invariant("vote sequence overflow"))?;
        self.pending.vote_seq.insert(m.0, next);
        Ok(next)
    }

    async fn insert_vote(&mut self, v: NewVote) -> Result<VoteId, StoreError> {
        let duplicate_pair = self
            .pending
            .votes
            .iter()
            .any(|row| row.user == v.user && row.market == v.market)
            || self
                .shared
                .state
                .lock()
                .votes
                .contains(&(v.user.0, v.market.0));
        if duplicate_pair {
            return Err(StoreError::Conflict("vote"));
        }
        let duplicate_key = self
            .pending
            .votes
            .iter()
            .any(|row| row.idempotency_key == v.idempotency_key)
            || self
                .shared
                .state
                .lock()
                .vote_rows
                .values()
                .any(|row| row.idempotency_key == v.idempotency_key);
        if duplicate_key {
            return Err(StoreError::Conflict("vote idempotency key"));
        }
        let id = VoteId(Uuid::new_v4());
        self.pending.votes.push(StoredVote {
            id,
            market: v.market,
            user: v.user,
            side: v.side,
            crowd_guess_pct: v.crowd_guess_pct,
            seq: v.seq,
            idempotency_key: v.idempotency_key,
            score: None,
            score_created_at: None,
            created_at: v.created_at,
            cast_ip: v.cast_ip,
            device_hash: v.device_hash,
        });
        Ok(id)
    }

    async fn vote_by_key(
        &mut self,
        key: &str,
        now: OffsetDateTime,
    ) -> Result<Option<VoteReceipt>, StoreError> {
        if let Some(row) = self.pending.votes.iter().find(|v| v.idempotency_key == key) {
            let st = self.shared.state.lock();
            let market = st
                .markets
                .get(&row.market.0)
                .ok_or(StoreError::NotFound("market"))?;
            return Ok(Some(row.receipt(market.state, market.tally_hidden_at, now)));
        }
        let st = self.shared.state.lock();
        let Some(row) = st
            .vote_rows
            .values()
            .find(|vote| vote.idempotency_key == key)
        else {
            return Ok(None);
        };
        let market = st
            .markets
            .get(&row.market.0)
            .ok_or(StoreError::NotFound("market"))?;
        Ok(Some(row.receipt(market.state, market.tally_hidden_at, now)))
    }
}

#[async_trait]
impl crate::ports::MoneyProposalIo for InMemTx {
    async fn insert_proposal(
        &mut self,
        proposal: crate::money::MoneyProposal,
    ) -> Result<crate::money::MoneyProposal, StoreError> {
        let replay_taken = self
            .pending
            .phase7
            .proposals
            .iter()
            .any(|row| row.replay_key == proposal.replay_key)
            || self
                .shared
                .state
                .lock()
                .phase7
                .proposals
                .iter()
                .any(|row| row.replay_key == proposal.replay_key);
        if replay_taken {
            return Err(StoreError::Conflict("money proposal replay key"));
        }
        self.pending.phase7.proposals.push(proposal.clone());
        Ok(proposal)
    }

    async fn get_proposal_by_replay(
        &mut self,
        replay_key: &str,
    ) -> Result<Option<crate::money::MoneyProposal>, StoreError> {
        if let Some(row) = self
            .pending
            .phase7
            .proposals
            .iter()
            .rev()
            .find(|row| row.replay_key == replay_key)
        {
            return Ok(Some(row.clone()));
        }
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .proposals
            .iter()
            .rev()
            .find(|row| row.replay_key == replay_key)
            .cloned())
    }

    async fn get_proposal(
        &mut self,
        id: uuid::Uuid,
    ) -> Result<crate::money::MoneyProposal, StoreError> {
        if let Some(row) = self
            .pending
            .phase7
            .proposals
            .iter()
            .rev()
            .find(|row| row.id == id)
        {
            return Ok(row.clone());
        }
        self.shared
            .state
            .lock()
            .phase7
            .proposals
            .iter()
            .rev()
            .find(|row| row.id == id)
            .cloned()
            .ok_or(StoreError::NotFound("money proposal"))
    }

    async fn confirm_proposal(
        &mut self,
        id: uuid::Uuid,
        confirmer: &str,
        now: OffsetDateTime,
    ) -> Result<crate::money::MoneyProposal, StoreError> {
        // CAS on status = Pending, mirroring the Pg semantics: a second
        // confirmer moves nothing and reads back the first confirmation.
        // Rows are append-only in the fake; readers take the LATEST row per
        // id, so the confirmed copy supersedes the pending one.
        let current = crate::ports::MoneyProposalIo::get_proposal(self, id).await?;
        if current.status != crate::money::ProposalStatus::Pending {
            return Ok(current);
        }
        let mut updated = current;
        updated.status = crate::money::ProposalStatus::Confirmed;
        updated.confirmer_token_id = Some(confirmer.to_string());
        let _ = now;
        self.pending.phase7.proposals.push(updated.clone());
        Ok(updated)
    }
}

#[async_trait]
impl crate::ports::ReferralPaidFinalize for InMemTx {
    async fn referral_relevant_users(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<UserId>, StoreError> {
        let referees =
            crate::money::CreditIo::referral_referees_for_paid_market(self, market).await?;
        let mut users = Vec::with_capacity(referees.len().saturating_mul(2));
        for referee in referees {
            // The referee must be locked even before a bind exists. BindReferral
            // takes this same lock, closing the enumeration/bind TOCTOU.
            users.push(referee);
            let Some(bind) =
                crate::money::CreditIo::referral_bind_for_referee(self, referee).await?
            else {
                continue;
            };
            let (referrer, bound_referee, _) =
                crate::money::CreditIo::referral_bind_parties(self, bind)
                    .await?
                    .ok_or(StoreError::Invariant(
                        "referral bind index points to no row",
                    ))?;
            if bound_referee != referee || referrer == referee {
                continue;
            }
            users.push(referrer);
        }
        users.sort_unstable_by_key(|user| user.0);
        users.dedup();
        Ok(users)
    }

    async fn grant_referrals_on_paid(&mut self, market: MarketId) -> Result<u32, StoreError> {
        let granted_at = self
            .pending
            .lp_results
            .iter()
            .rev()
            .find(|(id, _, _)| *id == market.0)
            .map_or_else(OffsetDateTime::now_utc, |(_, _, at)| *at);
        let referees =
            crate::money::CreditIo::referral_referees_for_paid_market(self, market).await?;
        let mut granted = 0_u32;
        for referee in referees {
            if crate::money::referrals::grant_referral_on_paid_in_tx(
                self, referee, market, granted_at,
            )
            .await?
            {
                granted = granted
                    .checked_add(1)
                    .ok_or(StoreError::Invariant("referral grant count overflow"))?;
            }
        }
        Ok(granted)
    }
}

#[async_trait]
impl crate::ports::FeeAllocationFinalize for InMemTx {
    async fn finalize_market_fee_allocations(
        &mut self,
        market: MarketId,
    ) -> Result<u32, StoreError> {
        move_market_fee_allocations(self, market, crate::money::AllocationKind::Finalized).await
    }
}

#[async_trait]
impl crate::ports::FeeAllocationReverse for InMemTx {
    async fn reverse_market_fee_allocations(
        &mut self,
        market: MarketId,
    ) -> Result<u32, StoreError> {
        move_market_fee_allocations(self, market, crate::money::AllocationKind::Reversed).await
    }
}

async fn move_market_fee_allocations(
    tx: &mut InMemTx,
    market: MarketId,
    terminal_kind: crate::money::AllocationKind,
) -> Result<u32, StoreError> {
    let mut trades: Vec<Uuid> = {
        let state = tx.shared.state.lock();
        state
            .trades
            .values()
            .filter(|trade| trade.new.market == market)
            .map(|trade| trade.id.0)
            .collect()
    };
    trades.extend(
        tx.pending
            .trades
            .iter()
            .filter(|trade| trade.new.market == market)
            .map(|trade| trade.id.0),
    );
    trades.sort_unstable();
    trades.dedup();
    let mut moved = 0_u32;
    for trade_id in trades {
        let live = crate::money::CreditIo::live_allocations_for_trade(tx, trade_id).await?;
        for source in live {
            let prefix = match terminal_kind {
                crate::money::AllocationKind::Finalized => "final",
                crate::money::AllocationKind::Reversed => "rev",
                crate::money::AllocationKind::Allocated => {
                    return Err(StoreError::Invariant(
                        "allocated is not a terminal credit fact",
                    ));
                }
            };
            crate::money::CreditIo::insert_allocation(
                tx,
                &crate::money::AllocationFact {
                    id: Uuid::new_v4(),
                    trade_id,
                    lot_id: source.lot_id,
                    split_seq: source.split_seq,
                    amount_micro: source.amount_micro,
                    kind: terminal_kind,
                    source_allocation_id: Some(source.id),
                    idempotency_key: format!("{prefix}:{trade_id}:{}", source.id),
                },
            )
            .await?;
            moved = moved
                .checked_add(1)
                .ok_or(StoreError::Invariant("credit allocation count overflow"))?;
        }
    }
    Ok(moved)
}

#[async_trait]
impl SettlementIo for InMemTx {
    async fn holdings(&mut self, m: MarketId) -> Result<Vec<Holding>, StoreError> {
        let st = self.shared.state.lock();
        let market = st.markets.get(&m.0).ok_or(StoreError::NotFound("market"))?;
        let mut out = Vec::new();
        for p in st.positions.values() {
            let side = if p.outcome == market.yes_outcome {
                Side::Yes
            } else if p.outcome == market.no_outcome {
                Side::No
            } else {
                continue;
            };
            if p.shares.0 == 0 {
                continue;
            }
            let account = st
                .accounts
                .get(&(OwnerRef::User(p.user), Currency::Usdc))
                .ok_or(StoreError::Invariant("position holder without account"))?;
            out.push(Holding {
                account: *account,
                owner: HoldingOwner::User(p.user),
                outcome: p.outcome,
                side,
                shares: p.shares,
            });
        }
        if let Some(pool) = st.pools.get(&m.0) {
            let account = st
                .accounts
                .get(&(OwnerRef::MarketPool(m), Currency::Usdc))
                .ok_or(StoreError::Invariant("pool reserves without pool account"))?;
            out.push(Holding {
                account: *account,
                owner: HoldingOwner::Pool,
                outcome: market.yes_outcome,
                side: Side::Yes,
                shares: pool.yes,
            });
            out.push(Holding {
                account: *account,
                owner: HoldingOwner::Pool,
                outcome: market.no_outcome,
                side: Side::No,
                shares: pool.no,
            });
        }
        out.sort_by_key(|holding| (holding.account.0, matches!(holding.side, Side::No)));
        Ok(out)
    }

    async fn escrow_balance(&mut self, m: MarketId) -> Result<MicroUsd, StoreError> {
        let escrow = {
            let st = self.shared.state.lock();
            *st.accounts
                .get(&(OwnerRef::MarketEscrow(m), Currency::Usdc))
                .ok_or(StoreError::NotFound("escrow account"))?
        };
        // Locked read (codex B4): hold the escrow account's row lock so the
        // settlement input cannot shift under us.
        if !self.account_guards.contains_key(&escrow.0) {
            let handle = self.shared.account_locks.handle(&escrow.0);
            let guard = handle.lock_owned().await;
            self.account_guards.insert(escrow.0, guard);
        }
        self.shared
            .state
            .lock()
            .balances
            .balance(escrow)
            .ok_or(StoreError::Invariant("escrow account without balance"))
    }

    async fn write_outcome_resolution(
        &mut self,
        o: OutcomeId,
        final_bps: u16,
        redemption: MicroUsd,
    ) -> Result<(), StoreError> {
        self.pending
            .outcome_resolutions
            .push((o.0, final_bps, redemption));
        Ok(())
    }

    async fn vote_facts(&mut self, m: MarketId) -> Result<Vec<VoteFact>, StoreError> {
        let st = self.shared.state.lock();
        let mut rows: Vec<&StoredVote> = st.vote_rows.values().filter(|v| v.market == m).collect();
        rows.sort_by_key(|v| v.seq);
        Ok(rows
            .into_iter()
            .map(|v| VoteFact {
                vote_id: v.id.0,
                user: v.user,
                side: v.side,
                crowd_guess_pct: v.crowd_guess_pct,
            })
            .collect())
    }

    async fn save_vote_scores(&mut self, batch: &[VoteScoreUpdate]) -> Result<(), StoreError> {
        let st = self.shared.state.lock();
        if batch
            .iter()
            .any(|row| !st.vote_rows.contains_key(&row.vote_id))
        {
            return Err(StoreError::Invariant("score for unknown vote"));
        }
        let scored_at = OffsetDateTime::now_utc();
        self.pending
            .vote_scores
            .extend(batch.iter().map(|row| (row.vote_id, row.score, scored_at)));
        Ok(())
    }

    async fn voter_ids(&mut self, m: MarketId) -> Result<Vec<UserId>, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .vote_rows
            .values()
            .filter(|vote| vote.market == m)
            .map(|vote| vote.user)
            .collect())
    }

    async fn reps_for_update(
        &mut self,
        users_sorted: &[UserId],
    ) -> Result<Vec<ReputationRow>, StoreError> {
        let mut rows = Vec::with_capacity(users_sorted.len());
        for user in users_sorted {
            self.lock_user(*user).await?;
            let (rep_micro, tier) = self
                .shared
                .state
                .lock()
                .reputation
                .get(&user.0)
                .copied()
                .ok_or(StoreError::NotFound("reputation"))?;
            rows.push(ReputationRow {
                user: *user,
                rep_micro,
                tier,
            });
        }
        Ok(rows)
    }

    async fn save_reps(&mut self, batch: &[ReputationRow]) -> Result<(), StoreError> {
        self.pending.reputation.extend_from_slice(batch);
        Ok(())
    }

    async fn set_market_state(&mut self, m: MarketId, s: MarketState) -> Result<(), StoreError> {
        self.buffer_market_state(m, s)
    }

    async fn open_interest(&mut self, m: MarketId) -> Result<MicroUsd, StoreError> {
        let st = self.shared.state.lock();
        let market = st.markets.get(&m.0).ok_or(StoreError::NotFound("market"))?;
        let mut total = 0_i64;
        for p in st.positions.values() {
            if p.outcome == market.yes_outcome || p.outcome == market.no_outcome {
                total = total
                    .checked_add(p.cost.0)
                    .ok_or(StoreError::Invariant("open interest overflow"))?;
            }
        }
        Ok(MicroUsd(total))
    }

    async fn flag_curator_needed(&mut self, market: MarketId) -> Result<bool, StoreError> {
        let state = self
            .pending
            .market_states
            .iter()
            .rev()
            .find(|(id, _)| *id == market.0)
            .map(|(_, state)| *state)
            .or_else(|| {
                self.shared
                    .state
                    .lock()
                    .markets
                    .get(&market.0)
                    .map(|row| row.state)
            });
        if state != Some(MarketState::Resolving) {
            return Ok(false);
        }
        let pending = self
            .pending
            .curator_flags
            .iter()
            .rev()
            .find(|(id, _)| *id == market.0)
            .map(|(_, value)| *value);
        let current = pending.unwrap_or_else(|| {
            self.shared
                .state
                .lock()
                .markets
                .get(&market.0)
                .and_then(|row| row.curator_flagged_at)
        });
        if current.is_some() {
            return Ok(false);
        }
        self.pending
            .curator_flags
            .push((market.0, Some(OffsetDateTime::now_utc())));
        Ok(true)
    }

    async fn clear_curator_flag(&mut self, market: MarketId) -> Result<(), StoreError> {
        self.pending.curator_flags.push((market.0, None));
        Ok(())
    }

    async fn integrity_report(
        &mut self,
        market: MarketId,
    ) -> Result<Option<IntegrityReportRow>, StoreError> {
        Ok(self
            .pending
            .integrity_reports
            .iter()
            .find(|report| report.market == market)
            .cloned()
            .or_else(|| {
                self.shared
                    .state
                    .lock()
                    .integrity_reports
                    .get(&market.0)
                    .cloned()
            }))
    }

    async fn set_integrity_due_at(
        &mut self,
        market: MarketId,
        due_at: Option<OffsetDateTime>,
    ) -> Result<(), StoreError> {
        if !self.shared.state.lock().markets.contains_key(&market.0) {
            return Err(StoreError::NotFound("market"));
        }
        self.pending.integrity_due_updates.push((market.0, due_at));
        Ok(())
    }

    async fn pool_seeded_micro(&mut self, market: MarketId) -> Result<MicroUsd, StoreError> {
        self.shared
            .state
            .lock()
            .pool_seeded
            .get(&market.0)
            .copied()
            .ok_or(StoreError::NotFound("pool seed"))
    }

    async fn set_lp_result(
        &mut self,
        market: MarketId,
        pnl: MicroUsd,
        settled_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        if !self.shared.state.lock().markets.contains_key(&market.0) {
            return Err(StoreError::NotFound("market"));
        }
        self.pending.lp_results.push((market.0, pnl, settled_at));
        Ok(())
    }

    async fn set_collateral_at_close(
        &mut self,
        market: MarketId,
        collateral: MicroUsd,
    ) -> Result<(), StoreError> {
        if !self.shared.state.lock().markets.contains_key(&market.0) {
            return Err(StoreError::NotFound("market"));
        }
        self.pending
            .collateral_at_close
            .push((market.0, collateral));
        Ok(())
    }
}

#[async_trait]
impl RealizationWriter for InMemTx {
    async fn insert_realization(&mut self, fact: &RealizationFact) -> Result<bool, StoreError> {
        let duplicate = |row: &&RealizationFact| {
            row.ledger_txn == fact.ledger_txn
                && row.user == fact.user
                && row.outcome == fact.outcome
        };
        if self.pending.realizations.iter().any(|row| duplicate(&row))
            || self
                .shared
                .state
                .lock()
                .realizations
                .iter()
                .any(|row| duplicate(&row))
        {
            return Ok(false);
        }
        self.pending.realizations.push(*fact);
        Ok(true)
    }
}

fn subnet_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let octets = ip.octets();
            format!("{}.{}.{}", octets[0], octets[1], octets[2])
        }
        std::net::IpAddr::V6(ip) => {
            let segments = ip.segments();
            format!(
                "{:x}:{:x}:{:x}:{:x}",
                segments[0], segments[1], segments[2], segments[3]
            )
        }
    }
}

#[async_trait]
impl IntegritySweepIo for InMemTx {
    async fn vote_stats(
        &mut self,
        market: MarketId,
        config: IntegritySweepConfig,
    ) -> Result<domain::integrity::VoteStats, StoreError> {
        let st = self.shared.state.lock();
        let row = st
            .markets
            .get(&market.0)
            .ok_or(StoreError::NotFound("market"))?;
        let window = i64::try_from(config.burst_window_secs)
            .map_err(|_| StoreError::Invariant("burst window overflow"))?;
        let horizon = i64::from(config.prior_horizon_windows);
        let age = i64::try_from(config.young_account_age_secs)
            .map_err(|_| StoreError::Invariant("young-account age overflow"))?;
        let last_start = row.closes_at - time::Duration::seconds(window);
        let prior_span = window
            .checked_mul(horizon)
            .ok_or(StoreError::Invariant("prior horizon overflow"))?;
        let prior_start = last_start - time::Duration::seconds(prior_span);
        let votes: Vec<_> = st
            .vote_rows
            .values()
            .filter(|vote| vote.market == market)
            .collect();
        let mut subnets = HashMap::<String, u32>::new();
        let mut devices = HashMap::<String, u32>::new();
        for vote in &votes {
            if let Some(ip) = vote.cast_ip {
                *subnets.entry(subnet_key(ip)).or_default() += 1;
            }
            if let Some(device) = &vote.device_hash {
                *devices.entry(device.clone()).or_default() += 1;
            }
        }
        Ok(domain::integrity::VoteStats {
            total: u32::try_from(votes.len())
                .map_err(|_| StoreError::Invariant("vote count overflow"))?,
            last_window: u32::try_from(
                votes
                    .iter()
                    .filter(|vote| vote.created_at >= last_start && vote.created_at < row.closes_at)
                    .count(),
            )
            .map_err(|_| StoreError::Invariant("vote count overflow"))?,
            prior_windows_total: u32::try_from(
                votes
                    .iter()
                    .filter(|vote| vote.created_at >= prior_start && vote.created_at < last_start)
                    .count(),
            )
            .map_err(|_| StoreError::Invariant("vote count overflow"))?,
            prior_horizon_windows: config.prior_horizon_windows,
            young_accounts: u32::try_from(
                votes
                    .iter()
                    .filter(|vote| {
                        st.user_created_at.get(&vote.user.0).is_some_and(|created| {
                            vote.created_at - *created < time::Duration::seconds(age)
                        })
                    })
                    .count(),
            )
            .map_err(|_| StoreError::Invariant("vote count overflow"))?,
            subnet_observed: subnets.values().sum(),
            top_subnet_count: subnets.values().copied().max().unwrap_or(0),
            device_observed: devices.values().sum(),
            top_device_count: devices.values().copied().max().unwrap_or(0),
        })
    }

    async fn insert_integrity_report(
        &mut self,
        report: &IntegrityReportRow,
    ) -> Result<bool, StoreError> {
        if self
            .pending
            .integrity_reports
            .iter()
            .any(|existing| existing.market == report.market)
            || self
                .shared
                .state
                .lock()
                .integrity_reports
                .contains_key(&report.market.0)
        {
            return Ok(false);
        }
        self.pending.integrity_reports.push(report.clone());
        Ok(true)
    }

    async fn integrity_report(
        &mut self,
        market: MarketId,
    ) -> Result<Option<IntegrityReportRow>, StoreError> {
        SettlementIo::integrity_report(self, market).await
    }
}

#[async_trait]
impl DepositWriter for InMemTx {
    async fn deposit_by_sig(&mut self, chain_sig: &str) -> Result<Option<DepositId>, StoreError> {
        if let Some(d) = self
            .pending
            .deposits
            .iter()
            .find(|d| d.new.chain_sig == chain_sig)
        {
            return Ok(Some(d.id));
        }
        Ok(self
            .shared
            .state
            .lock()
            .deposits_by_sig
            .get(chain_sig)
            .copied()
            .map(DepositId))
    }

    async fn insert_deposit(&mut self, d: NewDeposit) -> Result<DepositId, StoreError> {
        if self.deposit_by_sig(&d.chain_sig).await?.is_some() {
            return Err(StoreError::Conflict("deposit chain signature"));
        }
        let id = DepositId(Uuid::new_v4());
        self.pending.deposits.push(StoredDeposit { id, new: d });
        Ok(id)
    }
}

#[async_trait]
impl MarketWriter for InMemTx {
    async fn insert_market(&mut self, m: NewMarket) -> Result<MarketId, StoreError> {
        let exists = self.pending.markets.iter().any(|row| row.id == m.id)
            || self.shared.state.lock().markets.contains_key(&m.id.0);
        if exists {
            return Err(StoreError::Conflict("market id"));
        }
        self.pending.markets.push(MarketRow {
            id: m.id,
            question: m.slug.clone(),
            slug: m.slug,
            state: MarketState::Draft,
            min_votes_to_resolve: m.min_votes_to_resolve,
            opens_at: OffsetDateTime::UNIX_EPOCH,
            closes_at: m.closes_at,
            tally_hidden_at: m.tally_hidden_at,
            yes_outcome: OutcomeId(Uuid::new_v4()),
            no_outcome: OutcomeId(Uuid::new_v4()),
            curator_flagged_at: None,
            integrity_due_at: None,
            poster_asset_url: None,
            video_asset_url: None,
        });
        Ok(m.id)
    }

    async fn create_pool(
        &mut self,
        m: MarketId,
        fee: BasisPoints,
        seeded: MicroUsd,
    ) -> Result<(), StoreError> {
        let pool = Pool::new(MicroShares(seeded.0), MicroShares(seeded.0), fee)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        {
            // The pool's ledger account is created with its reserves —
            // reserves cannot exist without their owner (codex P1R2 B3).
            let mut st = self.shared.state.lock();
            account_in(&mut st, OwnerRef::MarketPool(m), Currency::Usdc)?;
        }
        self.lock_pool_row(m.0).await;
        self.pending.reserves.push((m.0, pool));
        self.pending.pool_seeded.push((m.0, seeded));
        Ok(())
    }

    async fn set_market_state(&mut self, m: MarketId, s: MarketState) -> Result<(), StoreError> {
        self.buffer_market_state(m, s)
    }
}

#[async_trait]
impl SeedEconomyIo for InMemTx {
    async fn seeded_market_by_key(&mut self, key: &str) -> Result<Option<MarketId>, StoreError> {
        let state = self.shared.state.lock();
        let Some(txn) = state.ledger_txns.values().find(|txn| txn.key == key) else {
            return Ok(None);
        };
        Ok(txn.txn.entries().iter().find_map(|entry| {
            state
                .accounts
                .iter()
                .find_map(|((owner, _currency), account)| match owner {
                    OwnerRef::MarketEscrow(market)
                        if *account == entry.account && entry.amount.0 > 0 =>
                    {
                        Some(*market)
                    }
                    _ => None,
                })
        }))
    }

    async fn lock_lp_kill_switch(&mut self) -> Result<(), StoreError> {
        if self.lp_kill_guard.is_none() {
            self.lp_kill_guard = Some(Arc::clone(&self.shared.lp_kill_lock).lock_owned().await);
        }
        Ok(())
    }

    async fn lp_pnl_sum(
        &mut self,
        since: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Result<MicroUsd, StoreError> {
        let total = self
            .shared
            .state
            .lock()
            .lp_results
            .values()
            .filter(|(_, settled_at)| *settled_at >= since && *settled_at < until)
            .try_fold(0_i64, |sum, (pnl, _)| sum.checked_add(pnl.0))
            .ok_or(StoreError::Invariant("LP PnL overflow"))?;
        Ok(MicroUsd(total))
    }
}

#[async_trait]
impl LifecycleCommandWriter for InMemTx {
    async fn lifecycle_command(
        &mut self,
        key: &str,
    ) -> Result<Option<LifecycleCommand>, StoreError> {
        if let Some((_, command)) = self
            .pending
            .lifecycle_commands
            .iter()
            .find(|(candidate, _)| candidate == key)
        {
            return Ok(Some(*command));
        }
        Ok(self
            .shared
            .state
            .lock()
            .lifecycle_commands
            .get(key)
            .copied())
    }

    async fn record_lifecycle_command(
        &mut self,
        key: &str,
        command: LifecycleCommand,
    ) -> Result<(), StoreError> {
        if self.lifecycle_command(key).await?.is_some() {
            return Err(StoreError::DuplicateKey);
        }
        self.pending
            .lifecycle_commands
            .push((key.to_string(), command));
        Ok(())
    }
}

#[async_trait]
impl UserWriter for InMemTx {
    async fn insert_user(&mut self, handle: &str) -> Result<UserId, StoreError> {
        let id = UserId(Uuid::new_v4());
        self.pending.users.push((id, handle.to_string()));
        self.pending
            .user_created_at
            .push((id, OffsetDateTime::now_utc()));
        self.pending.reputation.push(ReputationRow {
            user: id,
            rep_micro: 0,
            tier: 0,
        });
        Ok(id)
    }

    async fn link_channel(
        &mut self,
        u: UserId,
        channel: &str,
        address: &str,
    ) -> Result<(), StoreError> {
        let key = (channel.to_string(), address.to_string());
        let taken = self.pending.channels.iter().any(|(k, _)| *k == key)
            || self.shared.state.lock().channels.contains_key(&key);
        if taken {
            return Err(StoreError::Conflict("channel link"));
        }
        self.pending.channels.push((key, u));
        Ok(())
    }
}

#[async_trait]
impl MarketQueries for InMemoryStore {
    async fn market_by_ref(&self, r: &str) -> Result<MarketRow, StoreError> {
        let st = self.shared.state.lock();
        if let Ok(id) = Uuid::parse_str(r) {
            return st
                .markets
                .get(&id)
                .cloned()
                .ok_or(StoreError::NotFound("market"));
        }
        st.markets
            .values()
            .find(|m| m.slug == r)
            .cloned()
            .ok_or(StoreError::NotFound("market"))
    }

    async fn list_markets(&self, status: Option<&str>) -> Result<Vec<MarketRow>, StoreError> {
        let st = self.shared.state.lock();
        let mut rows: Vec<MarketRow> = st
            .markets
            .values()
            .filter(|m| status.is_none_or(|s| state_name(m.state) == s))
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(rows)
    }

    async fn pool(&self, m: MarketId) -> Result<PoolRow, StoreError> {
        let st = self.shared.state.lock();
        st.pools
            .get(&m.0)
            .map(|pool| PoolRow {
                market: m,
                pool: *pool,
            })
            .ok_or(StoreError::NotFound("pool"))
    }

    async fn market_snapshot(
        &self,
        m: MarketId,
        now: OffsetDateTime,
    ) -> Result<MarketSnapshot, StoreError> {
        let st = self.shared.state.lock();
        let market = st.markets.get(&m.0).ok_or(StoreError::NotFound("market"))?;
        let pool = st.pools.get(&m.0).ok_or(StoreError::NotFound("pool"))?;
        let tally =
            (market.state == MarketState::Live && now < market.tally_hidden_at).then(|| {
                st.tallies.get(&m.0).copied().unwrap_or(Tally {
                    yes_votes: 0,
                    no_votes: 0,
                })
            });
        Ok(MarketSnapshot {
            market: m,
            state: market.state,
            price_yes_micro: domain::amm::price_micro(pool, Side::Yes),
            price_no_micro: domain::amm::price_micro(pool, Side::No),
            tally,
            closes_at: market.closes_at,
            tally_hidden_at: market.tally_hidden_at,
            under_review: market.state == MarketState::Resolving,
            poster_asset_url: market.poster_asset_url.clone(),
            video_asset_url: market.video_asset_url.clone(),
        })
    }

    async fn due_markets(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DueMarket>, StoreError> {
        let st = self.shared.state.lock();
        if st.due_query_fails {
            return Err(StoreError::Backend(
                "injected due query failure".to_string(),
            ));
        }
        let mut due: Vec<_> = st
            .markets
            .values()
            .filter(|market| match market.state {
                MarketState::Scheduled => now >= market.opens_at,
                MarketState::Live => now >= market.tally_hidden_at,
                MarketState::Closing => now >= market.closes_at,
                MarketState::Closed => market.curator_flagged_at.is_none(),
                MarketState::Resolving => {
                    market.curator_flagged_at.is_none()
                        && market.integrity_due_at.is_some_and(|due| now >= due)
                }
                MarketState::Draft
                | MarketState::Resolved
                | MarketState::Paid
                | MarketState::Voided => false,
            })
            .map(|market| DueMarket {
                market: market.id,
                state: market.state,
            })
            .collect();
        due.sort_by_key(|row| row.market.0);
        due.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(due)
    }

    async fn price_history(
        &self,
        market: MarketId,
        bucket_secs: u32,
        since: OffsetDateTime,
    ) -> Result<Vec<PricePoint>, StoreError> {
        if !(1..=86_400).contains(&bucket_secs) {
            return Err(StoreError::Invariant("invalid chart bucket"));
        }
        let st = self.shared.state.lock();
        let market_row = st
            .markets
            .get(&market.0)
            .ok_or(StoreError::NotFound("market"))?;
        let width = i64::from(bucket_secs);
        let mut buckets: BTreeMap<i64, (i128, i128, u32)> = BTreeMap::new();
        for trade in st
            .trades
            .values()
            .filter(|trade| trade.new.market == market && trade.created_at >= since)
        {
            let raw = i128::from(trade.new.gross.0)
                .checked_mul(1_000_000)
                .ok_or(StoreError::Invariant("chart price overflow"))?
                / i128::from(trade.new.shares.0);
            let yes_price = if trade.new.outcome == market_row.no_outcome {
                1_000_000_i128 - raw
            } else {
                raw
            };
            let bucket = trade.created_at.unix_timestamp().div_euclid(width) * width;
            let entry = buckets.entry(bucket).or_default();
            entry.0 = entry
                .0
                .checked_add(
                    yes_price
                        .checked_mul(i128::from(trade.new.gross.0))
                        .ok_or(StoreError::Invariant("chart weight overflow"))?,
                )
                .ok_or(StoreError::Invariant("chart weight overflow"))?;
            entry.1 = entry
                .1
                .checked_add(i128::from(trade.new.gross.0))
                .ok_or(StoreError::Invariant("chart volume overflow"))?;
            entry.2 = entry.2.saturating_add(1);
        }
        buckets
            .into_iter()
            .map(|(timestamp, (weighted, volume, trades))| {
                Ok(PricePoint {
                    bucket_start: OffsetDateTime::from_unix_timestamp(timestamp)
                        .map_err(|_| StoreError::Invariant("invalid chart timestamp"))?,
                    avg_price_micro: i64::try_from(weighted / volume)
                        .map_err(|_| StoreError::Invariant("chart price overflow"))?,
                    volume_micro: i64::try_from(volume)
                        .map_err(|_| StoreError::Invariant("chart volume overflow"))?,
                    trades,
                })
            })
            .collect()
    }

    async fn tape(&self, market: MarketId, limit: u32) -> Result<Vec<TapeRow>, StoreError> {
        let st = self.shared.state.lock();
        let mut trades: Vec<_> = st
            .trades
            .values()
            .filter(|trade| trade.new.market == market)
            .map(|trade| {
                Ok(TapeRow {
                    handle: st
                        .users
                        .get(&trade.new.user.0)
                        .cloned()
                        .ok_or(StoreError::NotFound("user"))?,
                    side: trade.new.side,
                    action: trade.new.action,
                    collateral_micro: trade.new.gross.0,
                    created_at: trade.created_at,
                    trade_seq: trade.trade_seq,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        trades.sort_by_key(|trade| std::cmp::Reverse(trade.trade_seq));
        trades.truncate(usize::try_from(limit.min(200)).unwrap_or(200));
        Ok(trades)
    }

    async fn positions(&self, u: UserId) -> Result<Vec<PositionView>, StoreError> {
        let st = self.shared.state.lock();
        let mut views = Vec::new();
        for p in st.positions.values().filter(|p| p.user == u) {
            let (market, side) = st
                .markets
                .values()
                .find_map(|m| {
                    if m.yes_outcome == p.outcome {
                        Some((m.id, Side::Yes))
                    } else if m.no_outcome == p.outcome {
                        Some((m.id, Side::No))
                    } else {
                        None
                    }
                })
                .ok_or(StoreError::Invariant("position references unknown outcome"))?;
            views.push(PositionView {
                market,
                outcome: p.outcome,
                side,
                shares: p.shares,
                cost: p.cost,
                realized_pnl: p.realized_pnl,
                rep_micro: st.reputation.get(&u.0).map_or(0, |rep| rep.0),
                tier: st.reputation.get(&u.0).map_or(0, |rep| rep.1),
            });
        }
        views.sort_by_key(|v| v.outcome.0);
        Ok(views)
    }

    async fn user_voted(&self, u: UserId, m: MarketId) -> Result<bool, StoreError> {
        Ok(self.shared.state.lock().votes.contains(&(u.0, m.0)))
    }

    async fn user_by_channel(
        &self,
        channel: &str,
        address: &str,
    ) -> Result<Option<UserId>, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .channels
            .get(&(channel.to_string(), address.to_string()))
            .copied()
            .map(UserId))
    }

    async fn user_rep(&self, u: UserId) -> Result<ReputationRow, StoreError> {
        let (rep_micro, tier) = self
            .shared
            .state
            .lock()
            .reputation
            .get(&u.0)
            .copied()
            .ok_or(StoreError::NotFound("reputation"))?;
        Ok(ReputationRow {
            user: u,
            rep_micro,
            tier,
        })
    }

    async fn last_buy_at(
        &self,
        u: UserId,
        o: OutcomeId,
    ) -> Result<Option<OffsetDateTime>, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .trades
            .values()
            .filter(|trade| {
                trade.new.user == u
                    && trade.new.outcome == o
                    && trade.new.action == crate::model::TradeAction::Buy
            })
            .map(|trade| trade.created_at)
            .max())
    }

    async fn flagged_markets(&self) -> Result<Vec<FlaggedMarketRow>, StoreError> {
        let st = self.shared.state.lock();
        let mut rows: Vec<_> = st
            .markets
            .values()
            .filter(|market| {
                market.state == MarketState::Resolving || market.curator_flagged_at.is_some()
            })
            .map(|market| FlaggedMarketRow {
                market: market.clone(),
                report: st.integrity_reports.get(&market.id.0).cloned(),
            })
            .collect();
        rows.sort_by_key(|row| row.market.id.0);
        Ok(rows)
    }

    async fn top_traders(
        &self,
        since: OffsetDateTime,
        until: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<TraderRow>, StoreError> {
        let st = self.shared.state.lock();
        let mut grouped = HashMap::<Uuid, (i64, u32)>::new();
        for fact in st
            .realizations
            .iter()
            .filter(|fact| fact.created_at >= since && fact.created_at < until)
        {
            let row = grouped.entry(fact.user.0).or_default();
            row.0 = row
                .0
                .checked_add(fact.realized_delta.0)
                .ok_or(StoreError::Invariant("trader PnL overflow"))?;
            row.1 = row.1.saturating_add(1);
        }
        let mut rows = grouped
            .into_iter()
            .map(|(user, (realized_pnl_micro, realizations))| {
                Ok(TraderRow {
                    handle: st
                        .users
                        .get(&user)
                        .cloned()
                        .ok_or(StoreError::NotFound("user"))?,
                    realized_pnl_micro,
                    realizations,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        rows.sort_by(|a, b| {
            b.realized_pnl_micro
                .cmp(&a.realized_pnl_micro)
                .then_with(|| b.realizations.cmp(&a.realizations))
                .then_with(|| a.handle.cmp(&b.handle))
        });
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(rows)
    }

    async fn top_voters(
        &self,
        since: OffsetDateTime,
        until: OffsetDateTime,
        limit: u32,
        min_scored: u32,
    ) -> Result<Vec<VoterRow>, StoreError> {
        let st = self.shared.state.lock();
        let mut grouped = HashMap::<Uuid, (i64, u32)>::new();
        for vote in st.vote_rows.values().filter(|vote| {
            vote.score_created_at
                .is_some_and(|at| at >= since && at < until)
        }) {
            let score = vote
                .score
                .ok_or(StoreError::Invariant("scored vote missing score"))?;
            let row = grouped.entry(vote.user.0).or_default();
            row.0 = row
                .0
                .checked_add(i64::from(score.score_bp))
                .ok_or(StoreError::Invariant("voter score overflow"))?;
            row.1 = row.1.saturating_add(1);
        }
        let mut rows = grouped
            .into_iter()
            .filter(|(_, (_, count))| *count >= min_scored)
            .filter_map(|(user, (sum, count))| {
                let (rep_micro, tier) = st.reputation.get(&user).copied()?;
                let _ = rep_micro;
                (tier >= 1).then(|| {
                    Ok(VoterRow {
                        handle: st
                            .users
                            .get(&user)
                            .cloned()
                            .ok_or(StoreError::NotFound("user"))?,
                        avg_score_bp: (sum + i64::from(count / 2)) / i64::from(count),
                        markets_scored: count,
                        tier,
                    })
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        rows.sort_by(|a, b| {
            b.avg_score_bp
                .cmp(&a.avg_score_bp)
                .then_with(|| b.markets_scored.cmp(&a.markets_scored))
                .then_with(|| a.handle.cmp(&b.handle))
        });
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(rows)
    }

    async fn fee_summary(
        &self,
        since: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Result<Vec<DailyFeeRow>, StoreError> {
        let st = self.shared.state.lock();
        let fees = st.accounts.get(&(OwnerRef::Fees, Currency::Usdc)).copied();
        let mut grouped = BTreeMap::<time::Date, (i64, i64)>::new();
        if let Some(fees) = fees {
            for stored in st
                .ledger_txns
                .values()
                .filter(|txn| txn.created_at >= since && txn.created_at < until)
            {
                let amount = stored
                    .txn
                    .entries()
                    .iter()
                    .filter(|entry| entry.account == fees)
                    .try_fold(0_i64, |sum, entry| sum.checked_add(entry.amount.0))
                    .ok_or(StoreError::Invariant("fee summary overflow"))?;
                let row = grouped.entry(stored.created_at.date()).or_default();
                match stored.txn.kind() {
                    TxnKind::Trade => {
                        row.0 = row
                            .0
                            .checked_add(amount)
                            .ok_or(StoreError::Invariant("fee summary overflow"))?;
                    }
                    TxnKind::Payout => {
                        row.1 = row
                            .1
                            .checked_add(amount)
                            .ok_or(StoreError::Invariant("fee summary overflow"))?;
                    }
                    _ => {}
                }
            }
        }
        grouped
            .into_iter()
            .map(|(day, (trade_fee_micro, payout_dust_micro))| {
                Ok(DailyFeeRow {
                    day,
                    trade_fee_micro,
                    payout_dust_micro,
                    total_micro: trade_fee_micro
                        .checked_add(payout_dust_micro)
                        .ok_or(StoreError::Invariant("fee summary overflow"))?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod phase7_market_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::money::{AllocationFact, AllocationKind};
    use crate::ports::Store;

    fn proposal(now: OffsetDateTime) -> crate::money::MoneyProposal {
        crate::money::MoneyProposal {
            id: Uuid::new_v4(),
            kind: "approve_withdrawal".into(),
            subject_id: Uuid::new_v4(),
            payload_hash: "payload".into(),
            proposer_token_id: "finance-a".into(),
            confirmer_token_id: None,
            reason: "reviewed".into(),
            status: crate::money::ProposalStatus::Pending,
            confirm_not_before: now,
            expires_at: now + time::Duration::minutes(15),
            replay_key: "money-proposal-replay".into(),
            created_at: now,
        }
    }

    fn test_market(store: &InMemoryStore, slug: &str) -> MarketRow {
        let now = OffsetDateTime::UNIX_EPOCH;
        store
            .add_market(
                slug,
                MarketState::Live,
                now + time::Duration::hours(2),
                now + time::Duration::hours(1),
                MicroShares(1_000_000),
                BasisPoints(100),
            )
            .unwrap()
    }

    async fn seed_provisional_allocation(
        store: &InMemoryStore,
        market: &MarketRow,
        suffix: &str,
    ) -> (Uuid, Uuid) {
        let user = store.add_user(
            &format!("allocation-{suffix}"),
            OffsetDateTime::UNIX_EPOCH,
            1,
        );
        let mut tx = store.trade_tx().await.unwrap();
        let trade = tx
            .insert_trade(NewTrade {
                market: market.id,
                user,
                outcome: market.yes_outcome,
                side: Side::Yes,
                action: crate::model::TradeAction::Buy,
                shares: MicroShares(5),
                gross: MicroUsd(10),
                fee: MicroUsd(5),
                avg_price_micro: 2,
                ledger_txn: Uuid::new_v4(),
                run_id: None,
                pending_action_id: None,
            })
            .await
            .unwrap();
        let lot_id = Uuid::new_v4();
        let source_id = Uuid::new_v4();
        tx.insert_allocation(&AllocationFact {
            id: source_id,
            trade_id: trade.id.0,
            lot_id,
            split_seq: 0,
            amount_micro: 5,
            kind: AllocationKind::Allocated,
            source_allocation_id: None,
            idempotency_key: format!("allocated-{suffix}"),
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (lot_id, source_id)
    }

    async fn terminal_fact(store: &InMemoryStore, lot_id: Uuid, source_id: Uuid) -> AllocationFact {
        let mut tx = store.credit_convert_tx().await.unwrap();
        tx.allocations_for_lot(lot_id)
            .await
            .unwrap()
            .into_iter()
            .find(|fact| fact.source_allocation_id == Some(source_id))
            .unwrap()
    }

    #[tokio::test]
    async fn money_proposals_replay_conflict_and_confirm_with_one_cas_winner() {
        let store = InMemoryStore::new();
        let now = OffsetDateTime::UNIX_EPOCH;
        let proposal = proposal(now);
        let mut tx = store.deposit_admission_tx().await.unwrap();

        assert_eq!(
            tx.insert_proposal(proposal.clone()).await.unwrap(),
            proposal
        );
        assert_eq!(
            tx.get_proposal_by_replay(&proposal.replay_key)
                .await
                .unwrap(),
            Some(proposal.clone())
        );
        assert_eq!(tx.get_proposal(proposal.id).await.unwrap(), proposal);

        let duplicate = crate::money::MoneyProposal {
            id: Uuid::new_v4(),
            ..proposal.clone()
        };
        assert_eq!(
            tx.insert_proposal(duplicate).await,
            Err(StoreError::Conflict("money proposal replay key"))
        );
        assert_eq!(
            tx.get_proposal(Uuid::new_v4()).await,
            Err(StoreError::NotFound("money proposal"))
        );

        let confirmed = tx
            .confirm_proposal(proposal.id, "finance-b", now)
            .await
            .unwrap();
        assert_eq!(confirmed.status, crate::money::ProposalStatus::Confirmed);
        assert_eq!(confirmed.confirmer_token_id.as_deref(), Some("finance-b"));
        assert_eq!(
            tx.confirm_proposal(proposal.id, "finance-c", now)
                .await
                .unwrap(),
            confirmed,
            "the second confirmer reads the first CAS winner"
        );
        tx.commit().await.unwrap();

        let mut replay = store.deposit_admission_tx().await.unwrap();
        assert_eq!(
            replay
                .get_proposal_by_replay(&proposal.replay_key)
                .await
                .unwrap(),
            Some(confirmed)
        );
        assert_eq!(
            replay.insert_proposal(proposal).await,
            Err(StoreError::Conflict("money proposal replay key"))
        );
    }

    #[tokio::test]
    async fn fee_allocations_move_once_to_the_requested_terminal_state() {
        let store = InMemoryStore::new();

        let finalized_market = test_market(&store, "finalized-allocation");
        let (finalized_lot, finalized_source) =
            seed_provisional_allocation(&store, &finalized_market, "finalized").await;
        let mut finalize = store.resolve_tx().await.unwrap();
        assert_eq!(
            finalize
                .finalize_market_fee_allocations(finalized_market.id)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            finalize
                .finalize_market_fee_allocations(finalized_market.id)
                .await
                .unwrap(),
            0,
            "a terminal child makes the provisional allocation no longer live"
        );
        finalize.commit().await.unwrap();
        let finalized = terminal_fact(&store, finalized_lot, finalized_source).await;
        assert_eq!(finalized.kind, AllocationKind::Finalized);
        assert_eq!(finalized.amount_micro, 5);
        assert!(finalized.idempotency_key.starts_with("final:"));

        let reversed_market = test_market(&store, "reversed-allocation");
        let (reversed_lot, reversed_source) =
            seed_provisional_allocation(&store, &reversed_market, "reversed").await;
        let mut reverse = store.unwind_tx().await.unwrap();
        assert_eq!(
            reverse
                .reverse_market_fee_allocations(reversed_market.id)
                .await
                .unwrap(),
            1
        );
        reverse.commit().await.unwrap();
        let reversed = terminal_fact(&store, reversed_lot, reversed_source).await;
        assert_eq!(reversed.kind, AllocationKind::Reversed);
        assert_eq!(reversed.amount_micro, 5);
        assert!(reversed.idempotency_key.starts_with("rev:"));

        let invalid_market = test_market(&store, "invalid-terminal-allocation");
        seed_provisional_allocation(&store, &invalid_market, "invalid").await;
        let mut invalid = store.tx();
        assert_eq!(
            move_market_fee_allocations(&mut invalid, invalid_market.id, AllocationKind::Allocated)
                .await,
            Err(StoreError::Invariant(
                "allocated is not a terminal credit fact"
            ))
        );
    }
}
