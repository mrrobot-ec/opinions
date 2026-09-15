//! D26 / codex r3 NEW-4 — command-based publish-now: the admin transaction
//! atomically inserts (idempotent publication command + audit) and the route
//! answers 202 + a status URL. The existing publisher tick consumes due
//! commands FIRST (before cadence-due drafts) under a lease, so a crash
//! after authorization leaves a pending command the next tick executes
//! exactly once (the `PublishDraft` saga is idempotent per draft).

use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{
    AdminContext, ContentConfig, DraftId, DraftStatus, Event, LpKillConfig, RepConfig,
};
use crate::ports::{Clock, Store};
use crate::ports::{PublicationCommand, PublicationCommandStatus};

use super::publish_draft::PublishDraft;
use crate::ops::audit::audit_for;

#[derive(Debug, Clone)]
pub struct PublishNowCmd {
    pub draft: DraftId,
    pub idempotency_key: String,
}

/// The 202 receipt: command identity + current status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishNowReceipt {
    pub command: PublicationCommand,
    pub replayed: bool,
}

pub struct PublishNow<'a, S: Store> {
    pub store: &'a S,
    pub actor: AdminContext,
}

impl<S: Store> PublishNow<'_, S> {
    /// Authorizes publication: inserts the durable command + audit in ONE
    /// transaction. An open command for the draft (same or different key)
    /// replays idempotently.
    ///
    /// # Errors
    /// [`AppError::DraftNotApproved`] / [`AppError::DraftExpired`] for
    /// unpublishable drafts; store failures.
    pub async fn execute(&self, cmd: PublishNowCmd) -> Result<PublishNowReceipt, AppError> {
        let mut tx = self.store.content_tx().await?;
        tx.lock_publication(cmd.draft).await?;
        if let Some(existing) = tx.publication_command_by_key(&cmd.idempotency_key).await? {
            return Ok(PublishNowReceipt {
                command: existing,
                replayed: true,
            });
        }
        let draft = tx.draft_for_update(cmd.draft).await?;
        match draft.status {
            DraftStatus::Approved => {}
            DraftStatus::Published => {
                // Already terminal: report the latest command if any.
                if let Some(existing) = tx.publication_command_by_draft(cmd.draft).await? {
                    return Ok(PublishNowReceipt {
                        command: existing,
                        replayed: true,
                    });
                }
                return Err(AppError::DraftNotApproved);
            }
            DraftStatus::Expired => return Err(AppError::DraftExpired),
            _ => return Err(AppError::DraftNotApproved),
        }
        if let Some(open) = tx.publication_command_by_draft(cmd.draft).await? {
            if matches!(
                open.status,
                PublicationCommandStatus::Pending | PublicationCommandStatus::Executing
            ) {
                return Ok(PublishNowReceipt {
                    command: open,
                    replayed: true,
                });
            }
        }
        let requested_by = match &self.actor {
            AdminContext::Admin { token_digest, .. } => token_digest.clone(),
            AdminContext::Machine => "machine".to_string(),
        };
        let command = PublicationCommand {
            id: Uuid::new_v4(),
            draft: cmd.draft,
            idempotency_key: cmd.idempotency_key.clone(),
            requested_by,
            status: PublicationCommandStatus::Pending,
            attempts: 0,
            lease_expires_at: None,
            result_market: None,
            error: None,
        };
        tx.insert_publication_command(command.clone()).await?;
        if let Some(row) = audit_for(
            &self.actor,
            "publish_now",
            format!("draft:{}", cmd.draft.0),
            Some(json!({ "status": format!("{:?}", draft.status) })),
            Some(json!({ "command_id": command.id.to_string() })),
            None,
        ) {
            tx.audit_insert(row).await?;
        }
        tx.append(Event {
            event_type: "PublicationCommanded",
            aggregate_type: "draft",
            aggregate_id: cmd.draft.0,
            payload: json!({ "command_id": command.id.to_string() }),
        })
        .await?;
        tx.commit().await?;
        Ok(PublishNowReceipt {
            command,
            replayed: false,
        })
    }
}

/// Publisher-tick arm: consumes due publication commands FIRST. Claims under
/// a lease in one transaction, executes the idempotent saga outside it, and
/// settles done/failed in a second transaction — crash windows re-lease and
/// the saga replays cleanly.
///
/// # Errors
/// Claim/settle store failures; saga failures settle the command `Failed`
/// without aborting the sweep.
pub async fn consume_due_commands<S: Store, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
    config: &ContentConfig,
    rep_config: RepConfig,
    lp_kill_config: LpKillConfig,
) -> Result<u32, AppError> {
    let now = clock.now();
    let lease_secs = i64::try_from(config.lease_secs.max(1)).unwrap_or(60);
    let lease_until = now + time::Duration::seconds(lease_secs);
    let mut claim = store.content_tx().await?;
    claim.lock_publication_queue().await?;
    let due = claim
        .due_publication_commands(now, config.max_pending_drafts)
        .await?;
    let mut claimed = Vec::with_capacity(due.len());
    for mut command in due {
        command.status = PublicationCommandStatus::Executing;
        command.lease_expires_at = Some(lease_until);
        command.attempts = command.attempts.saturating_add(1);
        claim.save_publication_command(&command).await?;
        claimed.push(command);
    }
    claim.commit().await?;

    let mut published = 0_u32;
    for command in claimed {
        let saga = PublishDraft::new(store, clock, config, rep_config, lp_kill_config);
        let outcome = saga.execute(command.draft).await;
        let mut settle = store.content_tx().await?;
        settle.lock_publication(command.draft).await?;
        let mut settled = command.clone();
        match outcome {
            Ok(receipt) => {
                settled.status = PublicationCommandStatus::Done;
                settled.result_market = Some(receipt.market);
                settled.error = None;
                published = published.saturating_add(1);
            }
            Err(error) => {
                settled.status = PublicationCommandStatus::Failed;
                settled.error = Some(error.to_string());
            }
        }
        settled.lease_expires_at = None;
        settle.save_publication_command(&settled).await?;
        settle.commit().await?;
    }
    Ok(published)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::content::create_draft::{CreateDraft, CreateDraftCmd};
    use crate::content::publisher::publisher_tick;
    use crate::content::review_draft::ReviewDraft;
    use crate::content::template_engine::TemplateDraftEngine;
    use crate::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{AdminRole, DraftStatus};
    use crate::ports::OpsQueries;
    use domain::drafting::{DraftSource, DraftTier};
    use domain::ledger::Currency;
    use domain::money::MicroUsd;
    use time::{Duration, OffsetDateTime};

    fn curator() -> AdminContext {
        AdminContext::Admin {
            token_digest: "digest-curator".into(),
            role: AdminRole::Curator,
        }
    }

    async fn approved_draft(
        store: &InMemoryStore,
        clock: &FakeClock,
        config: &ContentConfig,
    ) -> DraftId {
        EnsureGenesis { store }
            .execute(EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(2_000_000_000),
            })
            .await
            .unwrap();
        let engine = TemplateDraftEngine::new(config.clone());
        let draft = CreateDraft {
            store,
            clock,
            config,
            primary: &engine,
            template: &engine,
        }
        .execute(CreateDraftCmd {
            topics: vec!["publish now".into()],
            tier: DraftTier::Flash,
            requested_source: DraftSource::Template,
            allow_fallback: false,
        })
        .await
        .unwrap()
        .drafts
        .remove(0);
        let reviewer = store.add_user("curator", clock.now() - Duration::days(2), 2);
        ReviewDraft {
            store,
            clock,
            config,
            lp_kill_config: LpKillConfig::default(),
        }
        .approve(draft.id, reviewer)
        .await
        .unwrap();
        draft.id
    }

    #[tokio::test]
    async fn authorize_then_tick_executes_exactly_once_with_audit() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let config = ContentConfig::default();
        let draft = approved_draft(&store, &clock, &config).await;

        // Authorization: 202-shaped receipt, durable pending command, audit.
        let receipt = PublishNow {
            store: &store,
            actor: curator(),
        }
        .execute(PublishNowCmd {
            draft,
            idempotency_key: "pn-1".into(),
        })
        .await
        .unwrap();
        assert!(!receipt.replayed);
        assert_eq!(receipt.command.status, PublicationCommandStatus::Pending);
        let audits = store.audit_page(None, 10).await.unwrap();
        assert!(audits.iter().any(|row| row.action.action == "publish_now"));
        // Idempotent replay (same key) and open-command replay (new key).
        let replay = PublishNow {
            store: &store,
            actor: curator(),
        }
        .execute(PublishNowCmd {
            draft,
            idempotency_key: "pn-1".into(),
        })
        .await
        .unwrap();
        assert!(replay.replayed);
        let second_key = PublishNow {
            store: &store,
            actor: curator(),
        }
        .execute(PublishNowCmd {
            draft,
            idempotency_key: "pn-2".into(),
        })
        .await
        .unwrap();
        assert!(second_key.replayed, "one open command per draft");
        assert_eq!(second_key.command.id, receipt.command.id);

        // Crash-after-authorize shape: the NEXT tick consumes the command
        // FIRST and publishes exactly once.
        let sweep = publisher_tick(
            &store,
            &clock,
            &config,
            crate::model::RepConfig::default(),
            LpKillConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(sweep.published, 1);
        let status = store
            .publication_command_for_draft(draft)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.status, PublicationCommandStatus::Done);
        let market = status.result_market.unwrap();
        assert_eq!(store.draft(draft).unwrap().status, DraftStatus::Published);
        // A second tick re-publishes nothing (exactly once).
        let second = publisher_tick(
            &store,
            &clock,
            &config,
            crate::model::RepConfig::default(),
            LpKillConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(second.published, 0);
        assert_eq!(
            store
                .publication_command_for_draft(draft)
                .await
                .unwrap()
                .unwrap()
                .result_market,
            Some(market)
        );

        let terminal_replay = PublishNow {
            store: &store,
            actor: curator(),
        }
        .execute(PublishNowCmd {
            draft,
            idempotency_key: "pn-after-publish".into(),
        })
        .await
        .unwrap();
        assert!(terminal_replay.replayed);
        assert_eq!(
            terminal_replay.command.status,
            PublicationCommandStatus::Done
        );
    }

    #[tokio::test]
    async fn unapproved_and_unknown_drafts_cannot_be_commanded() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let config = ContentConfig::default();
        EnsureGenesis { store: &store }
            .execute(EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(2_000_000_000),
            })
            .await
            .unwrap();
        let engine = TemplateDraftEngine::new(config.clone());
        let pending = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &engine,
            template: &engine,
        }
        .execute(CreateDraftCmd {
            topics: vec!["still pending".into()],
            tier: DraftTier::Flash,
            requested_source: DraftSource::Template,
            allow_fallback: false,
        })
        .await
        .unwrap()
        .drafts
        .remove(0);
        let uc = PublishNow {
            store: &store,
            actor: curator(),
        };
        let denied = uc
            .execute(PublishNowCmd {
                draft: pending.id,
                idempotency_key: "pn-x".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(denied, AppError::DraftNotApproved);
        let missing = uc
            .execute(PublishNowCmd {
                draft: DraftId(uuid::Uuid::new_v4()),
                idempotency_key: "pn-y".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(missing, AppError::Store(_)));
    }

    #[tokio::test]
    async fn machine_authorization_is_unaudited_and_an_expired_saga_settles_failed() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let config = ContentConfig::default();
        let draft = approved_draft(&store, &clock, &config).await;
        let receipt = PublishNow {
            store: &store,
            actor: AdminContext::Machine,
        }
        .execute(PublishNowCmd {
            draft,
            idempotency_key: "pn-machine".into(),
        })
        .await
        .unwrap();
        assert_eq!(receipt.command.requested_by, "machine");
        assert!(store.audit_page(None, 10).await.unwrap().is_empty());

        let mut corrupt = store.content_tx().await.unwrap();
        let mut row = corrupt.draft_for_update(draft).await.unwrap();
        row.status = DraftStatus::Rejected;
        corrupt.save_draft(&row).await.unwrap();
        corrupt.commit().await.unwrap();
        assert_eq!(
            consume_due_commands(
                &store,
                &clock,
                &config,
                RepConfig::default(),
                LpKillConfig::default(),
            )
            .await
            .unwrap(),
            0
        );
        let settled = store
            .publication_command_for_draft(draft)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settled.status, PublicationCommandStatus::Failed);
        assert!(settled.error.is_some());
    }

    #[tokio::test]
    async fn terminal_draft_and_terminal_command_edges_are_explicit() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let config = ContentConfig::default();

        for (status, expected) in [
            (DraftStatus::Published, AppError::DraftNotApproved),
            (DraftStatus::Expired, AppError::DraftExpired),
        ] {
            let store = InMemoryStore::new();
            let clock = FakeClock::at(now);
            let draft = approved_draft(&store, &clock, &config).await;
            let mut tx = store.content_tx().await.unwrap();
            let mut row = tx.draft_for_update(draft).await.unwrap();
            row.status = status;
            tx.save_draft(&row).await.unwrap();
            tx.commit().await.unwrap();
            assert_eq!(
                PublishNow {
                    store: &store,
                    actor: curator()
                }
                .execute(PublishNowCmd {
                    draft,
                    idempotency_key: format!("terminal-{status:?}"),
                })
                .await
                .unwrap_err(),
                expected
            );
        }

        let store = InMemoryStore::new();
        let clock = FakeClock::at(now);
        let draft = approved_draft(&store, &clock, &config).await;
        let first = PublishNow {
            store: &store,
            actor: curator(),
        }
        .execute(PublishNowCmd {
            draft,
            idempotency_key: "terminal-command-1".into(),
        })
        .await
        .unwrap();
        let mut tx = store.content_tx().await.unwrap();
        let mut done = first.command;
        done.status = PublicationCommandStatus::Done;
        tx.save_publication_command(&done).await.unwrap();
        tx.commit().await.unwrap();
        let next = PublishNow {
            store: &store,
            actor: curator(),
        }
        .execute(PublishNowCmd {
            draft,
            idempotency_key: "terminal-command-2".into(),
        })
        .await
        .unwrap();
        assert!(!next.replayed);
        assert_ne!(next.command.id, done.id);
    }
}
