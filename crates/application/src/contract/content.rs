//! Store-generic content transaction contract.

#![allow(clippy::missing_panics_doc, clippy::unwrap_used)]

use domain::drafting::{DraftSource, DraftSpec, DraftTier};
use domain::ledger::{Currency, Entry, TxnKind};
use domain::money::{BasisPoints, MicroUsd};
use time::{Duration, OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::content::create_draft::{CreateDraft, CreateDraftCmd};
use crate::content::publish_draft::PublishDraft;
use crate::content::review_draft::ReviewDraft;
use crate::content::template_engine::TemplateDraftEngine;
use crate::fakes::FakeClock;
use crate::model::{
    ContentConfig, DraftId, DraftRow, DraftStatus, LpKillConfig, OwnerRef, RepConfig, UserId,
};
use crate::ports::{MarketQueries, Store};

use super::unique_key;

/// Proves admission, ordering, row persistence, and durable slot-event
/// convergence against any `ContentTx` implementation.
pub async fn content_tx_contract<S: Store>(store: &S) {
    // A contract may be invoked repeatedly against the same database by
    // focused and workspace-wide suites.  Use the current instant so its
    // unique `(tier, publish_at)` slots cannot collide with a prior run.
    let now = OffsetDateTime::now_utc();
    let rows = [
        contract_draft(now, now + Duration::seconds(30), DraftTier::Daily),
        contract_draft(
            now - Duration::seconds(1),
            now + Duration::seconds(10),
            DraftTier::Daily,
        ),
        contract_draft(
            now - Duration::seconds(2),
            now + Duration::seconds(10),
            DraftTier::Flash,
        ),
    ];
    let budget_day = rows[0].publish_at.unwrap().to_offset(UtcOffset::UTC).date();
    let mut baseline_tx = store.content_tx().await.unwrap();
    let baseline = baseline_tx.pending_draft_count().await.unwrap();
    let baseline_reserved = baseline_tx.reserved_seed_for_day(budget_day).await.unwrap();
    baseline_tx.commit().await.unwrap();
    let mut tx = store.content_tx().await.unwrap();
    tx.lock_admission().await.unwrap();
    for row in &rows {
        tx.insert_draft(row.clone()).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mut tx = store.content_tx().await.unwrap();
    assert_eq!(tx.pending_draft_count().await.unwrap(), baseline + 3);
    let listed = tx.list_drafts(1_000).await.unwrap();
    assert!(rows
        .iter()
        .all(|expected| listed.iter().any(|actual| actual.id == expected.id)));
    let mut approved = tx.draft_for_update(rows[0].id).await.unwrap();
    approved.status = DraftStatus::Approved;
    tx.save_draft(&approved).await.unwrap();
    for row in &rows[1..] {
        let mut approved = tx.draft_for_update(row.id).await.unwrap();
        approved.status = DraftStatus::Approved;
        tx.save_draft(&approved).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mut tx = store.content_tx().await.unwrap();
    let due = tx
        .due_drafts(now + Duration::seconds(30), 100)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let ids: std::collections::BTreeSet<_> = rows.iter().map(|row| row.id).collect();
    let due: Vec<_> = due.into_iter().filter(|id| ids.contains(id)).collect();
    assert_eq!(due, vec![rows[2].id, rows[1].id, rows[0].id]);

    let mut stages = store.content_tx().await.unwrap();
    let mut in_flight = stages.draft_for_update(rows[1].id).await.unwrap();
    in_flight.publish_stage = Some(crate::model::PublishStage::Claimed);
    in_flight.published_market = Some(crate::model::MarketId(Uuid::new_v4()));
    stages.save_draft(&in_flight).await.unwrap();
    let mut published = stages.draft_for_update(rows[2].id).await.unwrap();
    published.status = DraftStatus::Published;
    published.publish_stage = Some(crate::model::PublishStage::Published);
    published.published_market = Some(crate::model::MarketId(Uuid::new_v4()));
    stages.save_draft(&published).await.unwrap();
    stages.commit().await.unwrap();

    let mut budget = store.content_tx().await.unwrap();
    assert_eq!(
        budget.reserved_seed_for_day(budget_day).await.unwrap(),
        baseline_reserved + 3_000_000
    );
    budget.commit().await.unwrap();

    let mut occupied = store.content_tx().await.unwrap();
    assert!(!occupied
        .claim_unfilled_slot(rows[2].publish_at.unwrap())
        .await
        .unwrap());
    occupied.commit().await.unwrap();

    let slot = now - Duration::seconds(10);
    let mut first = store.content_tx().await.unwrap();
    assert!(first.claim_unfilled_slot(slot).await.unwrap());
    first
        .append(crate::model::Event {
            event_type: crate::model::event_type::SLOT_UNFILLED,
            aggregate_type: "publication_slot",
            aggregate_id: Uuid::new_v4(),
            payload: serde_json::json!({"slot_unix": slot.unix_timestamp()}),
        })
        .await
        .unwrap();
    first.commit().await.unwrap();
    let mut replay = store.content_tx().await.unwrap();
    assert!(!replay.claim_unfilled_slot(slot).await.unwrap());
    replay.commit().await.unwrap();

    assert_expiry_query(store, now).await;
}

async fn assert_expiry_query<S: Store>(store: &S, now: OffsetDateTime) {
    let mut expired = contract_draft(
        now - Duration::days(2),
        now + Duration::seconds(40),
        DraftTier::Flash,
    );
    expired.expires_at = now - Duration::seconds(1);
    let mut insert = store.content_tx().await.unwrap();
    insert.insert_draft(expired.clone()).await.unwrap();
    insert.commit().await.unwrap();
    let mut query = store.content_tx().await.unwrap();
    let due = query.due_expiries(now, 1_000).await.unwrap();
    query.commit().await.unwrap();
    assert!(due.contains(&expired.id));
}

/// Proves the complete approval/publication authority, durable effect keys,
/// and replay convergence against both store implementations.
pub async fn curation_saga_contract<S: Store + MarketQueries>(store: &S) {
    let now = OffsetDateTime::from_unix_timestamp(1_710_100_000).unwrap();
    let clock = FakeClock::at(now);
    let config = ContentConfig::default();
    fund_house(store, 500_000_000).await;
    let reviewer = contract_user(store).await;
    let topic = format!("curation contract {}", Uuid::new_v4());
    let engine = TemplateDraftEngine::new(config.clone());
    let draft = CreateDraft {
        store,
        clock: &clock,
        config: &config,
        primary: &engine,
        template: &engine,
    }
    .execute(CreateDraftCmd {
        topics: vec![topic],
        tier: DraftTier::Flash,
        requested_source: DraftSource::Template,
        allow_fallback: false,
    })
    .await
    .unwrap()
    .drafts
    .remove(0);
    let approved = ReviewDraft {
        store,
        clock: &clock,
        config: &config,
        lp_kill_config: LpKillConfig::default(),
    }
    .approve(draft.id, reviewer)
    .await
    .unwrap();
    assert_eq!(approved.status, DraftStatus::Approved);
    assert!(approved.publish_at.unwrap() > now);

    let saga = PublishDraft::new(
        store,
        &clock,
        &config,
        RepConfig::default(),
        LpKillConfig::default(),
    );
    let first = saga.execute(draft.id).await.unwrap();
    assert!(!first.replayed);
    let replay = saga.execute(draft.id).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.market, first.market);
    assert_eq!(
        store
            .market_by_ref(&draft.spec.slug)
            .await
            .unwrap()
            .question,
        draft.spec.question
    );

    let mut content = store.content_tx().await.unwrap();
    let published = content.draft_for_update(draft.id).await.unwrap();
    content.commit().await.unwrap();
    assert_eq!(published.status, DraftStatus::Published);
    assert_eq!(
        published.publish_stage,
        Some(crate::model::PublishStage::Published)
    );
    assert_eq!(published.published_market, Some(first.market));

    let seed_key = format!("draft:{}:seed", draft.id.0);
    let mut bootstrap = store.bootstrap_tx().await.unwrap();
    bootstrap.serialize_key(&seed_key).await.unwrap();
    assert!(bootstrap.txn_by_key(&seed_key).await.unwrap().is_some());
    bootstrap.commit().await.unwrap();

    let live_key = format!("draft:{}:golive", draft.id.0);
    let mut advance = store.advance_tx().await.unwrap();
    advance.serialize_key(&live_key).await.unwrap();
    let live = advance.lifecycle_command(&live_key).await.unwrap().unwrap();
    advance.commit().await.unwrap();
    assert_eq!(live.market, first.market);
    assert_eq!(live.resulting_state, domain::market::MarketState::Live);
}

async fn contract_user<S: Store>(store: &S) -> UserId {
    let key = unique_key("curation-reviewer");
    let mut tx = store.bootstrap_tx().await.unwrap();
    tx.serialize_key(&key).await.unwrap();
    let user = tx.insert_user(&key).await.unwrap();
    tx.commit().await.unwrap();
    user
}

async fn fund_house<S: Store>(store: &S, amount: i64) {
    let key = unique_key("curation-capital");
    let mut tx = store.bootstrap_tx().await.unwrap();
    tx.serialize_key(&key).await.unwrap();
    let external = tx
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let house = tx.account(OwnerRef::House, Currency::Usdc).await.unwrap();
    tx.ledger_apply(
        TxnKind::Deposit,
        &key,
        &[
            Entry {
                account: external,
                amount: MicroUsd(-amount),
            },
            Entry {
                account: house,
                amount: MicroUsd(amount),
            },
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

fn contract_draft(
    created_at: OffsetDateTime,
    publish_at: OffsetDateTime,
    tier: DraftTier,
) -> DraftRow {
    let id = DraftId(Uuid::new_v4());
    DraftRow {
        id,
        spec: DraftSpec {
            question: format!("Will contract draft {id:?} publish?"),
            description: "Contract fixture".into(),
            video_script: "Contract fixture".into(),
            slug: format!("contract-{id:?}"),
            tier,
            seed: MicroUsd(1_000_000),
            fee: BasisPoints(100),
            min_votes_to_resolve: 3,
            open_secs: 3_600,
            hidden_window_secs: 300,
        },
        source: DraftSource::Template,
        fallback_from: None,
        status: DraftStatus::Pending,
        publish_stage: None,
        published_market: None,
        publish_at: Some(publish_at),
        expires_at: created_at + Duration::days(1),
        created_at,
    }
}

#[cfg(test)]
mod tests {
    use crate::error::StoreError;
    use crate::fakes::{InMemoryStore, UnavailableDraftEngine, UnavailableModerationPreflight};
    use crate::model::DraftRequest;
    use crate::ports::{DraftEngine, DraftEngineUnavailable, ModerationPreflight};

    #[tokio::test]
    async fn fake_passes_content_transaction_contract() {
        let store = InMemoryStore::new();
        super::content_tx_contract(&store).await;
        super::curation_saga_contract(&store).await;
    }

    #[tokio::test]
    async fn external_placeholders_remain_typed() {
        let request = DraftRequest {
            topic: "local transit".into(),
            tier: domain::drafting::DraftTier::Daily,
        };
        assert_eq!(
            UnavailableDraftEngine.generate(&request).await,
            Err(DraftEngineUnavailable)
        );
        assert_eq!(
            UnavailableModerationPreflight.screen("hello").await,
            Err(StoreError::Unavailable("phase5:moderation-preflight"))
        );
    }
}
