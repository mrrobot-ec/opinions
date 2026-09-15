//! Single draft-locked expiry authority.

use time::OffsetDateTime;

use crate::error::StoreError;
use crate::model::{DraftId, DraftRow, DraftStatus};
use crate::ports::Store;

/// Applies the only legal expiry transition to a row already held for update.
pub(super) fn expire_locked(row: &mut DraftRow, now: OffsetDateTime) -> bool {
    if row.status == DraftStatus::Pending && now >= row.expires_at {
        row.status = DraftStatus::Expired;
        true
    } else {
        false
    }
}

/// Expires one draft if it is still pending at its persisted boundary.
///
/// # Errors
/// Returns a store error when the draft cannot be locked, saved, or committed.
pub async fn expire_draft<S: Store + ?Sized>(
    store: &S,
    draft: DraftId,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let mut tx = store.content_tx().await?;
    let mut row = tx.draft_for_update(draft).await?;
    if !expire_locked(&mut row, now) {
        return Ok(false);
    }
    tx.save_draft(&row).await?;
    tx.commit().await?;
    Ok(true)
}

/// Bounded, expiry-time-ordered scheduler arm.
///
/// # Errors
/// Returns a store error when selecting, expiring, or counting due drafts fails.
pub async fn expire_due<S: Store + ?Sized>(
    store: &S,
    now: OffsetDateTime,
    limit: u32,
) -> Result<u32, StoreError> {
    let mut query = store.content_tx().await?;
    let due = query.due_expiries(now, limit).await?;
    query.commit().await?;
    let mut expired = 0_u32;
    for draft in due {
        if expire_draft(store, draft, now).await? {
            // `due_expiries` is bounded by a `u32` limit, so one increment per
            // returned row cannot exceed `u32::MAX`.
            expired += 1;
        }
    }
    Ok(expired)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use domain::drafting::{DraftSource, DraftTier};

    use super::*;
    use crate::content::create_draft::{CreateDraft, CreateDraftCmd};
    use crate::content::template_engine::TemplateDraftEngine;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::ContentConfig;

    #[tokio::test]
    async fn expiry_boundary_transitions_once_and_due_count_reports_the_winner() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let config = ContentConfig {
            draft_ttl_secs: 1,
            ..ContentConfig::default()
        };
        let engine = TemplateDraftEngine::new(config.clone());
        let draft = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &engine,
            template: &engine,
        }
        .execute(CreateDraftCmd {
            topics: vec!["expiry boundary".into()],
            tier: DraftTier::Flash,
            requested_source: DraftSource::Template,
            allow_fallback: false,
        })
        .await
        .unwrap()
        .drafts
        .remove(0);

        assert!(!expire_draft(&store, draft.id, now).await.unwrap());
        assert_eq!(expire_due(&store, draft.expires_at, 10).await.unwrap(), 1);
        assert!(!expire_draft(&store, draft.id, draft.expires_at)
            .await
            .unwrap());
        assert_eq!(store.draft(draft.id).unwrap().status, DraftStatus::Expired);
    }
}
