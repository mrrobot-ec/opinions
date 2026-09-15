//! Server-issued referral codes bound on [`PhoneVerification`] (D32).
//!
//! A grant requires the verified-phone fact. Linked-but-unverified ⇒ grant 0.
//! Two accounts sharing one number: only one becomes grant-eligible.

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::GrantClass;
use crate::error::{AppError, StoreError};
use crate::model::{Event, MarketId, OwnerRef, UserId};
use crate::money::CreditIo;
use crate::ports::Store;

#[derive(Debug, Clone)]
pub struct IssueReferralCodeCmd {
    pub user: UserId,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferralCodeReceipt {
    pub code: String,
    pub replayed: bool,
}

/// Server-side referral-code issuer. Callers never choose the code.
pub struct IssueReferralCode<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store> IssueReferralCode<'_, S> {
    /// # Errors
    /// Store failures or an astronomically unlikely generated-code collision.
    pub async fn execute(
        &self,
        cmd: IssueReferralCodeCmd,
    ) -> Result<ReferralCodeReceipt, AppError> {
        let mut tx = self.store.credit_convert_tx().await?;
        tx.serialize_key(&cmd.idempotency_key).await?;
        tx.serialize_key(&format!("referral-code:{}", cmd.user.0))
            .await?;
        if let Some(code) = tx.referral_code_for_user(cmd.user).await? {
            return Ok(ReferralCodeReceipt {
                code,
                replayed: true,
            });
        }
        let code = format!("OP-{}", Uuid::new_v4().simple());
        tx.insert_referral_code(cmd.user, &code).await?;
        tx.append(Event {
            event_type: "ReferralCodeIssued",
            aggregate_type: "user",
            aggregate_id: cmd.user.0,
            payload: json!({ "code": code.clone() }),
        })
        .await?;
        tx.commit().await?;
        Ok(ReferralCodeReceipt {
            code,
            replayed: false,
        })
    }
}

#[derive(Debug, Clone)]
pub struct BindReferralCodeCmd {
    pub code: String,
    pub referee: UserId,
    pub bind_key: String,
    pub idempotency_key: String,
}

/// Resolve a server-issued code to its immutable owner, then establish the
/// verified-phone-gated bind.
pub struct BindReferralCode<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store> BindReferralCode<'_, S> {
    /// # Errors
    /// Unknown codes, self-referrals, unverified referees, or store failures.
    pub async fn execute(&self, cmd: BindReferralCodeCmd) -> Result<BindReceipt, AppError> {
        let referrer = {
            let mut tx = self.store.credit_convert_tx().await?;
            tx.serialize_key(&cmd.idempotency_key).await?;
            tx.referral_code_owner(&cmd.code)
                .await?
                .ok_or(AppError::ReferralIneligible)?
        };
        BindReferral { store: self.store }
            .execute(BindReferralCmd {
                referrer,
                referee: cmd.referee,
                bind_key: cmd.bind_key,
                idempotency_key: cmd.idempotency_key,
            })
            .await
    }
}

/// Bind a referee to a referrer on one of the bind keys.
#[derive(Debug, Clone)]
pub struct BindReferralCmd {
    pub referrer: UserId,
    pub referee: UserId,
    pub bind_key: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindReceipt {
    pub bind_id: Uuid,
    pub replayed: bool,
}

/// `BindReferral` — persist a referrer≠referee bind.
pub struct BindReferral<'a, S: Store> {
    pub store: &'a S,
}

fn reconcile_bind_indexes(
    by_key: Option<Uuid>,
    by_referee: Option<Uuid>,
) -> Result<Option<Uuid>, StoreError> {
    if by_key.is_some() && by_referee.is_some() && by_key != by_referee {
        return Err(StoreError::Invariant("referral bind indexes disagree"));
    }
    Ok(by_key.or(by_referee))
}

fn require_bind_parties(
    parties: Option<(UserId, UserId, String)>,
) -> Result<(UserId, UserId, String), StoreError> {
    parties.ok_or(StoreError::Invariant(
        "referral bind index points to no row",
    ))
}

fn referral_pair_matches(
    referrer: UserId,
    referee: UserId,
    expected_referrer: UserId,
    expected_referee: UserId,
) -> bool {
    referrer == expected_referrer && referee == expected_referee && referrer != referee
}

fn reject_reciprocal_bind(
    reverse_referrer: UserId,
    reverse_referee: UserId,
    referrer: UserId,
    referee: UserId,
) -> Result<(), AppError> {
    if reverse_referrer == referee && reverse_referee == referrer {
        return Err(AppError::ReferralIneligible);
    }
    Ok(())
}

fn validated_referral_lots(
    referrer_lot: Option<crate::money::CreditLotRow>,
    referee_lot: Option<crate::money::CreditLotRow>,
    referrer: UserId,
    referee: UserId,
) -> Result<(crate::money::CreditLotRow, crate::money::CreditLotRow), StoreError> {
    let referrer_lot = referrer_lot.ok_or(StoreError::Invariant(
        "referral grant transaction has no referrer lot",
    ))?;
    let referee_lot = referee_lot.ok_or(StoreError::Invariant(
        "referral grant transaction has no referee lot",
    ))?;
    if referrer_lot.user != referrer || referee_lot.user != referee {
        return Err(StoreError::Invariant(
            "referral grant lots name the wrong users",
        ));
    }
    Ok((referrer_lot, referee_lot))
}

impl<S: Store> BindReferral<'_, S> {
    /// # Errors
    /// Same-user bind, store failures.
    pub async fn execute(&self, cmd: BindReferralCmd) -> Result<BindReceipt, AppError> {
        if cmd.referrer == cmd.referee {
            return Err(AppError::ReferralIneligible);
        }
        let mut tx = self.store.credit_convert_tx().await?;
        tx.serialize_key(&cmd.idempotency_key).await?;
        let mut pair = [cmd.referrer.0, cmd.referee.0];
        pair.sort_unstable();
        tx.serialize_key(&format!("referral-pair:{}:{}", pair[0], pair[1]))
            .await?;
        tx.serialize_key(&format!("referral-referee:{}", cmd.referee.0))
            .await?;
        let mut users = [cmd.referrer, cmd.referee];
        users.sort_unstable_by_key(|user| user.0);
        for user in users {
            tx.lock_user(user).await?;
        }
        let by_key = tx.referral_bind_by_key(&cmd.bind_key).await?;
        let by_referee = tx.referral_bind_for_referee(cmd.referee).await?;
        if let Some(id) = reconcile_bind_indexes(by_key, by_referee)? {
            let (referrer, referee, bind_key) =
                require_bind_parties(tx.referral_bind_parties(id).await?)?;
            if referrer == cmd.referrer && referee == cmd.referee && bind_key == cmd.bind_key {
                return Ok(BindReceipt {
                    bind_id: id,
                    replayed: true,
                });
            }
            return Err(AppError::ReferralIneligible);
        }
        if let Some(reverse_id) = tx.referral_bind_for_referee(cmd.referrer).await? {
            let (reverse_referrer, reverse_referee, _) =
                require_bind_parties(tx.referral_bind_parties(reverse_id).await?)?;
            reject_reciprocal_bind(reverse_referrer, reverse_referee, cmd.referrer, cmd.referee)?;
        }
        if !tx
            .phone_verified(cmd.referee, OffsetDateTime::now_utc())
            .await?
        {
            return Err(AppError::ReferralIneligible);
        }
        let bind_id = tx
            .insert_referral_bind(cmd.referrer, cmd.referee, &cmd.bind_key)
            .await?;
        tx.append(Event {
            event_type: "ReferralBound",
            aggregate_type: "user",
            aggregate_id: cmd.referee.0,
            payload: json!({
                "referrer": cmd.referrer.0.to_string(),
                "bind_key": cmd.bind_key,
            }),
        })
        .await?;
        tx.commit().await?;
        Ok(BindReceipt {
            bind_id,
            replayed: false,
        })
    }
}

/// Whether a referee is eligible for a referral grant: verified phone AND
/// a bind exists AND referrer ≠ referee.
///
/// # Errors
/// Store failures.
pub async fn referee_grant_eligible(
    tx: &mut (dyn CreditIo + '_),
    referee: UserId,
    now: OffsetDateTime,
) -> Result<bool, AppError> {
    if !tx.phone_verified(referee, now).await? {
        return Ok(false);
    }
    Ok(tx.referral_bind_for_referee(referee).await?.is_some())
}

#[derive(Debug, Clone)]
pub struct GrantReferralCmd {
    pub referee: UserId,
    pub paid_market: MarketId,
    pub granted_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantReferralReceipt {
    pub bind_id: Uuid,
    pub ledger_txn: Uuid,
    pub referrer_lot: Option<Uuid>,
    pub referee_lot: Option<Uuid>,
    pub replayed: bool,
}

/// Atomically mint both referral legs after the referee's first qualifying
/// market is Paid. The bind id is the durable idempotency namespace, so a
/// retry (or another Paid market) can never mint the pair twice.
pub struct GrantReferral<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store> GrantReferral<'_, S> {
    /// # Errors
    /// Ineligible bind/phone/market, mint or reserve cap, or store failures.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(&self, cmd: GrantReferralCmd) -> Result<GrantReferralReceipt, AppError> {
        let mut tx = self.store.credit_convert_tx().await?;
        tx.serialize_key(&format!("referral-grant-referee:{}", cmd.referee.0))
            .await?;
        let bind_id = tx
            .referral_bind_for_referee(cmd.referee)
            .await?
            .ok_or(AppError::ReferralIneligible)?;
        let (referrer, referee, _) =
            require_bind_parties(tx.referral_bind_parties(bind_id).await?)?;
        if !referral_pair_matches(referrer, referee, referrer, cmd.referee) {
            return Err(AppError::ReferralIneligible);
        }
        let grant_key = format!("referral-grant:{bind_id}");
        tx.serialize_key(&grant_key).await?;
        let referrer_lot_key = format!("{grant_key}:referrer");
        let referee_lot_key = format!("{grant_key}:referee");
        if let Some(ledger_txn) = tx.txn_by_key(&grant_key).await? {
            let referrer_lot = tx.lot_by_idempotency(&referrer_lot_key).await?;
            let referee_lot = tx.lot_by_idempotency(&referee_lot_key).await?;
            let (referrer_lot, referee_lot) =
                validated_referral_lots(referrer_lot, referee_lot, referrer, referee)?;
            return Ok(GrantReferralReceipt {
                bind_id,
                ledger_txn,
                referrer_lot: Some(referrer_lot.id),
                referee_lot: Some(referee_lot.id),
                replayed: true,
            });
        }

        let mut users = [referrer, referee];
        users.sort_unstable_by_key(|user| user.0);
        for user in users {
            tx.lock_user(user).await?;
        }
        for user in users {
            tx.convert_then_collect(
                user,
                cmd.granted_at,
                &format!("referral-grant-lock:{bind_id}:{}", user.0),
            )
            .await?;
        }
        let (locked_referrer, locked_referee, _) =
            require_bind_parties(tx.referral_bind_parties(bind_id).await?)?;
        if !referral_pair_matches(locked_referrer, locked_referee, referrer, referee) {
            return Err(AppError::ReferralIneligible);
        }
        if !tx.config_flag("feature_referrals").await?
            || !tx.phone_verified(referee, cmd.granted_at).await?
        {
            return Err(AppError::ReferralIneligible);
        }
        for user in users {
            crate::money::enforcement::enforce_money_mutation(
                tx.as_mut(),
                user,
                crate::money::enforcement::MoneyMutation::CreditGrant,
                0,
                cmd.granted_at,
            )
            .await?;
        }
        let paid = tx
            .first_paid_market_for_user(referee)
            .await?
            .ok_or(AppError::ReferralIneligible)?;
        let minimum = tx
            .config_i64("referral_min_notional_micro")
            .await?
            .unwrap_or(10_000_000);
        if paid.market != cmd.paid_market || paid.notional_micro < minimum {
            return Err(AppError::ReferralIneligible);
        }

        let referrer_amount = tx
            .config_i64("credit_referral_referrer_micro")
            .await?
            .unwrap_or(5_000_000);
        let referee_amount = tx
            .config_i64("credit_referral_referee_micro")
            .await?
            .unwrap_or(5_000_000);
        if referrer_amount <= 0 || referee_amount <= 0 {
            return Err(AppError::ReferralIneligible);
        }
        let total = referrer_amount
            .checked_add(referee_amount)
            .ok_or(AppError::Overflow)?;
        let grant_class = match tx.config_text("bonus_structure").await?.as_deref() {
            None | Some("real_money") => GrantClass::RealMoney,
            Some("sweeps") => GrantClass::Sweeps,
            Some(_) => {
                return Err(crate::error::StoreError::Invariant(
                    "invalid persisted bonus structure",
                )
                .into())
            }
        };

        for user in users {
            tx.lock_credit_lots(user).await?;
        }
        let reserve = tx.bonus_reserve_balance().await?;
        let mint_cap = tx
            .config_i64("bonus_mint_daily_cap_micro")
            .await?
            .unwrap_or(500_000_000);
        let minted = tx
            .bonus_minted_since(cmd.granted_at - time::Duration::hours(24))
            .await?;
        let next_minted = minted.checked_add(total).ok_or(AppError::Overflow)?;
        if mint_cap == 0 || next_minted > mint_cap {
            return Err(AppError::MoneyForbidden("bonus mint cap"));
        }
        if grant_class == GrantClass::RealMoney {
            let promised = tx.remaining_real_money_promise().await?;
            let next_promised = promised.checked_add(total).ok_or(AppError::Overflow)?;
            if reserve < next_promised {
                return Err(AppError::InsufficientBonusReserve {
                    reserve_micro: reserve,
                    promised_micro: next_promised,
                });
            }
        }

        let house = tx
            .account(OwnerRef::House, domain::ledger::Currency::UsdcCredit)
            .await?;
        let referrer_account = tx
            .account(
                OwnerRef::User(referrer),
                domain::ledger::Currency::UsdcCredit,
            )
            .await?;
        let referee_account = tx
            .account(
                OwnerRef::User(referee),
                domain::ledger::Currency::UsdcCredit,
            )
            .await?;
        let ledger_txn = tx
            .ledger_apply(
                domain::ledger::TxnKind::CreditGrant,
                &grant_key,
                &[
                    domain::ledger::Entry {
                        account: house,
                        amount: domain::money::MicroUsd(-total),
                    },
                    domain::ledger::Entry {
                        account: referrer_account,
                        amount: domain::money::MicroUsd(referrer_amount),
                    },
                    domain::ledger::Entry {
                        account: referee_account,
                        amount: domain::money::MicroUsd(referee_amount),
                    },
                ],
            )
            .await?;
        let policy_version = format!("referral-paid:{}", cmd.paid_market.0);
        let referrer_lot = crate::money::CreditLotRow {
            id: Uuid::new_v4(),
            user: referrer,
            source: format!("referral:{bind_id}:referrer"),
            amount_micro: referrer_amount,
            granted_at: cmd.granted_at,
            grant_class,
            policy_version: policy_version.clone(),
            converted_at: None,
        };
        let referee_lot = crate::money::CreditLotRow {
            id: Uuid::new_v4(),
            user: referee,
            source: format!("referral:{bind_id}:referee"),
            amount_micro: referee_amount,
            granted_at: cmd.granted_at,
            grant_class,
            policy_version,
            converted_at: None,
        };
        tx.insert_credit_lot(&referrer_lot).await?;
        tx.remember_lot_idempotency(&referrer_lot_key, referrer_lot.id)
            .await?;
        tx.insert_credit_lot(&referee_lot).await?;
        tx.remember_lot_idempotency(&referee_lot_key, referee_lot.id)
            .await?;
        for (user, lot, amount, leg) in [
            (referrer, referrer_lot.id, referrer_amount, "referrer"),
            (referee, referee_lot.id, referee_amount, "referee"),
        ] {
            tx.append(Event {
                event_type: "CreditGranted",
                aggregate_type: "user",
                aggregate_id: user.0,
                payload: json!({
                    "lot_id": lot.to_string(),
                    "amount_micro": amount,
                    "source": format!("referral:{bind_id}:{leg}"),
                    "grant_class": grant_class.as_str(),
                    "paid_market": cmd.paid_market.0.to_string(),
                }),
            })
            .await?;
        }
        tx.commit().await?;
        Ok(GrantReferralReceipt {
            bind_id,
            ledger_txn,
            referrer_lot: Some(referrer_lot.id),
            referee_lot: Some(referee_lot.id),
            replayed: false,
        })
    }
}

/// Mint one qualifying referral pair inside an already-open Paid-market
/// transaction. The resolve path has acquired both user locks before its
/// market lock; this helper therefore performs no late advisory locking and
/// commits nothing on its own.
///
/// `Ok(false)` means the pair was already granted or is presently ineligible
/// (including cap/reserve refusal). Backend or invariant failures remain
/// errors so the surrounding market transaction cannot partially commit.
///
/// # Errors
/// Store failures or malformed persisted referral/config state.
#[allow(clippy::too_many_lines)]
pub async fn grant_referral_on_paid_in_tx(
    tx: &mut dyn crate::ports::CreditConvertTx,
    referee: UserId,
    paid_market: MarketId,
    granted_at: OffsetDateTime,
) -> Result<bool, StoreError> {
    let Some(bind_id) = tx.referral_bind_for_referee(referee).await? else {
        return Ok(false);
    };
    let (referrer, bound_referee, _) =
        require_bind_parties(tx.referral_bind_parties(bind_id).await?)?;
    if !referral_pair_matches(referrer, bound_referee, referrer, referee) {
        return Ok(false);
    }

    let grant_key = format!("referral-grant:{bind_id}");
    let referrer_lot_key = format!("{grant_key}:referrer");
    let referee_lot_key = format!("{grant_key}:referee");
    if tx.txn_by_key(&grant_key).await?.is_some() {
        let referrer_lot = tx.lot_by_idempotency(&referrer_lot_key).await?;
        let referee_lot = tx.lot_by_idempotency(&referee_lot_key).await?;
        validated_referral_lots(referrer_lot, referee_lot, referrer, referee)?;
        return Ok(false);
    }

    if !tx.config_flag("feature_referrals").await?
        || !tx.phone_verified(referee, granted_at).await?
    {
        return Ok(false);
    }
    let mut users = [referrer, referee];
    users.sort_unstable_by_key(|user| user.0);
    for user in users {
        match crate::money::enforcement::enforce_money_mutation(
            tx,
            user,
            crate::money::enforcement::MoneyMutation::CreditGrant,
            0,
            granted_at,
        )
        .await
        {
            Ok(()) => {}
            Err(AppError::Store(error)) => return Err(error),
            Err(_) => return Ok(false),
        }
        tx.convert_then_collect(
            user,
            granted_at,
            &format!("referral-grant-lock:{bind_id}:{}", user.0),
        )
        .await
        .map_err(referral_operation_error)?;
    }

    let Some(paid) = tx.first_paid_market_for_user(referee).await? else {
        return Ok(false);
    };
    let minimum = tx
        .config_i64("referral_min_notional_micro")
        .await?
        .unwrap_or(10_000_000);
    if paid.market != paid_market || paid.notional_micro < minimum {
        return Ok(false);
    }

    let referrer_amount = tx
        .config_i64("credit_referral_referrer_micro")
        .await?
        .unwrap_or(5_000_000);
    let referee_amount = tx
        .config_i64("credit_referral_referee_micro")
        .await?
        .unwrap_or(5_000_000);
    if referrer_amount <= 0 || referee_amount <= 0 {
        return Ok(false);
    }
    let total = referrer_amount
        .checked_add(referee_amount)
        .ok_or(StoreError::Invariant("referral grant total overflow"))?;
    let grant_class = match tx.config_text("bonus_structure").await?.as_deref() {
        None | Some("real_money") => GrantClass::RealMoney,
        Some("sweeps") => GrantClass::Sweeps,
        Some(_) => return Err(StoreError::Invariant("invalid persisted bonus structure")),
    };

    for user in users {
        tx.lock_credit_lots(user).await?;
    }
    let reserve = tx.bonus_reserve_balance().await?;
    let mint_cap = tx
        .config_i64("bonus_mint_daily_cap_micro")
        .await?
        .unwrap_or(500_000_000);
    let minted = tx
        .bonus_minted_since(granted_at - time::Duration::hours(24))
        .await?;
    let next_minted = minted
        .checked_add(total)
        .ok_or(StoreError::Invariant("referral mint total overflow"))?;
    if mint_cap == 0 || next_minted > mint_cap {
        return Ok(false);
    }
    if grant_class == GrantClass::RealMoney {
        let promised = tx.remaining_real_money_promise().await?;
        let next_promised = promised
            .checked_add(total)
            .ok_or(StoreError::Invariant("referral promise total overflow"))?;
        if reserve < next_promised {
            return Ok(false);
        }
    }

    let house = tx
        .account(OwnerRef::House, domain::ledger::Currency::UsdcCredit)
        .await?;
    let referrer_account = tx
        .account(
            OwnerRef::User(referrer),
            domain::ledger::Currency::UsdcCredit,
        )
        .await?;
    let referee_account = tx
        .account(
            OwnerRef::User(referee),
            domain::ledger::Currency::UsdcCredit,
        )
        .await?;
    tx.ledger_apply(
        domain::ledger::TxnKind::CreditGrant,
        &grant_key,
        &[
            domain::ledger::Entry {
                account: house,
                amount: domain::money::MicroUsd(-total),
            },
            domain::ledger::Entry {
                account: referrer_account,
                amount: domain::money::MicroUsd(referrer_amount),
            },
            domain::ledger::Entry {
                account: referee_account,
                amount: domain::money::MicroUsd(referee_amount),
            },
        ],
    )
    .await?;

    let policy_version = format!("referral-paid:{}", paid_market.0);
    let referrer_lot = crate::money::CreditLotRow {
        id: Uuid::new_v4(),
        user: referrer,
        source: format!("referral:{bind_id}:referrer"),
        amount_micro: referrer_amount,
        granted_at,
        grant_class,
        policy_version: policy_version.clone(),
        converted_at: None,
    };
    let referee_lot = crate::money::CreditLotRow {
        id: Uuid::new_v4(),
        user: referee,
        source: format!("referral:{bind_id}:referee"),
        amount_micro: referee_amount,
        granted_at,
        grant_class,
        policy_version,
        converted_at: None,
    };
    tx.insert_credit_lot(&referrer_lot).await?;
    tx.remember_lot_idempotency(&referrer_lot_key, referrer_lot.id)
        .await?;
    tx.insert_credit_lot(&referee_lot).await?;
    tx.remember_lot_idempotency(&referee_lot_key, referee_lot.id)
        .await?;
    for (user, lot, amount, leg) in [
        (referrer, referrer_lot.id, referrer_amount, "referrer"),
        (referee, referee_lot.id, referee_amount, "referee"),
    ] {
        tx.append(Event {
            event_type: "CreditGranted",
            aggregate_type: "user",
            aggregate_id: user.0,
            payload: json!({
                "lot_id": lot.to_string(),
                "amount_micro": amount,
                "source": format!("referral:{bind_id}:{leg}"),
                "grant_class": grant_class.as_str(),
                "paid_market": paid_market.0.to_string(),
            }),
        })
        .await?;
    }
    Ok(true)
}

fn referral_operation_error(error: AppError) -> StoreError {
    match error {
        AppError::Store(error) => error,
        _ => StoreError::Invariant("referral grant money operation failed"),
    }
}

/// Grant-class stamp used when minting a referral lot.
#[must_use]
pub fn referral_grant_class(bonus_structure: &str) -> GrantClass {
    if bonus_structure == "sweeps" {
        GrantClass::Sweeps
    } else {
        GrantClass::RealMoney
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use crate::fakes::InMemoryStore;

    #[derive(Clone, Copy)]
    struct ReferralClock(OffsetDateTime);

    impl crate::ports::Clock for ReferralClock {
        fn now(&self) -> OffsetDateTime {
            self.0
        }
    }

    async fn qualifying_referral_fixture(
    ) -> (InMemoryStore, UserId, UserId, MarketId, OffsetDateTime) {
        use crate::model::{OwnerRef, TradeAction};
        use crate::place_trade::{PlaceTrade, PlaceTradeCmd};
        use crate::ports::Store;
        use domain::amm::Side;
        use domain::ledger::{Currency, Entry, TxnKind};
        use domain::market::MarketState;
        use domain::money::{BasisPoints, MicroShares, MicroUsd};

        let store = InMemoryStore::new();
        let now = OffsetDateTime::from_unix_timestamp(1_700_100_000).unwrap();
        let referrer = store.add_user("fixture-referrer", OffsetDateTime::UNIX_EPOCH, 0);
        let referee = store.add_user("fixture-referee", OffsetDateTime::UNIX_EPOCH, 0);
        store.set_phone_verified(referee, true);
        BindReferral { store: &store }
            .execute(BindReferralCmd {
                referrer,
                referee,
                bind_key: "fixture:phone".into(),
                idempotency_key: "fixture:bind".into(),
            })
            .await
            .unwrap();
        store.set_money_flag("feature_referrals", true);
        store.set_money_i64("referral_min_notional_micro", 10_000_000);
        store.set_money_i64("credit_referral_referrer_micro", 5_000_000);
        store.set_money_i64("credit_referral_referee_micro", 5_000_000);
        store.set_money_i64("bonus_mint_daily_cap_micro", 100_000_000);
        store.set_money_text("bonus_structure", "real_money");

        let mut seed = store.credit_convert_tx().await.unwrap();
        let external = seed
            .account(OwnerRef::External, Currency::Usdc)
            .await
            .unwrap();
        let reserve = seed
            .account(OwnerRef::BonusReserve, Currency::Usdc)
            .await
            .unwrap();
        seed.ledger_apply(
            TxnKind::Seed,
            "fixture-referral-reserve",
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-20_000_000),
                },
                Entry {
                    account: reserve,
                    amount: MicroUsd(20_000_000),
                },
            ],
        )
        .await
        .unwrap();
        let external_credit = seed
            .account(OwnerRef::External, Currency::UsdcCredit)
            .await
            .unwrap();
        let house_credit = seed
            .account(OwnerRef::House, Currency::UsdcCredit)
            .await
            .unwrap();
        seed.ledger_apply(
            TxnKind::CreditGrant,
            "fixture-referral-house-credit",
            &[
                Entry {
                    account: external_credit,
                    amount: MicroUsd(-40_000_000),
                },
                Entry {
                    account: house_credit,
                    amount: MicroUsd(40_000_000),
                },
            ],
        )
        .await
        .unwrap();
        seed.commit().await.unwrap();

        let market = store
            .add_market(
                "fixture-referral-paid",
                MarketState::Live,
                now + time::Duration::hours(2),
                now + time::Duration::hours(1),
                MicroShares(1_000_000_000),
                BasisPoints(100),
            )
            .unwrap();
        store.record_vote(referee, market.id, Side::Yes);
        store.fund_user(referee, MicroUsd(20_000_000)).unwrap();
        PlaceTrade {
            store: &store,
            clock: &ReferralClock(now),
            rep_config: crate::model::RepConfig::default(),
        }
        .execute(PlaceTradeCmd {
            market: market.id,
            user: referee,
            side: Side::Yes,
            action: TradeAction::Buy,
            amount_micro: 10_000_000,
            idempotency_key: "fixture-referral-trade".into(),
            run_id: None,
            pending_action_id: None,
            expected_config_version: Some(1),
        })
        .await
        .unwrap();
        store.set_market_state(market.id, MarketState::Paid);
        (store, referrer, referee, market.id, now)
    }

    #[test]
    fn unverified_phone_is_never_grant_eligible_by_construction() {
        // The predicate is phone_verified AND bind. Either missing ⇒ 0.
        assert_eq!(referral_grant_class("real_money"), GrantClass::RealMoney);
        assert_eq!(referral_grant_class("sweeps"), GrantClass::Sweeps);
        assert_eq!(referral_grant_class("mystery"), GrantClass::RealMoney);
    }

    #[test]
    fn referral_invariant_validators_are_total() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert_eq!(reconcile_bind_indexes(None, None).unwrap(), None);
        assert_eq!(
            reconcile_bind_indexes(Some(first), None).unwrap(),
            Some(first)
        );
        assert_eq!(
            reconcile_bind_indexes(Some(first), Some(first)).unwrap(),
            Some(first)
        );
        assert_eq!(
            reconcile_bind_indexes(Some(first), Some(second)).unwrap_err(),
            StoreError::Invariant("referral bind indexes disagree")
        );

        let referrer = UserId(Uuid::new_v4());
        let referee = UserId(Uuid::new_v4());
        let parties = (referrer, referee, "phone:bound".to_string());
        assert_eq!(
            require_bind_parties(Some(parties.clone())).unwrap(),
            parties
        );
        assert_eq!(
            require_bind_parties(None).unwrap_err(),
            StoreError::Invariant("referral bind index points to no row")
        );
        assert!(referral_pair_matches(referrer, referee, referrer, referee));
        assert!(!referral_pair_matches(referrer, referee, referee, referrer));
        assert!(!referral_pair_matches(
            referrer, referrer, referrer, referrer
        ));
        assert_eq!(
            reject_reciprocal_bind(referrer, referee, referee, referrer),
            Err(AppError::ReferralIneligible)
        );
        assert!(reject_reciprocal_bind(referrer, referee, referrer, referee).is_ok());

        let referrer_lot = crate::money::CreditLotRow {
            id: Uuid::new_v4(),
            user: referrer,
            source: "referral:referrer".into(),
            amount_micro: 5_000_000,
            granted_at: OffsetDateTime::UNIX_EPOCH,
            grant_class: GrantClass::RealMoney,
            policy_version: "referral-v1".into(),
            converted_at: None,
        };
        let referee_lot = crate::money::CreditLotRow {
            id: Uuid::new_v4(),
            user: referee,
            source: "referral:referee".into(),
            amount_micro: 5_000_000,
            granted_at: OffsetDateTime::UNIX_EPOCH,
            grant_class: GrantClass::RealMoney,
            policy_version: "referral-v1".into(),
            converted_at: None,
        };
        assert!(validated_referral_lots(
            Some(referrer_lot.clone()),
            Some(referee_lot.clone()),
            referrer,
            referee,
        )
        .is_ok());
        assert_eq!(
            validated_referral_lots(None, Some(referee_lot.clone()), referrer, referee)
                .unwrap_err(),
            StoreError::Invariant("referral grant transaction has no referrer lot")
        );
        assert_eq!(
            validated_referral_lots(Some(referrer_lot.clone()), None, referrer, referee)
                .unwrap_err(),
            StoreError::Invariant("referral grant transaction has no referee lot")
        );
        assert_eq!(
            validated_referral_lots(Some(referrer_lot), Some(referee_lot), referee, referrer,)
                .unwrap_err(),
            StoreError::Invariant("referral grant lots name the wrong users")
        );
    }

    #[tokio::test]
    async fn bind_requires_phone_verification_and_exact_replay() {
        let store = InMemoryStore::new();
        let referrer = UserId(Uuid::new_v4());
        let referee = UserId(Uuid::new_v4());
        assert_eq!(
            BindReferral { store: &store }
                .execute(BindReferralCmd {
                    referrer,
                    referee: referrer,
                    bind_key: "self".into(),
                    idempotency_key: "self".into(),
                })
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
        let mut inspect = store.credit_convert_tx().await.unwrap();
        assert!(
            !referee_grant_eligible(&mut *inspect, referee, OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap()
        );
        drop(inspect);
        let command = BindReferralCmd {
            referrer,
            referee,
            bind_key: "phone:verified-hmac".into(),
            idempotency_key: "bind-referral".into(),
        };
        assert_eq!(
            BindReferral { store: &store }
                .execute(command.clone())
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
        store.set_phone_verified(referee, true);
        let first = BindReferral { store: &store }
            .execute(command.clone())
            .await
            .unwrap();
        assert!(!first.replayed);
        let replay = BindReferral { store: &store }
            .execute(command)
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.bind_id, first.bind_id);
        let mut inspect = store.credit_convert_tx().await.unwrap();
        assert!(
            referee_grant_eligible(&mut *inspect, referee, OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn two_referrers_racing_for_one_referee_have_one_winner() {
        let store = InMemoryStore::new();
        let referee = UserId(Uuid::new_v4());
        store.set_phone_verified(referee, true);
        let left = BindReferral { store: &store };
        let right = BindReferral { store: &store };
        let (a, b) = tokio::join!(
            left.execute(BindReferralCmd {
                referrer: UserId(Uuid::new_v4()),
                referee,
                bind_key: "device:left".into(),
                idempotency_key: "bind:left".into(),
            }),
            right.execute(BindReferralCmd {
                referrer: UserId(Uuid::new_v4()),
                referee,
                bind_key: "device:right".into(),
                idempotency_key: "bind:right".into(),
            })
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert!(matches!(
            a.err().or_else(|| b.err()),
            Some(AppError::ReferralIneligible)
        ));
    }

    #[tokio::test]
    async fn bind_waits_for_both_participant_locks_in_uuid_order() {
        use crate::ports::Store;

        let store = InMemoryStore::new();
        let first = store.add_user("bind-lock-first", OffsetDateTime::UNIX_EPOCH, 0);
        let second = store.add_user("bind-lock-second", OffsetDateTime::UNIX_EPOCH, 0);
        let (referrer, referee) = if first.0 < second.0 {
            (first, second)
        } else {
            (second, first)
        };
        store.set_phone_verified(referee, true);

        let mut blocker = store.credit_convert_tx().await.unwrap();
        blocker.lock_user(referrer).await.unwrap();

        let use_case = BindReferral { store: &store };
        let bind = use_case.execute(BindReferralCmd {
            referrer,
            referee,
            bind_key: "bind-lock-order".into(),
            idempotency_key: "bind-lock-order-command".into(),
        });
        tokio::pin!(bind);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut bind)
                .await
                .is_err(),
            "bind must wait for the referrer's class-2 user lock"
        );

        let mut higher_user_probe = store.credit_convert_tx().await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            higher_user_probe.lock_user(referee),
        )
        .await
        .unwrap_or_else(|_| {
            panic!("the higher UUID lock must remain free while bind waits on the lower UUID")
        })
        .unwrap();
        drop(higher_user_probe);
        drop(blocker);

        let receipt = tokio::time::timeout(std::time::Duration::from_secs(1), bind)
            .await
            .unwrap_or_else(|_| {
                panic!("bind should finish after both participant locks are released")
            })
            .unwrap();
        assert!(!receipt.replayed);
    }

    #[tokio::test]
    async fn reciprocal_referral_binds_racing_have_one_winner() {
        let store = InMemoryStore::new();
        let a = UserId(Uuid::new_v4());
        let b = UserId(Uuid::new_v4());
        store.set_phone_verified(a, true);
        store.set_phone_verified(b, true);
        let left = BindReferral { store: &store };
        let right = BindReferral { store: &store };
        let (a_to_b, b_to_a) = tokio::join!(
            left.execute(BindReferralCmd {
                referrer: a,
                referee: b,
                bind_key: "device:a-to-b".into(),
                idempotency_key: "bind:a-to-b".into(),
            }),
            right.execute(BindReferralCmd {
                referrer: b,
                referee: a,
                bind_key: "device:b-to-a".into(),
                idempotency_key: "bind:b-to-a".into(),
            })
        );
        assert_eq!(usize::from(a_to_b.is_ok()) + usize::from(b_to_a.is_ok()), 1);
        assert!(matches!(
            a_to_b.err().or_else(|| b_to_a.err()),
            Some(AppError::ReferralIneligible)
        ));
    }

    #[tokio::test]
    async fn reciprocal_referral_is_rejected_after_the_first_bind_commits() {
        let store = InMemoryStore::new();
        let a = UserId(Uuid::new_v4());
        let b = UserId(Uuid::new_v4());
        store.set_phone_verified(a, true);
        store.set_phone_verified(b, true);
        BindReferral { store: &store }
            .execute(BindReferralCmd {
                referrer: a,
                referee: b,
                bind_key: "sequential:a-to-b".into(),
                idempotency_key: "sequential-bind:a-to-b".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            BindReferral { store: &store }
                .execute(BindReferralCmd {
                    referrer: b,
                    referee: a,
                    bind_key: "sequential:b-to-a".into(),
                    idempotency_key: "sequential-bind:b-to-a".into(),
                })
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
    }

    #[tokio::test]
    async fn codes_are_server_issued_replay_stable_and_bind_verified_referee() {
        let store = InMemoryStore::new();
        let referrer = UserId(Uuid::new_v4());
        let referee = UserId(Uuid::new_v4());
        let issue = IssueReferralCode { store: &store };
        assert_eq!(
            BindReferralCode { store: &store }
                .execute(BindReferralCodeCmd {
                    code: "OP-missing".into(),
                    referee,
                    bind_key: "phone:missing".into(),
                    idempotency_key: "bind-missing-code".into(),
                })
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
        let command = IssueReferralCodeCmd {
            user: referrer,
            idempotency_key: "issue-code".into(),
        };
        let first = issue.execute(command.clone()).await.unwrap();
        assert!(first.code.starts_with("OP-"));
        assert!(!first.replayed);
        let replay = issue.execute(command).await.unwrap();
        assert_eq!(replay.code, first.code);
        assert!(replay.replayed);

        store.set_phone_verified(referee, true);
        let bound = BindReferralCode { store: &store }
            .execute(BindReferralCodeCmd {
                code: first.code,
                referee,
                bind_key: "phone:verified".into(),
                idempotency_key: "bind-code".into(),
            })
            .await
            .unwrap();
        assert!(!bound.replayed);
    }

    #[tokio::test]
    async fn standalone_grant_enforces_every_policy_boundary_before_minting() {
        let (store, _, referee, market, now) = qualifying_referral_fixture().await;
        let command = GrantReferralCmd {
            referee,
            paid_market: market,
            granted_at: now,
        };

        store.set_money_flag("feature_referrals", false);
        assert_eq!(
            GrantReferral { store: &store }
                .execute(command.clone())
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
        store.set_money_flag("feature_referrals", true);
        store.set_phone_verified(referee, false);
        assert_eq!(
            GrantReferral { store: &store }
                .execute(command.clone())
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
        store.set_phone_verified(referee, true);

        let mut wrong_market = command.clone();
        wrong_market.paid_market = MarketId(Uuid::new_v4());
        assert_eq!(
            GrantReferral { store: &store }
                .execute(wrong_market)
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );

        store.set_money_i64("credit_referral_referrer_micro", 0);
        assert_eq!(
            GrantReferral { store: &store }
                .execute(command.clone())
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
        store.set_money_i64("credit_referral_referrer_micro", 5_000_000);

        store.set_money_text("bonus_structure", "invalid");
        assert_eq!(
            GrantReferral { store: &store }
                .execute(command.clone())
                .await
                .unwrap_err(),
            AppError::Store(StoreError::Invariant("invalid persisted bonus structure"))
        );
        store.set_money_text("bonus_structure", "real_money");

        store.set_money_i64("bonus_mint_daily_cap_micro", 0);
        assert_eq!(
            GrantReferral { store: &store }
                .execute(command.clone())
                .await
                .unwrap_err(),
            AppError::MoneyForbidden("bonus mint cap")
        );
        store.set_money_i64("bonus_mint_daily_cap_micro", 100_000_000);

        store.set_money_i64("credit_referral_referrer_micro", 15_000_000);
        store.set_money_i64("credit_referral_referee_micro", 15_000_000);
        assert_eq!(
            GrantReferral { store: &store }
                .execute(command.clone())
                .await
                .unwrap_err(),
            AppError::InsufficientBonusReserve {
                reserve_micro: 20_000_000,
                promised_micro: 30_000_000,
            }
        );

        store.set_money_i64("credit_referral_referrer_micro", 5_000_000);
        store.set_money_i64("credit_referral_referee_micro", 5_000_000);
        store.set_money_text("bonus_structure", "sweeps");
        assert!(
            !GrantReferral { store: &store }
                .execute(command)
                .await
                .unwrap()
                .replayed
        );
    }

    #[tokio::test]
    async fn paid_hook_refuses_ineligible_pairs_caps_and_reserve_without_partial_grants() {
        let empty = InMemoryStore::new();
        let mut empty_tx = empty.credit_convert_tx().await.unwrap();
        assert!(!grant_referral_on_paid_in_tx(
            empty_tx.as_mut(),
            UserId(Uuid::new_v4()),
            MarketId(Uuid::new_v4()),
            OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .unwrap());

        let unqualified = InMemoryStore::new();
        let unqualified_referrer = UserId(Uuid::new_v4());
        let unqualified_referee = UserId(Uuid::new_v4());
        unqualified.set_phone_verified(unqualified_referee, true);
        BindReferral {
            store: &unqualified,
        }
        .execute(BindReferralCmd {
            referrer: unqualified_referrer,
            referee: unqualified_referee,
            bind_key: "unqualified:phone".into(),
            idempotency_key: "unqualified:bind".into(),
        })
        .await
        .unwrap();
        unqualified.set_money_flag("feature_referrals", true);
        let mut unqualified_tx = unqualified.credit_convert_tx().await.unwrap();
        assert!(!grant_referral_on_paid_in_tx(
            unqualified_tx.as_mut(),
            unqualified_referee,
            MarketId(Uuid::new_v4()),
            OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .unwrap());

        let (store, referrer, referee, market, now) = qualifying_referral_fixture().await;
        let command = GrantReferralCmd {
            referee,
            paid_market: market,
            granted_at: now,
        };
        store.corrupt_referral_parties_after(0);
        assert_eq!(
            GrantReferral { store: &store }
                .execute(command.clone())
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
        store.corrupt_referral_parties_after(1);
        assert_eq!(
            GrantReferral { store: &store }
                .execute(command)
                .await
                .unwrap_err(),
            AppError::ReferralIneligible
        );
        store.corrupt_referral_parties_after(0);
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert!(
            !grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap()
        );
        drop(tx);

        store.set_money_flag("feature_referrals", false);
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert!(
            !grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap()
        );
        drop(tx);
        store.set_money_flag("feature_referrals", true);

        store.fail_next_fresh_clear();
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert_eq!(
            grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap_err(),
            StoreError::Unavailable("fake:fresh-clear")
        );
        drop(tx);

        store.set_money_user(referrer, "banned", 2);
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert!(
            !grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap()
        );
        drop(tx);
        store.set_money_user(referrer, "active", 2);

        let mut tx = store.credit_convert_tx().await.unwrap();
        assert!(
            !grant_referral_on_paid_in_tx(tx.as_mut(), referee, MarketId(Uuid::new_v4()), now,)
                .await
                .unwrap()
        );
        drop(tx);

        store.set_money_i64("credit_referral_referrer_micro", 0);
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert!(
            !grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap()
        );
        drop(tx);
        store.set_money_i64("credit_referral_referrer_micro", 5_000_000);

        store.set_money_text("bonus_structure", "invalid");
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert_eq!(
            grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap_err(),
            StoreError::Invariant("invalid persisted bonus structure")
        );
        drop(tx);
        store.set_money_text("bonus_structure", "real_money");

        store.set_money_i64("bonus_mint_daily_cap_micro", 0);
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert!(
            !grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap()
        );
        drop(tx);
        store.set_money_i64("bonus_mint_daily_cap_micro", 100_000_000);

        store.set_money_i64("credit_referral_referrer_micro", 15_000_000);
        store.set_money_i64("credit_referral_referee_micro", 15_000_000);
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert!(
            !grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap()
        );
        drop(tx);

        store.set_money_i64("credit_referral_referrer_micro", 5_000_000);
        store.set_money_i64("credit_referral_referee_micro", 5_000_000);
        store.set_money_text("bonus_structure", "sweeps");
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert!(
            grant_referral_on_paid_in_tx(tx.as_mut(), referee, market, now)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();

        assert_eq!(
            referral_operation_error(AppError::Store(StoreError::Conflict("fixture"))),
            StoreError::Conflict("fixture")
        );
        assert_eq!(
            referral_operation_error(AppError::Overflow),
            StoreError::Invariant("referral grant money operation failed")
        );
    }

    #[tokio::test]
    async fn first_qualifying_paid_market_grants_both_legs_once() {
        use crate::model::{OwnerRef, TradeAction};
        use crate::place_trade::{PlaceTrade, PlaceTradeCmd};
        use crate::ports::{Clock, Store};
        use domain::amm::Side;
        use domain::ledger::{Currency, Entry, TxnKind};
        use domain::market::MarketState;
        use domain::money::{BasisPoints, MicroShares, MicroUsd};

        #[derive(Clone, Copy)]
        struct FixedClock(OffsetDateTime);
        impl Clock for FixedClock {
            fn now(&self) -> OffsetDateTime {
                self.0
            }
        }

        let store = InMemoryStore::new();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let referrer = store.add_user("referrer", OffsetDateTime::UNIX_EPOCH, 0);
        let referee = store.add_user("referee", OffsetDateTime::UNIX_EPOCH, 0);
        store.set_phone_verified(referee, true);
        let bind = BindReferral { store: &store }
            .execute(BindReferralCmd {
                referrer,
                referee,
                bind_key: "phone:referee".into(),
                idempotency_key: "bind-paid".into(),
            })
            .await
            .unwrap();
        let referrer_two = store.add_user("referrer-two", OffsetDateTime::UNIX_EPOCH, 0);
        let referee_two = store.add_user("referee-two", OffsetDateTime::UNIX_EPOCH, 0);
        store.set_phone_verified(referee_two, true);
        BindReferral { store: &store }
            .execute(BindReferralCmd {
                referrer: referrer_two,
                referee: referee_two,
                bind_key: "phone:referee-two".into(),
                idempotency_key: "bind-paid-two".into(),
            })
            .await
            .unwrap();
        store.set_money_flag("feature_referrals", true);
        store.set_money_i64("referral_min_notional_micro", 10_000_000);
        store.set_money_i64("credit_referral_referrer_micro", 5_000_000);
        store.set_money_i64("credit_referral_referee_micro", 5_000_000);
        store.set_money_i64("bonus_mint_daily_cap_micro", 20_000_000);

        let mut seed = store.credit_convert_tx().await.unwrap();
        let external = seed
            .account(OwnerRef::External, Currency::Usdc)
            .await
            .unwrap();
        let reserve = seed
            .account(OwnerRef::BonusReserve, Currency::Usdc)
            .await
            .unwrap();
        seed.ledger_apply(
            TxnKind::Seed,
            "referral-reserve",
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-20_000_000),
                },
                Entry {
                    account: reserve,
                    amount: MicroUsd(20_000_000),
                },
            ],
        )
        .await
        .unwrap();
        let external_credit = seed
            .account(OwnerRef::External, Currency::UsdcCredit)
            .await
            .unwrap();
        let house_credit = seed
            .account(OwnerRef::House, Currency::UsdcCredit)
            .await
            .unwrap();
        seed.ledger_apply(
            TxnKind::CreditGrant,
            "referral-house-credit",
            &[
                Entry {
                    account: external_credit,
                    amount: MicroUsd(-20_000_000),
                },
                Entry {
                    account: house_credit,
                    amount: MicroUsd(20_000_000),
                },
            ],
        )
        .await
        .unwrap();
        seed.commit().await.unwrap();

        let market = store
            .add_market(
                "referral-paid",
                MarketState::Live,
                now + time::Duration::hours(2),
                now + time::Duration::hours(1),
                MicroShares(1_000_000_000),
                BasisPoints(100),
            )
            .unwrap();
        store.record_vote(referee, market.id, Side::Yes);
        store.fund_user(referee, MicroUsd(20_000_000)).unwrap();
        PlaceTrade {
            store: &store,
            clock: &FixedClock(now),
            rep_config: crate::model::RepConfig::default(),
        }
        .execute(PlaceTradeCmd {
            market: market.id,
            user: referee,
            side: Side::Yes,
            action: TradeAction::Buy,
            amount_micro: 10_000_000,
            idempotency_key: "referral-notional".into(),
            run_id: None,
            pending_action_id: None,
            expected_config_version: Some(1),
        })
        .await
        .unwrap();
        store.record_vote(referee_two, market.id, Side::Yes);
        store.fund_user(referee_two, MicroUsd(20_000_000)).unwrap();
        PlaceTrade {
            store: &store,
            clock: &FixedClock(now),
            rep_config: crate::model::RepConfig::default(),
        }
        .execute(PlaceTradeCmd {
            market: market.id,
            user: referee_two,
            side: Side::Yes,
            action: TradeAction::Buy,
            amount_micro: 10_000_000,
            idempotency_key: "referral-notional-two".into(),
            run_id: None,
            pending_action_id: None,
            expected_config_version: Some(1),
        })
        .await
        .unwrap();
        store.set_market_state(market.id, MarketState::Paid);

        let command = GrantReferralCmd {
            referee,
            paid_market: market.id,
            granted_at: now,
        };
        let first = GrantReferral { store: &store }
            .execute(command.clone())
            .await
            .unwrap();
        assert!(!first.replayed);
        assert_eq!(first.bind_id, bind.bind_id);
        assert!(first.referrer_lot.is_some() && first.referee_lot.is_some());

        let mut paid_tx = store.resolve_tx().await.unwrap();
        let mut relevant = paid_tx.referral_relevant_users(market.id).await.unwrap();
        relevant.sort_unstable_by_key(|user| user.0);
        let mut expected = vec![referrer, referee, referrer_two, referee_two];
        expected.sort_unstable_by_key(|user| user.0);
        assert_eq!(relevant, expected);
        for user in &relevant {
            paid_tx.lock_user(*user).await.unwrap();
        }
        assert_eq!(paid_tx.grant_referrals_on_paid(market.id).await.unwrap(), 1);
        paid_tx.commit().await.unwrap();

        let replay = GrantReferral { store: &store }
            .execute(command)
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.referrer_lot, first.referrer_lot);
        assert_eq!(replay.referee_lot, first.referee_lot);
        assert_eq!(
            store.balance_of(OwnerRef::User(referrer), Currency::UsdcCredit),
            Some(MicroUsd(5_000_000))
        );
        assert_eq!(
            store.balance_of(OwnerRef::User(referrer_two), Currency::UsdcCredit),
            Some(MicroUsd(5_000_000))
        );
        assert_eq!(
            store.balance_of(OwnerRef::User(referee_two), Currency::UsdcCredit),
            Some(MicroUsd(5_000_000))
        );
        assert_eq!(
            store.balance_of(OwnerRef::User(referee), Currency::UsdcCredit),
            Some(MicroUsd(5_000_000))
        );
    }
}
