//! On-demand share cards (Task 5.2, codex B5 + grok B2): NOT jobs. Rendered
//! post-resolution only, cached content-addressed by
//! `sha256(user, market, realization digest, spec version)` under the
//! renderer's `cards/` subroot. Fields pinned: handle, question, side,
//! payout, return percent, brand mark — no referral code this phase.

use std::fmt::Write as _;

use domain::render_spec::{sha256_hex, share_card_spec, ShareCardInput, SPEC_VERSION};

use crate::error::AppError;
use crate::model::{
    ContentConfig, MarketId, MarketRow, RealizationFact, RealizationSource, UserId,
};
use crate::ports::{Renderer, Store};

/// Deterministic aggregate of one user's realizations on one market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSummary {
    pub side_yes: bool,
    pub payout_micro: i64,
    pub return_bps: i64,
    /// Hex digest over the ordered realization facts; part of the address.
    pub realization_digest: String,
}

fn source_name(source: RealizationSource) -> &'static str {
    match source {
        RealizationSource::Sell => "sell",
        RealizationSource::Settlement => "settlement",
        RealizationSource::Void => "void",
    }
}

fn clamp(value: i128) -> i64 {
    i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX })
}

/// Facts MUST already be ordered by `(created_at, ledger_txn)` — the port
/// contract — so the digest and side selection are deterministic.
///
/// # Panics
/// Never panics on non-empty input; callers guard emptiness.
#[must_use]
pub fn summarize(facts: &[RealizationFact], market: &MarketRow) -> CardSummary {
    assert!(!facts.is_empty(), "summarize requires at least one fact");
    let payout: i128 = facts.iter().map(|fact| i128::from(fact.payout.0)).sum();
    let delta: i128 = facts
        .iter()
        .map(|fact| i128::from(fact.realized_delta.0))
        .sum();
    let cost = payout - delta;
    let return_bps = if cost > 0 {
        clamp(delta * 10_000 / cost)
    } else {
        0
    };
    let last = &facts[facts.len() - 1];
    let mut digest_input = String::new();
    for fact in facts {
        let _ = write!(
            digest_input,
            "{}:{}:{}:{}:{};",
            source_name(fact.source),
            fact.outcome.0,
            fact.realized_delta.0,
            fact.payout.0,
            fact.ledger_txn,
        );
    }
    CardSummary {
        side_yes: last.outcome == market.yes_outcome,
        payout_micro: clamp(payout),
        return_bps,
        realization_digest: sha256_hex(digest_input.as_bytes()),
    }
}

/// Content address: `sha256(user, market, realization digest, spec version)`.
#[must_use]
pub fn card_address(
    user: UserId,
    market: MarketId,
    realization_digest: &str,
    spec_version: u8,
) -> String {
    sha256_hex(
        format!(
            "{}:{}:{realization_digest}:{spec_version}",
            user.0, market.0
        )
        .as_bytes(),
    )
}

fn post_resolution(state: domain::market::MarketState) -> bool {
    matches!(
        state,
        domain::market::MarketState::Resolved
            | domain::market::MarketState::Paid
            | domain::market::MarketState::Voided
    )
}

/// Renders (or serves cached) share-card bytes for one (user, market).
///
/// # Errors
/// [`AppError::JobNotReady`] before resolution, [`AppError::ArtifactNotFound`]
/// when the user has no realizations on the market, store failures otherwise.
pub async fn share_card_svg<S: Store + ?Sized>(
    store: &S,
    renderer: &dyn Renderer,
    config: &ContentConfig,
    user: UserId,
    market: MarketId,
) -> Result<Vec<u8>, AppError> {
    let mut tx = store.video_tx().await?;
    let market_row = tx.market_row(market).await?;
    if !post_resolution(market_row.state) {
        return Err(AppError::JobNotReady);
    }
    let facts = tx.realizations(user, market).await?;
    let handle = tx.user_handle(user).await?;
    tx.commit().await?;
    if facts.is_empty() {
        return Err(AppError::ArtifactNotFound);
    }
    let summary = summarize(&facts, &market_row);
    let spec = share_card_spec(&ShareCardInput {
        handle: &handle,
        question: &market_row.question,
        side_yes: summary.side_yes,
        payout_micro: summary.payout_micro,
        return_bps: summary.return_bps,
    });
    debug_assert_eq!(spec.version, SPEC_VERSION);
    let address = card_address(user, market, &summary.realization_digest, spec.version);
    renderer
        .share_card(&config.render_dir, &address, &spec)
        .await
        .map_err(AppError::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::InMemoryStore;
    use crate::model::OutcomeId;
    use crate::ports::{SettlementIo, Store};
    use crate::video::worker::test_support::{insert_market, insert_user, MemRenderer};
    use domain::money::MicroUsd;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn at(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    fn fact(
        user: UserId,
        market: MarketId,
        outcome: OutcomeId,
        source: RealizationSource,
        delta: i64,
        payout: i64,
        unix: i64,
    ) -> RealizationFact {
        RealizationFact {
            user,
            market,
            outcome,
            source,
            realized_delta: MicroUsd(delta),
            payout: MicroUsd(payout),
            ledger_txn: Uuid::new_v4(),
            created_at: at(unix),
        }
    }

    fn market_row(yes: OutcomeId) -> MarketRow {
        MarketRow {
            id: MarketId(Uuid::new_v4()),
            slug: "s".into(),
            question: "q".into(),
            state: domain::market::MarketState::Resolved,
            min_votes_to_resolve: 3,
            opens_at: at(0),
            closes_at: at(10),
            tally_hidden_at: at(5),
            yes_outcome: yes,
            no_outcome: OutcomeId(Uuid::new_v4()),
            curator_flagged_at: None,
            integrity_due_at: None,
            poster_asset_url: None,
            video_asset_url: None,
        }
    }

    #[test]
    fn summarize_pins_side_payout_return_and_digest() {
        let user = UserId(Uuid::new_v4());
        let yes = OutcomeId(Uuid::new_v4());
        let row = market_row(yes);
        let market = row.id;
        // Cost 10.00, payout 12.50 → delta 2.50 → +25.0% return.
        let facts = vec![fact(
            user,
            market,
            yes,
            RealizationSource::Settlement,
            2_500_000,
            12_500_000,
            100,
        )];
        let summary = summarize(&facts, &row);
        assert!(summary.side_yes);
        assert_eq!(summary.payout_micro, 12_500_000);
        assert_eq!(summary.return_bps, 2_500);
        assert_eq!(summary.realization_digest.len(), 64);

        // Same facts → same digest; different facts → different digest.
        assert_eq!(
            summarize(&facts, &row).realization_digest,
            summary.realization_digest
        );
        let other = vec![fact(
            user,
            market,
            yes,
            RealizationSource::Settlement,
            2_500_000,
            12_500_001,
            100,
        )];
        assert_ne!(
            summarize(&other, &row).realization_digest,
            summary.realization_digest
        );
    }

    #[test]
    fn summarize_handles_losses_voids_and_zero_cost() {
        let user = UserId(Uuid::new_v4());
        let yes = OutcomeId(Uuid::new_v4());
        let row = market_row(yes);
        let market = row.id;
        let no = row.no_outcome;
        // A NO-side loss: cost 10.00, payout 0 → -100%.
        let loss = vec![fact(
            user,
            market,
            no,
            RealizationSource::Settlement,
            -10_000_000,
            0,
            100,
        )];
        let summary = summarize(&loss, &row);
        assert!(!summary.side_yes);
        assert_eq!(summary.return_bps, -10_000);
        assert_eq!(summary.payout_micro, 0);
        // Zero cost never divides: return pins to 0.
        let zero_cost = vec![fact(user, market, yes, RealizationSource::Void, 5, 5, 100)];
        assert_eq!(summarize(&zero_cost, &row).return_bps, 0);
        // Multiple facts aggregate; the LAST fact picks the side.
        let mixed = vec![
            fact(
                user,
                market,
                yes,
                RealizationSource::Sell,
                1_000_000,
                3_000_000,
                100,
            ),
            fact(
                user,
                market,
                no,
                RealizationSource::Settlement,
                2_000_000,
                9_000_000,
                200,
            ),
        ];
        let mixed_summary = summarize(&mixed, &row);
        assert!(!mixed_summary.side_yes);
        assert_eq!(mixed_summary.payout_micro, 12_000_000);
        // delta 3.00 on cost 9.00 → +33.33% truncated to bps.
        assert_eq!(mixed_summary.return_bps, 3_333);
    }

    #[test]
    #[should_panic(expected = "summarize requires at least one fact")]
    fn summarize_refuses_empty_input() {
        let row = market_row(OutcomeId(Uuid::new_v4()));
        let _ = summarize(&[], &row);
    }

    #[test]
    fn card_address_is_stable_and_input_sensitive() {
        let user = UserId(Uuid::from_u128(1));
        let market = MarketId(Uuid::from_u128(2));
        let a = card_address(user, market, "digest", 1);
        assert_eq!(a, card_address(user, market, "digest", 1));
        assert_ne!(a, card_address(user, market, "digest", 2));
        assert_ne!(a, card_address(user, market, "other", 1));
        assert_ne!(
            a,
            card_address(UserId(Uuid::from_u128(3)), market, "digest", 1)
        );
    }

    async fn resolve_market(store: &InMemoryStore, market: MarketId) {
        let mut tx = store.resolve_tx().await.unwrap();
        tx.serialize_key(&format!("card-resolve-{}", market.0))
            .await
            .unwrap();
        tx.market_for_update(market).await.unwrap();
        SettlementIo::set_market_state(tx.as_mut(), market, domain::market::MarketState::Resolved)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn insert_fact(store: &InMemoryStore, fact: RealizationFact) {
        let mut tx = store.resolve_tx().await.unwrap();
        tx.serialize_key(&format!("card-fact-{}", Uuid::new_v4()))
            .await
            .unwrap();
        tx.insert_realization(&fact).await.unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn share_card_flow_gates_caches_and_renders() {
        let store = InMemoryStore::default();
        let renderer = MemRenderer::default();
        let config = crate::model::ContentConfig::default();
        let market = insert_market(&store).await;
        let user = insert_user(&store, "card-alice").await;

        // Pre-resolution: refused with the job-not-ready rejection.
        assert!(matches!(
            share_card_svg(&store, &renderer, &config, user, market).await,
            Err(AppError::JobNotReady)
        ));

        resolve_market(&store, market).await;

        // Post-resolution but no participation: not found.
        assert!(matches!(
            share_card_svg(&store, &renderer, &config, user, market).await,
            Err(AppError::ArtifactNotFound)
        ));

        let yes = {
            let mut tx = store.video_tx().await.unwrap();
            let row = tx.market_row(market).await.unwrap();
            tx.commit().await.unwrap();
            row.yes_outcome
        };
        insert_fact(
            &store,
            fact(
                user,
                market,
                yes,
                RealizationSource::Settlement,
                2_500_000,
                12_500_000,
                1_700_000_100,
            ),
        )
        .await;

        let bytes = share_card_svg(&store, &renderer, &config, user, market)
            .await
            .unwrap();
        assert!(!bytes.is_empty());
        // Second call is served from the content-addressed cache.
        let again = share_card_svg(&store, &renderer, &config, user, market)
            .await
            .unwrap();
        assert_eq!(bytes, again);
        assert_eq!(renderer.cards.lock().len(), 1);

        // Unknown market and unknown user surface typed not-found errors.
        assert!(matches!(
            share_card_svg(&store, &renderer, &config, user, MarketId(Uuid::new_v4())).await,
            Err(AppError::Store(crate::error::StoreError::NotFound(
                "market"
            )))
        ));
        let ghost = UserId(Uuid::new_v4());
        assert!(matches!(
            share_card_svg(&store, &renderer, &config, ghost, market).await,
            Err(AppError::Store(crate::error::StoreError::NotFound("user")))
        ));
    }
}
