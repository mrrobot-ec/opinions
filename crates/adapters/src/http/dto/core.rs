// ---------------------------------------------------------------------------
// Shared enums / primitives on the wire
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SideDto {
    Yes,
    No,
}

impl From<domain::amm::Side> for SideDto {
    fn from(s: domain::amm::Side) -> Self {
        match s {
            domain::amm::Side::Yes => Self::Yes,
            domain::amm::Side::No => Self::No,
        }
    }
}

impl From<SideDto> for domain::amm::Side {
    fn from(s: SideDto) -> Self {
        match s {
            SideDto::Yes => Self::Yes,
            SideDto::No => Self::No,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TradeActionDto {
    Buy,
    Sell,
}

impl From<TradeAction> for TradeActionDto {
    fn from(a: TradeAction) -> Self {
        match a {
            TradeAction::Buy => Self::Buy,
            TradeAction::Sell => Self::Sell,
        }
    }
}

impl From<TradeActionDto> for TradeAction {
    fn from(a: TradeActionDto) -> Self {
        match a {
            TradeActionDto::Buy => Self::Buy,
            TradeActionDto::Sell => Self::Sell,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarketStateDto {
    Draft,
    Scheduled,
    Live,
    Closing,
    Closed,
    Resolving,
    Resolved,
    Paid,
    Voided,
}

impl From<domain::market::MarketState> for MarketStateDto {
    fn from(s: domain::market::MarketState) -> Self {
        use domain::market::MarketState::*;
        match s {
            Draft => Self::Draft,
            Scheduled => Self::Scheduled,
            Live => Self::Live,
            Closing => Self::Closing,
            Closed => Self::Closed,
            Resolving => Self::Resolving,
            Resolved => Self::Resolved,
            Paid => Self::Paid,
            Voided => Self::Voided,
        }
    }
}

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AdvanceMarketRequest {
    /// Lifecycle event name, e.g. `go_live`, `enter_close_window`, `close`.
    pub event: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MarketAdvancedDto {
    pub market_id: Uuid,
    pub state: MarketStateDto,
    pub from_state: MarketStateDto,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ResolveMarketDto {
    pub market_id: Uuid,
    pub state: MarketStateDto,
    pub actual_yes_bps: Option<u16>,
    pub voided: bool,
    pub replayed: bool,
    pub ledger_txn: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CuratorDecisionDto {
    ResolveAtTally,
    Void,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ResolveMarketRequest {
    pub decision: CuratorDecisionDto,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrityReportDto {
    pub checks: serde_json::Value,
    pub verdict: String,
    pub created_at: time::OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FlaggedMarketDto {
    pub market_id: Uuid,
    pub slug: String,
    pub question: String,
    pub state: MarketStateDto,
    pub integrity_due_at: Option<time::OffsetDateTime>,
    pub curator_flagged_at: Option<time::OffsetDateTime>,
    pub report: Option<IntegrityReportDto>,
}

impl From<application::model::FlaggedMarketRow> for FlaggedMarketDto {
    fn from(row: application::model::FlaggedMarketRow) -> Self {
        Self {
            market_id: row.market.id.0,
            slug: row.market.slug,
            question: row.market.question,
            state: row.market.state.into(),
            integrity_due_at: row.market.integrity_due_at,
            curator_flagged_at: row.market.curator_flagged_at,
            report: row.report.map(|report| IntegrityReportDto {
                checks: report.checks,
                verdict: match report.verdict {
                    domain::integrity::Verdict::Pass => "pass",
                    domain::integrity::Verdict::Flag => "flag",
                }
                .to_string(),
                created_at: report.created_at,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use application::model::{MarketId, OutcomeId, PositionView};
    use domain::amm::{Pool, Side};
    use domain::market::MarketState;
    use domain::money::{BasisPoints, MicroShares, MicroUsd};
    use time::OffsetDateTime;

    use super::*;

    #[test]
    fn enum_conversions_cover_every_wire_variant() {
        assert_eq!(moderation_name(ModerationStatus::Visible), "visible");
        assert_eq!(moderation_name(ModerationStatus::Shadow), "shadow");
        assert_eq!(moderation_name(ModerationStatus::Blocked), "blocked");
        assert_eq!(SideDto::from(Side::Yes), SideDto::Yes);
        assert_eq!(SideDto::from(Side::No), SideDto::No);
        assert_eq!(Side::from(SideDto::Yes), Side::Yes);
        assert_eq!(Side::from(SideDto::No), Side::No);
        assert_eq!(TradeActionDto::from(TradeAction::Buy), TradeActionDto::Buy);
        assert_eq!(
            TradeActionDto::from(TradeAction::Sell),
            TradeActionDto::Sell
        );
        assert_eq!(TradeAction::from(TradeActionDto::Buy), TradeAction::Buy);
        assert_eq!(TradeAction::from(TradeActionDto::Sell), TradeAction::Sell);

        let states = [
            (MarketState::Draft, MarketStateDto::Draft),
            (MarketState::Scheduled, MarketStateDto::Scheduled),
            (MarketState::Live, MarketStateDto::Live),
            (MarketState::Closing, MarketStateDto::Closing),
            (MarketState::Closed, MarketStateDto::Closed),
            (MarketState::Resolving, MarketStateDto::Resolving),
            (MarketState::Resolved, MarketStateDto::Resolved),
            (MarketState::Paid, MarketStateDto::Paid),
            (MarketState::Voided, MarketStateDto::Voided),
        ];
        for (domain, wire) in states {
            assert_eq!(MarketStateDto::from(domain), wire);
        }
    }

    #[test]
    fn market_and_position_dtos_copy_plain_application_data() {
        let market = MarketId(Uuid::new_v4());
        let yes = OutcomeId(Uuid::new_v4());
        let no = OutcomeId(Uuid::new_v4());
        let row = MarketRow {
            id: market,
            slug: "dto-market".to_string(),
            question: "Will the DTO work?".to_string(),
            state: MarketState::Live,
            min_votes_to_resolve: 3,
            opens_at: OffsetDateTime::UNIX_EPOCH,
            closes_at: OffsetDateTime::UNIX_EPOCH,
            tally_hidden_at: OffsetDateTime::UNIX_EPOCH,
            yes_outcome: yes,
            no_outcome: no,
            curator_flagged_at: None,
            integrity_due_at: None,
            poster_asset_url: Some("/assets/poster.svg".into()),
            video_asset_url: Some("/assets/video.svg".into()),
        };
        let pool = Pool::new(
            MicroShares(1_000_000),
            MicroShares(1_000_000),
            BasisPoints(100),
        )
        .unwrap();
        let summary = MarketSummaryDto::from_row(&row, &pool);
        assert_eq!(summary.id, market.0);
        assert_eq!(summary.slug, "dto-market");
        assert_eq!(summary.yes_outcome_id, yes.0);
        assert_eq!(summary.no_outcome_id, no.0);
        assert_eq!(summary.price_yes_micro, 500_000);
        assert_eq!(summary.price_no_micro, 500_000);
        let snapshot = MarketSummaryDto::from_snapshot(
            &row,
            &application::model::MarketSnapshot {
                market,
                state: MarketState::Resolving,
                price_yes_micro: 600_000,
                price_no_micro: 400_000,
                tally: None,
                closes_at: OffsetDateTime::UNIX_EPOCH,
                tally_hidden_at: OffsetDateTime::UNIX_EPOCH,
                under_review: true,
                poster_asset_url: row.poster_asset_url.clone(),
                video_asset_url: row.video_asset_url.clone(),
            },
            OffsetDateTime::UNIX_EPOCH,
        );
        assert!(snapshot.under_review);
        assert_eq!(snapshot.price_yes_micro, 600_000);
        assert_eq!(snapshot.poster_asset_url.as_deref(), Some("/assets/poster.svg"));
        let flagged = FlaggedMarketDto::from(application::model::FlaggedMarketRow {
            market: row.clone(),
            report: Some(application::model::IntegrityReportRow {
                market,
                checks: serde_json::json!([]),
                verdict: domain::integrity::Verdict::Pass,
                created_at: OffsetDateTime::UNIX_EPOCH,
            }),
        });
        assert_eq!(flagged.report.unwrap().verdict, "pass");
        let flagged = FlaggedMarketDto::from(application::model::FlaggedMarketRow {
            market: row.clone(),
            report: Some(application::model::IntegrityReportRow {
                market,
                checks: serde_json::json!([]),
                verdict: domain::integrity::Verdict::Flag,
                created_at: OffsetDateTime::UNIX_EPOCH,
            }),
        });
        assert_eq!(flagged.report.unwrap().verdict, "flag");

        let position = PositionDto::from(PositionView {
            market,
            outcome: no,
            side: Side::No,
            shares: MicroShares(12),
            cost: MicroUsd(7),
            realized_pnl: MicroUsd(-2),
            rep_micro: 123,
            tier: 2,
        });
        assert_eq!(position.market_id, market.0);
        assert_eq!(position.outcome_id, no.0);
        assert_eq!(position.side, SideDto::No);
        assert_eq!(position.shares_micro, 12);
        assert_eq!(position.cost_micro, 7);
        assert_eq!(position.realized_pnl_micro, -2);
        assert_eq!(position.rep_micro, 123);
        assert_eq!(position.tier, 2);

        let price = PricePointDto::from(application::model::PricePoint {
            bucket_start: OffsetDateTime::UNIX_EPOCH,
            avg_price_micro: 510_000,
            volume_micro: 42,
            trades: 3,
        });
        assert_eq!(
            (price.avg_price_micro, price.volume_micro, price.trades),
            (510_000, 42, 3)
        );
        let tape = TapeRowDto::from(application::model::TapeRow {
            handle: "alice".to_string(),
            side: Side::Yes,
            action: TradeAction::Sell,
            collateral_micro: 9,
            created_at: OffsetDateTime::UNIX_EPOCH,
            trade_seq: 4,
        });
        assert_eq!(
            (tape.handle.as_str(), tape.action, tape.trade_seq),
            ("alice", TradeActionDto::Sell, 4)
        );
        let preview = TradePreviewDto::from(TradePreview {
            config_version: 1,
            market,
            side: Side::No,
            action: TradeAction::Buy,
            shares: MicroShares(12),
            gross: MicroUsd(10),
            fee: MicroUsd(1),
            avg_price_micro: 833_333,
        });
        assert_eq!((preview.side, preview.fee_micro), (SideDto::No, 1));
        let trade_id = application::model::TradeId(Uuid::new_v4());
        let trade = TradeReceiptDto::from(TradeReceipt {
            trade_id,
            ledger_txn: Uuid::new_v4(),
            side: Side::Yes,
            action: TradeAction::Buy,
            shares: MicroShares(4),
            gross: MicroUsd(3),
            fee: MicroUsd(1),
            avg_price_micro: 750_000,
            replayed: true,
        });
        assert_eq!(trade.trade_id, trade_id.0);
        assert!(trade.replayed);
        let vote_id = application::model::VoteId(Uuid::new_v4());
        let vote = VoteReceiptDto::from(application::model::VoteReceipt {
            vote_id,
            market,
            user: application::model::UserId(Uuid::new_v4()),
            side: Side::No,
            crowd_guess_pct: 44,
            seq: Some(8),
            replayed: false,
        });
        assert_eq!(
            (vote.vote_id, vote.seq, vote.side),
            (vote_id.0, Some(8), SideDto::No)
        );
    }

    #[test]
    fn economy_dtos_preserve_rankings_and_fee_source_totals() {
        let trader = TraderRowDto::from(application::model::TraderRow {
            handle: "trader".to_string(),
            realized_pnl_micro: -42,
            realizations: 3,
        });
        assert_eq!(
            trader,
            TraderRowDto {
                handle: "trader".to_string(),
                realized_pnl_micro: -42,
                realizations: 3,
            }
        );

        let voter = VoterRowDto::from(application::model::VoterRow {
            handle: "voter".to_string(),
            avg_score_bp: 7_501,
            markets_scored: 8,
            tier: 2,
        });
        assert_eq!(
            voter,
            VoterRowDto {
                handle: "voter".to_string(),
                avg_score_bp: 7_501,
                markets_scored: 8,
                tier: 2,
            }
        );

        let day = time::Date::from_calendar_date(2026, time::Month::August, 12).unwrap();
        let fees = DailyFeeRowDto::from(application::model::DailyFeeRow {
            day,
            trade_fee_micro: 11,
            payout_dust_micro: 4,
            total_micro: 15,
        });
        assert_eq!(
            fees,
            DailyFeeRowDto {
                day: "2026-08-12".to_string(),
                trade_fee_micro: 11,
                payout_dust_micro: 4,
                total_micro: 15,
            }
        );
    }
}
