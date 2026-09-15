use application::error::StoreError;
use application::model::{LifecycleCommand, MarketId};
use application::ports::LifecycleCommandWriter;
use async_trait::async_trait;
use domain::market::MarketEvent;
use sqlx::Row;

use super::rows::{db_error, market_state, market_state_name};
use super::store::PgTx;

fn event_name(event: MarketEvent) -> &'static str {
    match event {
        MarketEvent::Approve => "approve",
        MarketEvent::GoLive => "go_live",
        MarketEvent::EnterCloseWindow => "enter_close_window",
        MarketEvent::Close => "close",
        MarketEvent::StartIntegritySweep => "start_integrity_sweep",
        MarketEvent::Resolve => "resolve",
        MarketEvent::Pay => "pay",
        MarketEvent::VoidLowParticipation => "void_low_participation",
        MarketEvent::VoidByAdmin => "void_by_admin",
    }
}

fn parse_event(value: &str) -> Result<MarketEvent, StoreError> {
    match value {
        "approve" => Ok(MarketEvent::Approve),
        "go_live" => Ok(MarketEvent::GoLive),
        "enter_close_window" => Ok(MarketEvent::EnterCloseWindow),
        "close" => Ok(MarketEvent::Close),
        "start_integrity_sweep" => Ok(MarketEvent::StartIntegritySweep),
        "resolve" => Ok(MarketEvent::Resolve),
        "pay" => Ok(MarketEvent::Pay),
        "void_low_participation" => Ok(MarketEvent::VoidLowParticipation),
        "void_by_admin" => Ok(MarketEvent::VoidByAdmin),
        _ => Err(StoreError::Invariant("unknown lifecycle event")),
    }
}

#[async_trait]
impl LifecycleCommandWriter for PgTx {
    async fn lifecycle_command(
        &mut self,
        key: &str,
    ) -> Result<Option<LifecycleCommand>, StoreError> {
        let row = sqlx::query(
            "select market_id, event, resulting_state from lifecycle_commands where key = $1",
        )
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            let event: String = row.try_get("event").map_err(db_error)?;
            let state: String = row.try_get("resulting_state").map_err(db_error)?;
            Ok(LifecycleCommand {
                market: MarketId(row.try_get("market_id").map_err(db_error)?),
                event: parse_event(&event)?,
                resulting_state: market_state(&state)?,
            })
        })
        .transpose()
    }

    async fn record_lifecycle_command(
        &mut self,
        key: &str,
        command: LifecycleCommand,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            insert into lifecycle_commands (key, market_id, event, resulting_state)
            values ($1, $2, $3, $4)
            "#,
        )
        .bind(key)
        .bind(command.market.0)
        .bind(event_name(command.event))
        .bind(market_state_name(command.resulting_state))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_events_round_trip_through_the_database_vocabulary() {
        let events = [
            MarketEvent::Approve,
            MarketEvent::GoLive,
            MarketEvent::EnterCloseWindow,
            MarketEvent::Close,
            MarketEvent::StartIntegritySweep,
            MarketEvent::Resolve,
            MarketEvent::Pay,
            MarketEvent::VoidLowParticipation,
            MarketEvent::VoidByAdmin,
        ];
        for event in events {
            assert_eq!(parse_event(event_name(event)), Ok(event));
        }
        assert_eq!(
            parse_event("mystery"),
            Err(StoreError::Invariant("unknown lifecycle event"))
        );
    }
}
