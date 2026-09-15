use application::error::StoreError;
use application::model::{MarketId, MarketRow, OutcomeId, PoolRow, TradeAction};
use domain::amm::{Pool, Side};
use domain::ledger::{Currency, OwnerType, TxnKind};
use domain::money::{BasisPoints, MicroShares};
use sqlx::postgres::PgRow;
use sqlx::Row;

pub(super) fn db_error(error: sqlx::Error) -> StoreError {
    if error.as_database_error().is_some_and(|database| {
        database
            .code()
            .is_some_and(|code| code.starts_with("23") || code == "P0001")
    }) {
        StoreError::Integrity(error.to_string())
    } else {
        StoreError::Backend(error.to_string())
    }
}

pub(super) fn unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.code().as_deref() == Some("23505"))
}

pub(super) fn market_from_row(row: &PgRow) -> Result<MarketRow, StoreError> {
    let state: String = row.try_get("status").map_err(db_error)?;
    let closes_at = row
        .try_get::<Option<time::OffsetDateTime>, _>("closes_at")
        .map_err(db_error)?
        .ok_or(StoreError::Invariant("market has no closes_at"))?;
    let tally_hidden_at = row
        .try_get::<Option<time::OffsetDateTime>, _>("tally_hidden_at")
        .map_err(db_error)?
        .ok_or(StoreError::Invariant("market has no tally_hidden_at"))?;
    Ok(MarketRow {
        id: MarketId(row.try_get("id").map_err(db_error)?),
        slug: row.try_get("slug").map_err(db_error)?,
        question: row.try_get("question").map_err(db_error)?,
        state: market_state(&state)?,
        min_votes_to_resolve: row.try_get("min_votes_to_resolve").map_err(db_error)?,
        opens_at: row
            .try_get::<Option<time::OffsetDateTime>, _>("opens_at")
            .map_err(db_error)?
            .ok_or(StoreError::Invariant("market has no opens_at"))?,
        closes_at,
        tally_hidden_at,
        yes_outcome: OutcomeId(row.try_get("yes_outcome").map_err(db_error)?),
        no_outcome: OutcomeId(row.try_get("no_outcome").map_err(db_error)?),
        curator_flagged_at: row.try_get("curator_flagged_at").map_err(db_error)?,
        integrity_due_at: row.try_get("integrity_due_at").map_err(db_error)?,
        poster_asset_url: row.try_get("poster_asset_url").map_err(db_error)?,
        video_asset_url: row.try_get("video_asset_url").map_err(db_error)?,
    })
}

pub(super) fn pool_from_row(row: &PgRow) -> Result<PoolRow, StoreError> {
    let fee: i32 = row.try_get("fee_bps").map_err(db_error)?;
    let fee = u16::try_from(fee).map_err(|_| StoreError::Invariant("invalid pool fee"))?;
    let pool = Pool::new(
        MicroShares(row.try_get("yes_reserve").map_err(db_error)?),
        MicroShares(row.try_get("no_reserve").map_err(db_error)?),
        BasisPoints(fee),
    )
    .map_err(|_| StoreError::Invariant("invalid pool reserves"))?;
    Ok(PoolRow {
        market: MarketId(row.try_get("market_id").map_err(db_error)?),
        pool,
    })
}

pub(super) fn market_state(value: &str) -> Result<domain::market::MarketState, StoreError> {
    use domain::market::MarketState;
    match value {
        "draft" => Ok(MarketState::Draft),
        "scheduled" => Ok(MarketState::Scheduled),
        "live" => Ok(MarketState::Live),
        "closing" => Ok(MarketState::Closing),
        "closed" => Ok(MarketState::Closed),
        "resolving" => Ok(MarketState::Resolving),
        "resolved" => Ok(MarketState::Resolved),
        "paid" => Ok(MarketState::Paid),
        "voided" => Ok(MarketState::Voided),
        _ => Err(StoreError::Invariant("unknown market status")),
    }
}

pub(super) const fn market_state_name(value: domain::market::MarketState) -> &'static str {
    use domain::market::MarketState;
    match value {
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

pub(super) fn owner_type(value: &str) -> Result<OwnerType, StoreError> {
    match value {
        "user" => Ok(OwnerType::User),
        "pool" => Ok(OwnerType::Pool),
        "fees" => Ok(OwnerType::Fees),
        "house" => Ok(OwnerType::House),
        "escrow" => Ok(OwnerType::Escrow),
        "external" => Ok(OwnerType::External),
        "withheld" => Ok(OwnerType::Withheld),
        "deposit_suspense" => Ok(OwnerType::DepositSuspense),
        "bonus_reserve" => Ok(OwnerType::BonusReserve),
        _ => Err(StoreError::Invariant("unknown account owner type")),
    }
}

pub(super) fn currency(value: &str) -> Result<Currency, StoreError> {
    match value {
        "usdc" => Ok(Currency::Usdc),
        "usdc_credit" => Ok(Currency::UsdcCredit),
        _ => Err(StoreError::Invariant("unknown ledger currency")),
    }
}

pub(super) const fn currency_name(value: Currency) -> &'static str {
    match value {
        Currency::Usdc => "usdc",
        Currency::UsdcCredit => "usdc_credit",
    }
}

pub(super) const fn txn_kind(value: TxnKind) -> &'static str {
    match value {
        TxnKind::Deposit => "deposit",
        TxnKind::Trade => "trade",
        TxnKind::Payout => "payout",
        TxnKind::Withdrawal => "withdrawal",
        TxnKind::Seed => "seed",
        TxnKind::Reversal => "reversal",
        TxnKind::CreditGrant => "credit_grant",
        TxnKind::CreditConvert => "credit_convert",
    }
}

pub(super) const fn action_name(value: TradeAction) -> &'static str {
    match value {
        TradeAction::Buy => "buy",
        TradeAction::Sell => "sell",
    }
}

pub(super) fn action(value: &str) -> Result<TradeAction, StoreError> {
    match value {
        "buy" => Ok(TradeAction::Buy),
        "sell" => Ok(TradeAction::Sell),
        _ => Err(StoreError::Invariant("unknown trade action")),
    }
}

pub(super) fn side_from_idx(value: i32) -> Result<Side, StoreError> {
    match value {
        0 => Ok(Side::Yes),
        1 => Ok(Side::No),
        _ => Err(StoreError::Invariant("unknown outcome index")),
    }
}

pub(super) const MARKET_SELECT: &str = r#"
    select m.id, m.slug, m.question, m.status, m.min_votes_to_resolve, m.opens_at,
           m.closes_at, m.tally_hidden_at, m.curator_flagged_at, m.integrity_due_at,
           m.poster_asset_url, m.video_asset_url,
           yes_outcome.id as yes_outcome, no_outcome.id as no_outcome
      from markets m
      join outcomes yes_outcome on yes_outcome.market_id = m.id and yes_outcome.idx = 0
      join outcomes no_outcome on no_outcome.market_id = m.id and no_outcome.idx = 1
"#;

pub(super) const POOL_SELECT: &str = r#"
    select p.id as pool_id, p.market_id, p.fee_bps,
           yes_reserve.reserve_micro_shares as yes_reserve,
           no_reserve.reserve_micro_shares as no_reserve
      from pools p
      join outcomes yes_outcome on yes_outcome.market_id = p.market_id and yes_outcome.idx = 0
      join outcomes no_outcome on no_outcome.market_id = p.market_id and no_outcome.idx = 1
      join pool_reserves yes_reserve
        on yes_reserve.pool_id = p.id and yes_reserve.outcome_id = yes_outcome.id
       and yes_reserve.market_id = p.market_id
      join pool_reserves no_reserve
        on no_reserve.pool_id = p.id and no_reserve.outcome_id = no_outcome.id
       and no_reserve.market_id = p.market_id
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_errors_remain_distinct_from_integrity_errors() {
        let error = db_error(sqlx::Error::Protocol("offline".to_string()));
        assert!(matches!(error, StoreError::Backend(message) if message.contains("offline")));
        assert!(!unique_violation(&sqlx::Error::Protocol(
            "offline".to_string()
        )));
    }

    #[test]
    fn database_enums_round_trip_and_reject_unknown_values() {
        use domain::market::MarketState;

        let states = [
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
        for (state, name) in states {
            assert_eq!(market_state_name(state), name);
            assert_eq!(market_state(name), Ok(state));
        }
        assert_eq!(
            market_state("mystery"),
            Err(StoreError::Invariant("unknown market status"))
        );

        for (name, expected) in [
            ("user", OwnerType::User),
            ("pool", OwnerType::Pool),
            ("fees", OwnerType::Fees),
            ("house", OwnerType::House),
            ("escrow", OwnerType::Escrow),
            ("external", OwnerType::External),
            ("withheld", OwnerType::Withheld),
            ("deposit_suspense", OwnerType::DepositSuspense),
            ("bonus_reserve", OwnerType::BonusReserve),
        ] {
            assert_eq!(owner_type(name), Ok(expected));
        }
        assert_eq!(
            owner_type("mystery"),
            Err(StoreError::Invariant("unknown account owner type"))
        );

        for currency_value in [Currency::Usdc, Currency::UsdcCredit] {
            let name = currency_name(currency_value);
            assert_eq!(currency(name), Ok(currency_value));
        }
        assert_eq!(
            currency("mystery"),
            Err(StoreError::Invariant("unknown ledger currency"))
        );

        let kinds = [
            (TxnKind::Deposit, "deposit"),
            (TxnKind::Trade, "trade"),
            (TxnKind::Payout, "payout"),
            (TxnKind::Withdrawal, "withdrawal"),
            (TxnKind::Seed, "seed"),
            (TxnKind::Reversal, "reversal"),
            (TxnKind::CreditGrant, "credit_grant"),
            (TxnKind::CreditConvert, "credit_convert"),
        ];
        for (kind, name) in kinds {
            assert_eq!(txn_kind(kind), name);
        }

        for (value, name) in [(TradeAction::Buy, "buy"), (TradeAction::Sell, "sell")] {
            assert_eq!(action_name(value), name);
            assert_eq!(action(name), Ok(value));
        }
        assert_eq!(
            action("mystery"),
            Err(StoreError::Invariant("unknown trade action"))
        );
        assert_eq!(side_from_idx(0), Ok(Side::Yes));
        assert_eq!(side_from_idx(1), Ok(Side::No));
        assert_eq!(
            side_from_idx(2),
            Err(StoreError::Invariant("unknown outcome index"))
        );
    }
}
