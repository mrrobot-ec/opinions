// W3 credit / deposit-machine IO on the shared in-memory transaction.
// This file is `include!`d by fakes/mod.rs — no inner-doc, no re-imports.

use crate::error::AppError;
use crate::model::DepositMachineStatus;
use crate::money::credits::{lot_ready_to_convert, remaining_real_money_promise};
use crate::money::{
    allows_progress, AllocationFact, AmlDirection, AmlFlag, AmlLeg, AmlPolicy, ComplianceDecision,
    ConvertCollectReceipt, CreditConversionTxn, CreditIo, CreditLotRow, DepositMachineRow,
    DepositRefundPayment, ObservedDeposit, ReferralPaidMarket,
};
use crate::ops::receivable_collection::auto_collect;
use crate::ports::ScreenVerdict;

#[async_trait]
impl CreditIo for InMemTx {
    async fn lock_credit_lots(&mut self, user: UserId) -> Result<(), StoreError> {
        self.lock_user(user).await
    }

    async fn insert_credit_lot(&mut self, lot: &CreditLotRow) -> Result<Uuid, StoreError> {
        self.pending.phase7.lots.push(lot.clone());
        Ok(lot.id)
    }

    async fn lots_for_user(&mut self, user: UserId) -> Result<Vec<CreditLotRow>, StoreError> {
        let mut lots: Vec<CreditLotRow> = self
            .shared
            .state
            .lock()
            .phase7
            .lots
            .iter()
            .filter(|lot| lot.user == user)
            .cloned()
            .collect();
        lots.extend(
            self.pending
                .phase7
                .lots
                .iter()
                .filter(|lot| lot.user == user)
                .cloned(),
        );
        lots.sort_by_key(|lot| lot.granted_at);
        Ok(lots)
    }

    async fn lot_by_idempotency(&mut self, key: &str) -> Result<Option<CreditLotRow>, StoreError> {
        if let Some((_, id)) = self
            .pending
            .phase7
            .lot_keys
            .iter()
            .rev()
            .find(|(k, _)| k == key)
        {
            return Ok(self
                .pending
                .phase7
                .lots
                .iter()
                .find(|lot| lot.id == *id)
                .cloned());
        }
        let st = self.shared.state.lock();
        Ok(st
            .phase7
            .lot_keys
            .get(key)
            .and_then(|id| st.phase7.lots.iter().find(|lot| lot.id == *id).cloned()))
    }

    async fn remember_lot_idempotency(&mut self, key: &str, lot: Uuid) -> Result<(), StoreError> {
        self.pending.phase7.lot_keys.push((key.to_string(), lot));
        Ok(())
    }

    async fn mark_lot_converted(
        &mut self,
        lot: Uuid,
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.pending.phase7.converted.push((lot, at));
        Ok(())
    }

    async fn insert_allocation(&mut self, fact: &AllocationFact) -> Result<(), StoreError> {
        let mut existing = self.shared.state.lock().phase7.allocations.clone();
        existing.extend(self.pending.phase7.allocations.iter().cloned());
        if let Some(replay) = existing
            .iter()
            .find(|row| row.idempotency_key == fact.idempotency_key)
        {
            return if replay.trade_id == fact.trade_id
                && replay.lot_id == fact.lot_id
                && replay.split_seq == fact.split_seq
                && replay.amount_micro == fact.amount_micro
                && replay.kind == fact.kind
                && replay.source_allocation_id == fact.source_allocation_id
            {
                Ok(())
            } else {
                Err(StoreError::Conflict("credit allocation idempotency"))
            };
        }
        let valid = match fact.kind {
            crate::money::AllocationKind::Allocated => crate::money::credits::check_allocate(
                &existing,
                fact.trade_id,
                fact.lot_id,
                fact.split_seq,
                fact.amount_micro,
            ),
            crate::money::AllocationKind::Finalized | crate::money::AllocationKind::Reversed => {
                let source = fact.source_allocation_id.ok_or(StoreError::Invariant(
                    "terminal credit allocation missing source",
                ))?;
                crate::money::credits::check_terminal(&existing, source, fact.amount_micro)
            }
        };
        valid.map_err(|_| StoreError::Invariant("credit allocation algebra"))?;
        self.pending.phase7.allocations.push(fact.clone());
        Ok(())
    }

    async fn allocations_for_lot(&mut self, lot: Uuid) -> Result<Vec<AllocationFact>, StoreError> {
        let mut facts: Vec<AllocationFact> = self
            .shared
            .state
            .lock()
            .phase7
            .allocations
            .iter()
            .filter(|f| f.lot_id == lot)
            .cloned()
            .collect();
        facts.extend(
            self.pending
                .phase7
                .allocations
                .iter()
                .filter(|f| f.lot_id == lot)
                .cloned(),
        );
        Ok(facts)
    }

    async fn live_allocations_for_trade(
        &mut self,
        trade: Uuid,
    ) -> Result<Vec<AllocationFact>, StoreError> {
        let mut facts: Vec<AllocationFact> = self
            .shared
            .state
            .lock()
            .phase7
            .allocations
            .iter()
            .filter(|f| f.trade_id == trade)
            .cloned()
            .collect();
        facts.extend(
            self.pending
                .phase7
                .allocations
                .iter()
                .filter(|f| f.trade_id == trade)
                .cloned(),
        );
        let terminal_sources: std::collections::HashSet<Uuid> = facts
            .iter()
            .filter_map(|fact| fact.source_allocation_id)
            .collect();
        facts.retain(|fact| {
            fact.kind == crate::money::AllocationKind::Allocated
                && !terminal_sources.contains(&fact.id)
        });
        Ok(facts)
    }

    async fn bonus_reserve_balance(&mut self) -> Result<i64, StoreError> {
        // Reserve-at-grant is a global capacity decision. Hold the reserve
        // account lock to transaction end even though issuing a lot does not
        // debit it; concurrent grants then observe the preceding lot commit.
        let reserve = self.account(OwnerRef::BonusReserve, Currency::Usdc).await?;
        self.lock_accounts(&[Entry {
            account: reserve,
            amount: MicroUsd(1),
        }])
        .await;
        let st = self.shared.state.lock();
        Ok(st
            .accounts
            .get(&(OwnerRef::BonusReserve, Currency::Usdc))
            .and_then(|id| st.balances.balance(*id).map(|m| m.0))
            .unwrap_or(0))
    }

    async fn remaining_real_money_promise(&mut self) -> Result<i64, StoreError> {
        let st = self.shared.state.lock();
        let mut lots = st.phase7.lots.clone();
        lots.extend(self.pending.phase7.lots.iter().cloned());
        let mut facts = st.phase7.allocations.clone();
        facts.extend(self.pending.phase7.allocations.iter().cloned());
        Ok(remaining_real_money_promise(&lots, &facts))
    }

    async fn bonus_minted_since(&mut self, since: OffsetDateTime) -> Result<i64, StoreError> {
        let committed = self
            .shared
            .state
            .lock()
            .phase7
            .lots
            .iter()
            .filter(|lot| lot.granted_at >= since)
            .try_fold(0_i64, |sum, lot| sum.checked_add(lot.amount_micro))
            .ok_or(StoreError::Invariant("bonus mint total overflow"))?;
        self.pending
            .phase7
            .lots
            .iter()
            .filter(|lot| lot.granted_at >= since)
            .try_fold(committed, |sum, lot| sum.checked_add(lot.amount_micro))
            .ok_or(StoreError::Invariant("bonus mint total overflow"))
    }

    async fn observe_deposit(
        &mut self,
        obs: &ObservedDeposit,
        suspense_tx: Uuid,
        rail_fingerprint: &str,
    ) -> Result<DepositId, StoreError> {
        let id = DepositId(Uuid::new_v4());
        self.pending.phase7.deposits.push(DepositMachineRow {
            id,
            user: obs.user,
            amount: obs.amount,
            chain_sig: obs.chain_sig.clone(),
            source_address: obs.source_address.clone(),
            dest_address: obs.dest_address.clone(),
            mint: obs.mint.clone(),
            rail_fingerprint: rail_fingerprint.to_string(),
            slot: obs.slot,
            status: DepositMachineStatus::ObservedFinalized,
            suspense_tx_id: Some(suspense_tx),
            admit_tx_id: None,
            refund_tx_id: None,
        });
        Ok(id)
    }

    async fn deposit_machine_by_sig(
        &mut self,
        sig: &str,
    ) -> Result<Option<DepositMachineRow>, StoreError> {
        if let Some(row) = self
            .pending
            .phase7
            .deposits
            .iter()
            .rev()
            .find(|row| row.chain_sig == sig)
        {
            return Ok(Some(row.clone()));
        }
        let st = self.shared.state.lock();
        Ok(st
            .phase7
            .deposit_sigs
            .get(sig)
            .and_then(|id| st.phase7.deposits.get(id))
            .cloned())
    }

    async fn deposit_machine_by_id(
        &mut self,
        id: DepositId,
    ) -> Result<Option<DepositMachineRow>, StoreError> {
        if let Some(row) = self
            .pending
            .phase7
            .deposits
            .iter()
            .rev()
            .find(|row| row.id == id)
        {
            return Ok(Some(row.clone()));
        }
        Ok(self.shared.state.lock().phase7.deposits.get(&id.0).cloned())
    }

    async fn cas_deposit_status(
        &mut self,
        id: DepositId,
        from: DepositMachineStatus,
        to: DepositMachineStatus,
    ) -> Result<bool, StoreError> {
        if !crate::credit_deposit::legal_deposit_transition(from, to) {
            return Err(StoreError::Invariant("illegal deposit transition"));
        }
        {
            let mut state = self.shared.state.lock();
            if let Some(successes) = state.phase7.deposit_cas_successes_before_miss {
                if successes == 0 {
                    state.phase7.deposit_cas_successes_before_miss = None;
                    return Ok(false);
                }
                state.phase7.deposit_cas_successes_before_miss = Some(successes - 1);
            }
        }
        let current = self
            .pending
            .phase7
            .status_cas
            .iter()
            .rev()
            .find(|(row_id, _)| *row_id == id.0)
            .map(|(_, status)| *status)
            .or_else(|| {
                self.pending
                    .phase7
                    .deposits
                    .iter()
                    .rev()
                    .find(|row| row.id == id)
                    .map(|row| row.status)
            })
            .or_else(|| {
                self.shared
                    .state
                    .lock()
                    .phase7
                    .deposits
                    .get(&id.0)
                    .map(|row| row.status)
            });
        if current != Some(from) {
            return Ok(false);
        }
        if let Some(row) = self
            .pending
            .phase7
            .deposits
            .iter_mut()
            .rev()
            .find(|row| row.id == id)
        {
            row.status = to;
        }
        self.pending.phase7.status_cas.push((id.0, to));
        Ok(true)
    }

    async fn mark_admitted(&mut self, id: DepositId, admit_tx: Uuid) -> Result<(), StoreError> {
        self.pending.phase7.admits.push((id.0, admit_tx));
        Ok(())
    }

    async fn mark_refunded(&mut self, id: DepositId, refund_tx: Uuid) -> Result<(), StoreError> {
        self.pending.phase7.refunds.push((id.0, refund_tx));
        Ok(())
    }

    async fn insert_refund_payment(
        &mut self,
        deposit: DepositId,
        dest: &str,
        amount: i64,
        rail_fp: &str,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        self.pending
            .phase7
            .outbound_payments
            .push(crate::ports::OutboundPaymentRow {
                id,
                subject: crate::ports::OutboundSubject::DepositRefund,
                subject_id: deposit.0,
                dest: dest.to_string(),
                amount_micro: amount,
                rail_fingerprint: rail_fp.to_string(),
            });
        Ok(id)
    }

    async fn refund_payment_for_deposit(
        &mut self,
        deposit: DepositId,
    ) -> Result<Option<DepositRefundPayment>, StoreError> {
        let pending = self
            .pending
            .phase7
            .outbound_payments
            .iter()
            .rev()
            .find(|row| {
                row.subject == crate::ports::OutboundSubject::DepositRefund
                    && row.subject_id == deposit.0
            });
        let committed = self
            .shared
            .state
            .lock()
            .phase7
            .outbound_payments
            .iter()
            .rev()
            .find(|row| {
                row.subject == crate::ports::OutboundSubject::DepositRefund
                    && row.subject_id == deposit.0
            })
            .cloned();
        Ok(pending
            .cloned()
            .or(committed)
            .map(|row| DepositRefundPayment {
                id: row.id,
                deposit,
                dest: row.dest,
                amount_micro: row.amount_micro,
                rail_fingerprint: row.rail_fingerprint,
            }))
    }

    async fn user_status(&mut self, user: UserId) -> Result<String, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .user_status
            .get(&user.0)
            .cloned()
            .unwrap_or_else(|| "active".into()))
    }

    async fn user_kyc_tier(&mut self, user: UserId) -> Result<i32, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .kyc_tier
            .get(&user.0)
            .copied()
            .unwrap_or(2))
    }

    async fn fresh_clear(
        &mut self,
        user: UserId,
        context: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut st = self.shared.state.lock();
        if std::mem::take(&mut st.phase7.fail_next_fresh_clear) {
            return Err(StoreError::Unavailable("fake:fresh-clear"));
        }
        if let Some((_, _, verdict)) = st
            .phase7
            .screens
            .iter()
            .rev()
            .find(|(u, ctx, _)| *u == user.0 && ctx == context)
        {
            return Ok(allows_progress(verdict, now));
        }
        // Test default: no persisted fact is treated as a fresh Clear so
        // existing fixtures keep trading. PgStore does not do this.
        let _ = now;
        Ok(true)
    }

    async fn self_excluded(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .exclusions
            .get(&user.0)
            .is_some_and(|until| *until > now))
    }

    async fn deposit_limit_micro(&mut self, user: UserId) -> Result<Option<i64>, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .deposit_limits
            .get(&user.0)
            .copied())
    }

    async fn config_flag(&mut self, key: &str) -> Result<bool, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .flags
            .get(key)
            .copied()
            .unwrap_or(false))
    }

    async fn required_config_flag(&mut self, key: &str) -> Result<bool, StoreError> {
        self.shared
            .state
            .lock()
            .phase7
            .flags
            .get(key)
            .copied()
            .ok_or(StoreError::Invariant(
                "required money config missing or malformed",
            ))
    }

    async fn config_i64(&mut self, key: &str) -> Result<Option<i64>, StoreError> {
        Ok(self.shared.state.lock().phase7.ints.get(key).copied())
    }

    async fn config_text(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.shared.state.lock().phase7.strings.get(key).cloned())
    }

    async fn insert_compliance_decision(
        &mut self,
        decision: ComplianceDecision,
    ) -> Result<(), StoreError> {
        self.pending.phase7.decisions.push(decision);
        Ok(())
    }

    async fn phone_verified(
        &mut self,
        user: UserId,
        _now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .phone_verified
            .get(&user.0)
            .copied()
            .unwrap_or(false))
    }

    async fn referral_code_for_user(&mut self, user: UserId) -> Result<Option<String>, StoreError> {
        if let Some((_, code)) = self
            .pending
            .phase7
            .referral_codes
            .iter()
            .find(|(candidate, _)| *candidate == user)
        {
            return Ok(Some(code.clone()));
        }
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .referral_codes
            .get(&user.0)
            .cloned())
    }

    async fn referral_code_owner(&mut self, code: &str) -> Result<Option<UserId>, StoreError> {
        if let Some((user, _)) = self
            .pending
            .phase7
            .referral_codes
            .iter()
            .find(|(_, candidate)| candidate == code)
        {
            return Ok(Some(*user));
        }
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .referral_codes
            .iter()
            .find(|(_, candidate)| candidate.as_str() == code)
            .map(|(user, _)| UserId(*user)))
    }

    async fn insert_referral_code(&mut self, user: UserId, code: &str) -> Result<(), StoreError> {
        if self.referral_code_for_user(user).await?.is_some()
            || self.referral_code_owner(code).await?.is_some()
        {
            return Err(StoreError::Conflict("referral code"));
        }
        self.pending
            .phase7
            .referral_codes
            .push((user, code.to_string()));
        Ok(())
    }

    async fn insert_referral_bind(
        &mut self,
        referrer: UserId,
        referee: UserId,
        bind_key: &str,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        self.pending
            .phase7
            .binds
            .push((id, referrer, referee, bind_key.to_string()));
        Ok(id)
    }

    async fn referral_bind_by_key(&mut self, bind_key: &str) -> Result<Option<Uuid>, StoreError> {
        if let Some((id, _, _, _)) = self
            .pending
            .phase7
            .binds
            .iter()
            .find(|(_, _, _, key)| key == bind_key)
        {
            return Ok(Some(*id));
        }
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .referral_binds
            .iter()
            .find(|(_, _, _, key)| key == bind_key)
            .map(|row| row.0))
    }

    async fn referral_bind_parties(
        &mut self,
        bind: Uuid,
    ) -> Result<Option<(UserId, UserId, String)>, StoreError> {
        let mut parties = self
            .pending
            .phase7
            .binds
            .iter()
            .find(|(id, _, _, _)| *id == bind)
            .map(|(_, referrer, referee, key)| (*referrer, *referee, key.clone()))
            .or_else(|| {
                self.shared
                    .state
                    .lock()
                    .phase7
                    .referral_binds
                    .iter()
                    .find(|(id, _, _, _)| *id == bind)
                    .map(|(_, referrer, referee, key)| (*referrer, *referee, key.clone()))
            });
        let mut state = self.shared.state.lock();
        if let Some(reads) = state.phase7.referral_party_reads_before_corruption {
            if reads == 0 {
                state.phase7.referral_party_reads_before_corruption = None;
                if let Some((_, referee, _)) = parties.as_mut() {
                    parties = Some((*referee, *referee, "corrupt:self-referral".into()));
                }
            } else {
                state.phase7.referral_party_reads_before_corruption = Some(reads - 1);
            }
        }
        Ok(parties)
    }

    async fn referral_bind_for_referee(
        &mut self,
        referee: UserId,
    ) -> Result<Option<Uuid>, StoreError> {
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .referral_binds
            .iter()
            .find(|(_, _, r, _)| *r == referee)
            .map(|row| row.0)
            .or_else(|| {
                self.pending
                    .phase7
                    .binds
                    .iter()
                    .find(|(_, _, r, _)| *r == referee)
                    .map(|row| row.0)
            }))
    }

    async fn first_paid_market_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Option<ReferralPaidMarket>, StoreError> {
        let state = self.shared.state.lock();
        let mut by_market: BTreeMap<Uuid, i64> = BTreeMap::new();
        for trade in state.trades.values().filter(|trade| {
            let pending_state = self
                .pending
                .market_states
                .iter()
                .rev()
                .find(|(market, _)| *market == trade.new.market.0)
                .map(|(_, state)| *state);
            trade.new.user == user
                && pending_state.or_else(|| {
                    state
                        .markets
                        .get(&trade.new.market.0)
                        .map(|market| market.state)
                }) == Some(MarketState::Paid)
        }) {
            let total = by_market.entry(trade.new.market.0).or_default();
            *total = total
                .checked_add(trade.new.gross.0)
                .ok_or(StoreError::Invariant("referral notional overflow"))?;
        }
        let mut markets: Vec<_> = by_market
            .into_iter()
            .map(|(market, notional_micro)| {
                let settled_at = self
                    .pending
                    .lp_results
                    .iter()
                    .rev()
                    .find(|(id, _, _)| *id == market)
                    .map(|(_, _, at)| *at)
                    .or_else(|| state.lp_results.get(&market).map(|(_, at)| *at))
                    .unwrap_or(OffsetDateTime::UNIX_EPOCH);
                (settled_at, market, notional_micro)
            })
            .collect();
        markets.sort_unstable_by_key(|(settled_at, market, _)| (*settled_at, *market));
        Ok(markets
            .first()
            .map(|(_, market, notional_micro)| ReferralPaidMarket {
                market: MarketId(*market),
                notional_micro: *notional_micro,
            }))
    }

    async fn referral_referees_for_paid_market(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<UserId>, StoreError> {
        let state = self.shared.state.lock();
        let traders: BTreeSet<Uuid> = state
            .trades
            .values()
            .filter(|trade| trade.new.market == market)
            .map(|trade| trade.new.user.0)
            .collect();
        let mut referees: Vec<UserId> = state
            .phase7
            .referral_binds
            .iter()
            .map(|(_, _, referee, _)| *referee)
            .filter(|referee| traders.contains(&referee.0))
            .collect();
        referees.sort_unstable_by_key(|user| user.0);
        referees.dedup();
        Ok(referees)
    }

    async fn convert_then_collect(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
        key: &str,
    ) -> Result<ConvertCollectReceipt, AppError> {
        self.lock_credit_lots(user).await?;
        let lots = self.lots_for_user(user).await?;
        let mut converted_micro = 0_i64;
        let mut lots_converted = 0_u32;
        let mut conversion_txns = Vec::new();
        for lot in lots {
            let convert_key = format!("credit-convert:{}", lot.id);
            let pay_key = format!("credit-convert-pay:{}", lot.id);
            let retire_txn = self.txn_by_key(&convert_key).await?;
            let pay_txn = self.txn_by_key(&pay_key).await?;
            match (retire_txn, pay_txn) {
                (Some(retire_txn), Some(pay_txn)) => {
                    conversion_txns.push(CreditConversionTxn {
                        lot_id: lot.id,
                        retire_txn,
                        pay_txn,
                        replayed: true,
                    });
                    continue;
                }
                (None, None) => {}
                _ => {
                    return Err(StoreError::Invariant(
                        "credit conversion has only one ledger transaction",
                    )
                    .into())
                }
            }
            let facts = self.allocations_for_lot(lot.id).await?;
            if !lot_ready_to_convert(&lot, &facts) {
                continue;
            }
            let user_credit = self
                .account(OwnerRef::User(user), Currency::UsdcCredit)
                .await?;
            let house_credit = self.account(OwnerRef::House, Currency::UsdcCredit).await?;
            let reserve = self.account(OwnerRef::BonusReserve, Currency::Usdc).await?;
            let user_cash = self.account(OwnerRef::User(user), Currency::Usdc).await?;
            let retire_txn = self
                .ledger_apply(
                    TxnKind::CreditConvert,
                    &convert_key,
                    &[
                        Entry {
                            account: user_credit,
                            amount: MicroUsd(-lot.amount_micro),
                        },
                        Entry {
                            account: house_credit,
                            amount: MicroUsd(lot.amount_micro),
                        },
                    ],
                )
                .await?;
            let pay_txn = self
                .ledger_apply(
                    TxnKind::CreditConvert,
                    &pay_key,
                    &[
                        Entry {
                            account: reserve,
                            amount: MicroUsd(-lot.amount_micro),
                        },
                        Entry {
                            account: user_cash,
                            amount: MicroUsd(lot.amount_micro),
                        },
                    ],
                )
                .await?;
            self.mark_lot_converted(lot.id, now).await?;
            converted_micro += lot.amount_micro;
            lots_converted += 1;
            conversion_txns.push(CreditConversionTxn {
                lot_id: lot.id,
                retire_txn,
                pay_txn,
                replayed: false,
            });
        }
        let collected_micro =
            auto_collect(self, user, key, &format!("machine:convert:{key}")).await?;
        Ok(ConvertCollectReceipt {
            converted_micro,
            collected_micro,
            lots_converted,
            conversion_txns,
        })
    }
}

#[async_trait]
impl DepositAmlIo for InMemTx {
    async fn evaluate_deposit_aml_candidate(
        &mut self,
        deposit: DepositId,
        user: UserId,
        source_address: &str,
        amount_micro: i64,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let candidate_id = deposit.0;
        let (committed_legs, committed_flags) = {
            let state = self.shared.state.lock();
            (
                state.phase7.aml_legs.clone(),
                state.phase7.aml_flags.clone(),
            )
        };
        let recorded = self
            .pending
            .phase7
            .aml_legs
            .iter()
            .chain(committed_legs.iter())
            .find(|leg| leg.id == candidate_id);
        if recorded.is_some_and(|leg| {
            leg.user != user
                || leg.dest != source_address
                || leg.amount_micro != amount_micro
                || leg.direction != AmlDirection::Deposit
        }) {
            return Err(StoreError::Conflict("deposit aml candidate"));
        }

        if recorded.is_none() {
            let policy = AmlPolicy::seed();
            let mut existing = committed_legs;
            existing.extend(self.pending.phase7.aml_legs.iter().cloned());
            let candidate = AmlLeg {
                id: candidate_id,
                user,
                dest: source_address.to_string(),
                amount_micro,
                at,
                direction: AmlDirection::Deposit,
            };
            let evaluation = crate::money::evaluate_aml(&existing, &candidate, &policy);
            self.pending.phase7.aml_legs.push(candidate.clone());

            for rule in evaluation.flags {
                let has_open_rule = self
                    .pending
                    .phase7
                    .aml_flags
                    .iter()
                    .chain(committed_flags.iter())
                    .any(|flag| flag.user == user && flag.rule == rule && flag.open);
                if !has_open_rule {
                    self.pending.phase7.aml_flags.push(AmlFlag {
                        id: Uuid::new_v4(),
                        user,
                        rule,
                        window_label: format!("{}h", policy.window.whole_hours()),
                        evidence: serde_json::json!({
                            "structuring_user": evaluation.structuring_user,
                            "structuring_dest": evaluation.structuring_dest,
                            "deposit_velocity": evaluation.deposit_velocity,
                            "withdraw_velocity": evaluation.withdraw_velocity,
                            "leg_id": candidate.id,
                        }),
                        open: true,
                        at,
                    });
                }
            }
        }

        Ok(self
            .pending
            .phase7
            .aml_flags
            .iter()
            .chain(committed_flags.iter())
            .any(|flag| flag.user == user && flag.open))
    }
}

#[async_trait]
impl crate::ports::OutboundIo for InMemTx {
    async fn insert_outbound_payment(
        &mut self,
        payment: &crate::ports::OutboundPaymentRow,
    ) -> Result<(), StoreError> {
        let duplicate = self
            .pending
            .phase7
            .outbound_payments
            .iter()
            .chain(
                self.shared
                    .state
                    .lock()
                    .phase7
                    .outbound_payments
                    .clone()
                    .iter(),
            )
            .any(|row| {
                row.id == payment.id
                    || (row.subject == payment.subject && row.subject_id == payment.subject_id)
            });
        if duplicate {
            return Err(StoreError::Conflict("outbound payment"));
        }
        self.pending.phase7.outbound_payments.push(payment.clone());
        Ok(())
    }

    async fn outbound_by_subject(
        &mut self,
        subject: crate::ports::OutboundSubject,
        subject_id: Uuid,
    ) -> Result<Option<crate::ports::OutboundPaymentRow>, StoreError> {
        if let Some(row) = self
            .pending
            .phase7
            .outbound_payments
            .iter()
            .rev()
            .find(|row| row.subject == subject && row.subject_id == subject_id)
        {
            return Ok(Some(row.clone()));
        }
        Ok(self
            .shared
            .state
            .lock()
            .phase7
            .outbound_payments
            .iter()
            .rev()
            .find(|row| row.subject == subject && row.subject_id == subject_id)
            .cloned())
    }

    async fn insert_attempt(
        &mut self,
        attempt: &crate::ports::OutboundAttemptRow,
    ) -> Result<(), StoreError> {
        let mut existing = self.outbound_attempt_view(attempt.payment_id);
        existing.sort_by_key(|row| row.attempt_number);
        if attempt.attempt_number
            != existing
                .last()
                .map_or(1, |row| row.attempt_number.saturating_add(1))
            || attempt.landing_state != crate::ports::LandingState::Prepared
            || attempt.signed_tx_bytes.is_empty()
            || attempt.signature.trim().is_empty()
            || attempt.last_valid_block_height <= 0
            || attempt.lease_expires_at.is_none()
        {
            return Err(StoreError::Invariant("outbound prepared attempt shape"));
        }
        let committed = self.shared.state.lock().phase7.outbound_attempts.clone();
        if self
            .pending
            .phase7
            .outbound_attempts
            .iter()
            .chain(committed.iter())
            .any(|row| row.id == attempt.id || row.signature == attempt.signature)
        {
            return Err(StoreError::Conflict("outbound signature"));
        }
        self.pending.phase7.outbound_attempts.push(attempt.clone());
        Ok(())
    }

    async fn live_attempt(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Option<crate::ports::OutboundAttemptRow>, StoreError> {
        let mut live: Vec<_> = self
            .outbound_attempt_view(payment_id)
            .into_iter()
            .filter(|row| row.landing_state.is_live())
            .collect();
        if let Some(forced) = self
            .shared
            .state
            .lock()
            .phase7
            .force_next_live_attempt_state
            .take()
        {
            let mut row = live
                .pop()
                .ok_or(StoreError::Invariant("no live attempt to corrupt"))?;
            row.landing_state = forced;
            return Ok(Some(row));
        }
        if live.len() > 1 {
            return Err(StoreError::Invariant("multiple live outbound attempts"));
        }
        Ok(live.pop())
    }

    async fn save_attempt(
        &mut self,
        attempt: &crate::ports::OutboundAttemptRow,
    ) -> Result<(), StoreError> {
        let existing = self
            .outbound_attempt_view(attempt.payment_id)
            .into_iter()
            .find(|row| row.id == attempt.id)
            .ok_or(StoreError::NotFound("outbound attempt"))?;
        if existing.payment_id != attempt.payment_id
            || existing.attempt_number != attempt.attempt_number
            || existing.replaces_attempt_id != attempt.replaces_attempt_id
            || existing.signed_tx_bytes != attempt.signed_tx_bytes
            || existing.signature != attempt.signature
            || existing.last_valid_block_height != attempt.last_valid_block_height
        {
            return Err(StoreError::Invariant(
                "outbound attempt immutable fields changed",
            ));
        }
        if !fake_legal_attempt_edge(existing.landing_state, attempt.landing_state) {
            return Err(StoreError::Invariant("illegal outbound attempt transition"));
        }
        self.pending
            .phase7
            .outbound_attempts
            .retain(|row| row.id != attempt.id);
        self.pending.phase7.outbound_attempts.push(attempt.clone());
        Ok(())
    }

    async fn attempts_for(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Vec<crate::ports::OutboundAttemptRow>, StoreError> {
        let mut rows = self.outbound_attempt_view(payment_id);
        rows.sort_by_key(|row| row.attempt_number);
        Ok(rows)
    }
}

impl InMemTx {
    fn outbound_attempt_view(&self, payment_id: Uuid) -> Vec<crate::ports::OutboundAttemptRow> {
        let mut rows: Vec<_> = self
            .shared
            .state
            .lock()
            .phase7
            .outbound_attempts
            .iter()
            .filter(|row| row.payment_id == payment_id)
            .cloned()
            .collect();
        for pending in self
            .pending
            .phase7
            .outbound_attempts
            .iter()
            .filter(|row| row.payment_id == payment_id)
        {
            if let Some(existing) = rows.iter_mut().find(|row| row.id == pending.id) {
                *existing = pending.clone();
            } else {
                rows.push(pending.clone());
            }
        }
        rows
    }
}

fn fake_legal_attempt_edge(
    from: crate::ports::LandingState,
    to: crate::ports::LandingState,
) -> bool {
    if from == to {
        return true;
    }
    let legal = [
        (
            crate::ports::LandingState::Prepared,
            crate::ports::LandingState::Broadcast,
        ),
        (
            crate::ports::LandingState::Prepared,
            crate::ports::LandingState::Unknown,
        ),
        (
            crate::ports::LandingState::Prepared,
            crate::ports::LandingState::Finalized,
        ),
        (
            crate::ports::LandingState::Prepared,
            crate::ports::LandingState::DefinitiveFailed,
        ),
        (
            crate::ports::LandingState::Broadcast,
            crate::ports::LandingState::Unknown,
        ),
        (
            crate::ports::LandingState::Broadcast,
            crate::ports::LandingState::Finalized,
        ),
        (
            crate::ports::LandingState::Unknown,
            crate::ports::LandingState::Finalized,
        ),
        (
            crate::ports::LandingState::Unknown,
            crate::ports::LandingState::DefinitiveFailed,
        ),
    ];
    legal.contains(&(from, to))
}

impl InMemoryStore {
    /// Test observation of committed deposit AML legs.
    #[must_use]
    pub fn deposit_aml_legs(&self) -> Vec<crate::money::AmlLeg> {
        self.shared.state.lock().phase7.aml_legs.clone()
    }

    /// Test fixture: stamp KYC / status / screens for a user.
    pub fn set_money_user(&self, user: UserId, status: &str, kyc_tier: i32) {
        let mut st = self.shared.state.lock();
        st.phase7.user_status.insert(user.0, status.to_string());
        st.phase7.kyc_tier.insert(user.0, kyc_tier);
    }

    /// Test fixture: mark a phone verified (referral grant gate).
    pub fn set_phone_verified(&self, user: UserId, verified: bool) {
        self.shared
            .state
            .lock()
            .phase7
            .phone_verified
            .insert(user.0, verified);
    }

    /// Test fixture: set one Phase-7 boolean money config value.
    pub fn set_money_flag(&self, key: &str, value: bool) {
        self.shared
            .state
            .lock()
            .phase7
            .flags
            .insert(key.to_string(), value);
    }

    /// Test fixture: remove one typed boolean money config value.
    pub fn remove_money_flag(&self, key: &str) {
        self.shared.state.lock().phase7.flags.remove(key);
    }

    /// Test fixture: set one Phase-7 integer money config value.
    pub fn set_money_i64(&self, key: &str, value: i64) {
        self.shared
            .state
            .lock()
            .phase7
            .ints
            .insert(key.to_string(), value);
    }

    /// Test fixture: remove one typed integer money config value.
    pub fn remove_money_i64(&self, key: &str) {
        self.shared.state.lock().phase7.ints.remove(key);
    }

    /// Test fixture: set one Phase-7 string money config value.
    pub fn set_money_text(&self, key: &str, value: &str) {
        self.shared
            .state
            .lock()
            .phase7
            .strings
            .insert(key.to_string(), value.to_string());
    }

    /// Test fixture: remove one typed string money config value.
    pub fn remove_money_text(&self, key: &str) {
        self.shared.state.lock().phase7.strings.remove(key);
    }

    /// Test fixture: persist a screening verdict.
    pub fn set_screen(&self, user: UserId, context: &str, verdict: ScreenVerdict) {
        self.shared
            .state
            .lock()
            .phase7
            .screens
            .push((user.0, context.to_string(), verdict));
    }

    /// Test fixture: make the selected deposit CAS return `false` after this
    /// many successful CAS calls on the store.
    pub fn fail_deposit_cas_after(&self, successful_calls: u32) {
        self.shared
            .state
            .lock()
            .phase7
            .deposit_cas_successes_before_miss = Some(successful_calls);
    }

    /// Test fixture: fail the next compliance-screen freshness read.
    pub fn fail_next_fresh_clear(&self) {
        self.shared.state.lock().phase7.fail_next_fresh_clear = true;
    }

    /// Test fixture: violate `live_attempt` once to prove consumers fail
    /// closed if an adapter returns a terminal attempt.
    pub fn force_next_live_attempt_state(&self, state: crate::ports::LandingState) {
        self.shared
            .state
            .lock()
            .phase7
            .force_next_live_attempt_state = Some(state);
    }

    /// Test fixture: corrupt a bind-party read after the requested number of
    /// successful reads, proving every consumer revalidates the immutable pair.
    pub fn corrupt_referral_parties_after(&self, successful_reads: u32) {
        self.shared
            .state
            .lock()
            .phase7
            .referral_party_reads_before_corruption = Some(successful_reads);
    }
}

#[cfg(test)]
mod credit_fake_semantics_tests {
    use super::*;
    use crate::model::{NewTrade, TradeAction};
    use crate::money::{AllocationKind, GrantClass};
    use crate::ports::{LandingState, OutboundAttemptRow, OutboundSubject, Store};

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn lot(id_value: u128, user: UserId, granted_at: OffsetDateTime) -> CreditLotRow {
        CreditLotRow {
            id: id(id_value),
            user,
            source: "coverage-grant".into(),
            amount_micro: 7,
            granted_at,
            grant_class: GrantClass::RealMoney,
            policy_version: "v1".into(),
            converted_at: None,
        }
    }

    fn allocation(
        id_value: u128,
        trade_id: Uuid,
        lot_id: Uuid,
        kind: AllocationKind,
        source: Option<Uuid>,
        key: &str,
    ) -> AllocationFact {
        AllocationFact {
            id: id(id_value),
            trade_id,
            lot_id,
            split_seq: 0,
            amount_micro: 7,
            kind,
            source_allocation_id: source,
            idempotency_key: key.into(),
        }
    }

    fn prepared_attempt(
        id_value: u128,
        payment_id: Uuid,
        attempt_number: i32,
        replaces_attempt_id: Option<Uuid>,
        signature: &str,
    ) -> OutboundAttemptRow {
        OutboundAttemptRow {
            id: id(id_value),
            payment_id,
            attempt_number,
            replaces_attempt_id,
            signed_tx_bytes: signature.as_bytes().to_vec(),
            signature: signature.into(),
            last_valid_block_height: 100 + i64::from(attempt_number),
            landing_state: LandingState::Prepared,
            lease_expires_at: Some(OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(1)),
            evidence: None,
        }
    }

    #[tokio::test]
    async fn pending_credit_views_filter_users_and_terminal_allocations() {
        let store = InMemoryStore::new();
        let user = UserId(id(1));
        let other = UserId(id(2));
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut tx = store
            .deposit_admission_tx()
            .await
            .unwrap_or_else(|error| panic!("deposit transaction failed: {error}"));
        let own_lot = lot(20, user, now + time::Duration::seconds(1));
        let other_lot = lot(21, other, now - time::Duration::seconds(1));
        assert_eq!(tx.insert_credit_lot(&other_lot).await, Ok(other_lot.id));
        assert_eq!(tx.insert_credit_lot(&own_lot).await, Ok(own_lot.id));
        assert_eq!(
            tx.remember_lot_idempotency("own-lot", own_lot.id).await,
            Ok(())
        );
        assert_eq!(
            tx.remember_lot_idempotency("orphan-lot", id(999)).await,
            Ok(())
        );
        assert_eq!(tx.lots_for_user(user).await, Ok(vec![own_lot.clone()]));
        assert_eq!(
            tx.lot_by_idempotency("own-lot").await,
            Ok(Some(own_lot.clone()))
        );
        assert_eq!(tx.lot_by_idempotency("orphan-lot").await, Ok(None));
        assert_eq!(tx.bonus_minted_since(now).await, Ok(7));

        let trade = id(30);
        let allocated = allocation(
            31,
            trade,
            own_lot.id,
            AllocationKind::Allocated,
            None,
            "allocated",
        );
        assert_eq!(tx.insert_allocation(&allocated).await, Ok(()));
        assert_eq!(
            tx.allocations_for_lot(own_lot.id).await,
            Ok(vec![allocated.clone()])
        );
        assert_eq!(
            tx.live_allocations_for_trade(trade).await,
            Ok(vec![allocated.clone()])
        );
        let finalized = allocation(
            32,
            trade,
            own_lot.id,
            AllocationKind::Finalized,
            Some(allocated.id),
            "finalized",
        );
        assert_eq!(tx.insert_allocation(&finalized).await, Ok(()));
        assert_eq!(tx.live_allocations_for_trade(trade).await, Ok(Vec::new()));
        assert_eq!(tx.commit().await, Ok(()));

        let mut committed = store
            .deposit_admission_tx()
            .await
            .unwrap_or_else(|error| panic!("committed read transaction failed: {error}"));
        assert_eq!(
            committed.live_allocations_for_trade(trade).await,
            Ok(Vec::new())
        );
    }

    #[tokio::test]
    async fn pending_deposit_refund_and_referral_reads_are_transaction_local() {
        let store = InMemoryStore::new();
        let user = UserId(id(1));
        let other = UserId(id(2));
        let now = OffsetDateTime::UNIX_EPOCH;
        store
            .shared
            .state
            .lock()
            .phase7
            .exclusions
            .insert(user.0, now + time::Duration::hours(1));
        let mut tx = store
            .deposit_admission_tx()
            .await
            .unwrap_or_else(|error| panic!("deposit transaction failed: {error}"));
        let observation = ObservedDeposit {
            user: Some(user),
            amount: MicroUsd(17),
            chain_sig: "pending-chain-signature".into(),
            source_address: "source-address".into(),
            dest_address: "destination-address".into(),
            mint: "USDC".into(),
            slot: 44,
        };
        let deposit = tx
            .observe_deposit(&observation, id(40), "rail-fingerprint")
            .await
            .unwrap_or_else(|error| panic!("observation failed: {error}"));
        assert_eq!(
            tx.deposit_machine_by_sig(&observation.chain_sig).await,
            Ok(tx.deposit_machine_by_id(deposit).await.ok().flatten())
        );
        assert_eq!(
            tx.cas_deposit_status(
                deposit,
                DepositMachineStatus::ObservedFinalized,
                DepositMachineStatus::AdmissionPending,
            )
            .await,
            Ok(true)
        );
        assert_eq!(
            tx.deposit_machine_by_id(deposit)
                .await
                .ok()
                .flatten()
                .map(|row| row.status),
            Some(DepositMachineStatus::AdmissionPending)
        );
        let payment = tx
            .insert_refund_payment(deposit, &observation.source_address, 17, "rail-fingerprint")
            .await
            .unwrap_or_else(|error| panic!("refund payment insert failed: {error}"));
        assert_eq!(
            tx.refund_payment_for_deposit(deposit)
                .await
                .ok()
                .flatten()
                .map(|row| (row.id, row.dest, row.amount_micro)),
            Some((payment, observation.source_address.clone(), 17))
        );
        assert_eq!(tx.self_excluded(user, now).await, Ok(true));

        assert_eq!(tx.insert_referral_code(user, "REFCODE").await, Ok(()));
        assert_eq!(
            tx.referral_code_for_user(user).await,
            Ok(Some("REFCODE".into()))
        );
        assert_eq!(tx.referral_code_owner("REFCODE").await, Ok(Some(user)));
        let bind = tx
            .insert_referral_bind(user, other, "pending-bind")
            .await
            .unwrap_or_else(|error| panic!("referral bind failed: {error}"));
        assert_eq!(
            tx.referral_bind_by_key("pending-bind").await,
            Ok(Some(bind))
        );
        assert_eq!(
            tx.referral_bind_parties(bind).await,
            Ok(Some((user, other, "pending-bind".into())))
        );
        assert_eq!(tx.commit().await, Ok(()));
    }

    #[tokio::test]
    async fn first_paid_market_is_ordered_by_settlement_then_identity() {
        let store = InMemoryStore::new();
        let user = UserId(id(1));
        let now = OffsetDateTime::UNIX_EPOCH;
        let early = store
            .add_market(
                "paid-early",
                MarketState::Paid,
                now,
                now,
                MicroShares(1),
                BasisPoints(0),
            )
            .unwrap_or_else(|error| panic!("paid market fixture failed: {error}"));
        let late = store
            .add_market(
                "paid-late",
                MarketState::Paid,
                now,
                now,
                MicroShares(1),
                BasisPoints(0),
            )
            .unwrap_or_else(|error| panic!("paid market fixture failed: {error}"));
        {
            let mut state = store.shared.state.lock();
            for (trade_id, market, gross) in [(id(10), &late, 13), (id(11), &early, 11)] {
                state.trades.insert(
                    trade_id,
                    StoredTrade {
                        id: TradeId(trade_id),
                        new: NewTrade {
                            market: market.id,
                            user,
                            outcome: market.yes_outcome,
                            side: Side::Yes,
                            action: TradeAction::Buy,
                            shares: MicroShares(1),
                            gross: MicroUsd(gross),
                            fee: MicroUsd(0),
                            avg_price_micro: 1,
                            ledger_txn: id(100 + trade_id.as_u128()),
                            run_id: None,
                            pending_action_id: None,
                        },
                        trade_seq: 1,
                        created_at: now,
                    },
                );
            }
            state
                .lp_results
                .insert(early.id.0, (MicroUsd(0), now + time::Duration::seconds(1)));
            state
                .lp_results
                .insert(late.id.0, (MicroUsd(0), now + time::Duration::seconds(2)));
        }
        let mut tx = store
            .deposit_admission_tx()
            .await
            .unwrap_or_else(|error| panic!("referral transaction failed: {error}"));
        assert_eq!(
            tx.first_paid_market_for_user(user).await,
            Ok(Some(ReferralPaidMarket {
                market: early.id,
                notional_micro: 11,
            }))
        );
    }

    #[tokio::test]
    async fn outbound_lineage_reads_pending_updates_and_rejects_duplicate_intents() {
        let store = InMemoryStore::new();
        let payment = crate::ports::OutboundPaymentRow {
            id: id(200),
            subject: OutboundSubject::DepositRefund,
            subject_id: id(201),
            dest: "source-address".into(),
            amount_micro: 19,
            rail_fingerprint: "rail-fingerprint".into(),
        };
        let mut tx = store
            .deposit_admission_tx()
            .await
            .unwrap_or_else(|error| panic!("outbound transaction failed: {error}"));
        assert_eq!(tx.insert_outbound_payment(&payment).await, Ok(()));
        assert_eq!(
            tx.outbound_by_subject(payment.subject, payment.subject_id)
                .await,
            Ok(Some(payment.clone()))
        );
        assert_eq!(
            tx.insert_outbound_payment(&payment).await,
            Err(StoreError::Conflict("outbound payment"))
        );
        let same_subject = crate::ports::OutboundPaymentRow {
            id: id(204),
            ..payment.clone()
        };
        assert_eq!(
            tx.insert_outbound_payment(&same_subject).await,
            Err(StoreError::Conflict("outbound payment"))
        );

        let attempt = prepared_attempt(202, payment.id, 1, None, "signature-1");
        assert_eq!(tx.insert_attempt(&attempt).await, Ok(()));
        assert_eq!(tx.attempts_for(payment.id).await, Ok(vec![attempt.clone()]));
        let mut broadcast = attempt.clone();
        broadcast.landing_state = LandingState::Broadcast;
        assert_eq!(tx.save_attempt(&broadcast).await, Ok(()));
        assert_eq!(tx.save_attempt(&broadcast).await, Ok(()));
        assert!(!fake_legal_attempt_edge(
            LandingState::Finalized,
            LandingState::Broadcast,
        ));
        assert_eq!(
            tx.attempts_for(payment.id).await,
            Ok(vec![broadcast.clone()])
        );
        assert_eq!(tx.commit().await, Ok(()));

        let mut recovery = store
            .deposit_admission_tx()
            .await
            .unwrap_or_else(|error| panic!("outbound recovery transaction failed: {error}"));
        assert_eq!(
            recovery
                .outbound_by_subject(payment.subject, payment.subject_id)
                .await,
            Ok(Some(payment.clone()))
        );
        let mut unknown = broadcast.clone();
        unknown.landing_state = LandingState::Unknown;
        assert_eq!(recovery.save_attempt(&unknown).await, Ok(()));
        let mut failed = unknown;
        failed.landing_state = LandingState::DefinitiveFailed;
        assert_eq!(recovery.save_attempt(&failed).await, Ok(()));
        let replacement = prepared_attempt(203, payment.id, 2, Some(failed.id), "signature-2");
        assert_eq!(recovery.insert_attempt(&replacement).await, Ok(()));
        assert_eq!(
            recovery.live_attempt(payment.id).await,
            Ok(Some(replacement.clone()))
        );
        assert_eq!(
            recovery.attempts_for(payment.id).await,
            Ok(vec![failed, replacement])
        );
        assert_eq!(recovery.commit().await, Ok(()));
    }
}
