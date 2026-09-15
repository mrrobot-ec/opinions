//! `CreateUser` — bootstrap identity (codex B5): inserts a user and
//! optionally links a messaging channel (`link_channel("imessage", phone)`),
//! the phone→user identity the converse service resolves via
//! [`crate::ports::MarketQueries::user_by_channel`]. Unknown numbers get the
//! onboarding reply — accounts are never auto-created by the router.
//!
//! With a channel link, retries return the already-linked user. Without one
//! there is no natural key, so a repeated call creates a second user — like
//! any POST /users.

use serde_json::json;

use crate::error::{AppError, StoreError};
use crate::model::{AdminContext, Event, RepConfig, UserId};
use crate::ops::audit::audit_for;
use crate::ports::{MarketQueries, Store};

#[derive(Debug, Clone)]
pub struct CreateUserCmd {
    pub handle: String,
    /// Optional (channel, address) link, e.g. `("imessage", "+15550100")`.
    pub channel: Option<(String, String)>,
    pub idempotency_key: String,
    /// D28 identity-policy overrides: honored ONLY when the composition
    /// passes them through the two-factor staging arm; audited whenever an
    /// admin actor is present. `None` on every production path.
    pub created_at_override: Option<time::OffsetDateTime>,
    /// Rep seed in micro; the tier derives from [`RepConfig`].
    pub rep_seed_micro: Option<i64>,
}

impl CreateUserCmd {
    /// The production shape: no overrides.
    #[must_use]
    pub fn plain(handle: &str, channel: Option<(String, String)>, idempotency_key: &str) -> Self {
        Self {
            handle: handle.to_string(),
            channel,
            idempotency_key: idempotency_key.to_string(),
            created_at_override: None,
            rep_seed_micro: None,
        }
    }
}

pub struct CreateUser<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store + MarketQueries> CreateUser<'_, S> {
    /// # Errors
    /// [`StoreError::Conflict`] (via [`AppError::Store`]) when the channel
    /// address is already linked; store failures.
    pub async fn execute(&self, cmd: CreateUserCmd) -> Result<UserId, AppError> {
        self.execute_as(cmd, &AdminContext::Machine, RepConfig::default())
            .await
    }

    /// D26/D28 threading: staging-arm overrides ride an actor and are
    /// audited in the SAME transaction (admin actors only; the public
    /// signup path stays machine + override-free).
    ///
    /// # Errors
    /// As [`Self::execute`].
    pub async fn execute_as(
        &self,
        cmd: CreateUserCmd,
        actor: &AdminContext,
        rep_config: RepConfig,
    ) -> Result<UserId, AppError> {
        if let Some((channel, address)) = &cmd.channel {
            if let Some(existing) = self.store.user_by_channel(channel, address).await? {
                return Ok(existing);
            }
        }
        self.create_new(cmd, actor, rep_config).await
    }

    async fn create_new(
        &self,
        cmd: CreateUserCmd,
        actor: &AdminContext,
        rep_config: RepConfig,
    ) -> Result<UserId, AppError> {
        let mut tx = self.store.bootstrap_tx().await?;
        tx.serialize_key(&cmd.idempotency_key).await?;
        let user = tx.insert_user(&cmd.handle).await?;
        if let Some((channel, address)) = &cmd.channel {
            if let Err(error) = tx.link_channel(user, channel, address).await {
                drop(tx);
                return recover_channel_link_race(self.store, channel, address, error).await;
            }
        }
        if let Some(created_at) = cmd.created_at_override {
            tx.set_user_created_at(user, created_at).await?;
        }
        if let Some(rep_micro) = cmd.rep_seed_micro {
            let tier = domain::reputation::tier_for(rep_micro, &rep_config.tier_thresholds_micro);
            tx.seed_reputation(user, rep_micro, tier).await?;
        }
        if cmd.created_at_override.is_some() || cmd.rep_seed_micro.is_some() {
            if let Some(row) = audit_for(
                actor,
                "create_user_override",
                format!("user:{}", user.0),
                None,
                Some(json!({
                    "created_at_override": cmd.created_at_override.map(|at| at.to_string()),
                    "rep_seed_micro": cmd.rep_seed_micro,
                })),
                None,
            ) {
                tx.audit_insert(row).await?;
            }
        }
        tx.append(Event {
            event_type: "UserCreated",
            aggregate_type: "user",
            aggregate_id: user.0,
            payload: json!({
                "handle": cmd.handle,
                "channel": cmd.channel.as_ref().map(|(c, a)| json!({
                    "channel": c,
                    "address": a,
                })),
            }),
        })
        .await?;
        tx.commit().await?;
        Ok(user)
    }
}

async fn recover_channel_link_race<Q: MarketQueries + ?Sized>(
    queries: &Q,
    channel: &str,
    address: &str,
    error: StoreError,
) -> Result<UserId, AppError> {
    if error == StoreError::Conflict("channel link") {
        if let Some(existing) = queries.user_by_channel(channel, address).await? {
            return Ok(existing);
        }
    }
    Err(error.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::InMemoryStore;
    use crate::ports::MarketQueries;

    #[tokio::test]
    async fn creates_user_with_channel_link() {
        let store = InMemoryStore::new();
        let uc = CreateUser { store: &store };
        let user = uc
            .execute(CreateUserCmd::plain(
                "andres",
                Some(("imessage".to_string(), "+15550100".to_string())),
                "user-1",
            ))
            .await
            .unwrap();
        assert_eq!(store.user_handle(user).as_deref(), Some("andres"));
        assert_eq!(store.user_rep(user).await.unwrap().rep_micro, 0);
        assert_eq!(store.user_rep(user).await.unwrap().tier, 0);
        assert_eq!(
            store
                .user_by_channel("imessage", "+15550100")
                .await
                .unwrap(),
            Some(user)
        );
        let events = store.outbox();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "UserCreated");
    }

    #[tokio::test]
    async fn duplicate_channel_link_replays_the_existing_user() {
        let store = InMemoryStore::new();
        let uc = CreateUser { store: &store };
        let first = uc
            .execute(CreateUserCmd::plain(
                "first",
                Some(("imessage".to_string(), "+15550100".to_string())),
                "user-1",
            ))
            .await
            .unwrap();
        let replay = uc
            .execute(CreateUserCmd::plain(
                "second",
                Some(("imessage".to_string(), "+15550100".to_string())),
                "user-2",
            ))
            .await
            .unwrap();
        assert_eq!(replay, first);
        assert_eq!(
            store.outbox().len(),
            1,
            "retry creates no second user event"
        );
        assert_eq!(
            store
                .user_by_channel("imessage", "+15550100")
                .await
                .unwrap(),
            Some(first)
        );
    }

    #[tokio::test]
    async fn user_without_channel_is_created_bare() {
        let store = InMemoryStore::new();
        let uc = CreateUser { store: &store };
        let user = uc
            .execute(CreateUserCmd::plain("bare", None, "user-bare"))
            .await
            .unwrap();
        assert_eq!(store.user_handle(user).as_deref(), Some("bare"));
    }

    #[tokio::test]
    async fn staging_overrides_seed_age_and_reputation_with_an_atomic_audit() {
        use crate::model::AdminRole;
        use crate::ports::OpsQueries;

        let store = InMemoryStore::new();
        let created_at = time::OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap();
        let actor = AdminContext::Admin {
            token_digest: "staging-admin".into(),
            role: AdminRole::Ops,
        };
        let user = CreateUser { store: &store }
            .execute_as(
                CreateUserCmd {
                    handle: "seeded".into(),
                    channel: Some(("imessage".into(), "+15550199".into())),
                    idempotency_key: "seeded-user".into(),
                    created_at_override: Some(created_at),
                    rep_seed_micro: Some(250),
                },
                &actor,
                RepConfig {
                    tier_thresholds_micro: [100, 200, 300, 400],
                    ..RepConfig::default()
                },
            )
            .await
            .unwrap();

        let rep = store.user_rep(user).await.unwrap();
        assert_eq!((rep.rep_micro, rep.tier), (250, 2));
        let audit = store.audit_page(None, 10).await.unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action.action, "create_user_override");
        assert_eq!(audit[0].action.actor_token_digest, "staging-admin");
        assert_eq!(store.outbox().len(), 1);

        let machine = CreateUser { store: &store }
            .execute_as(
                CreateUserCmd {
                    handle: "machine-seeded".into(),
                    channel: None,
                    idempotency_key: "machine-seeded-user".into(),
                    created_at_override: Some(created_at),
                    rep_seed_micro: None,
                },
                &AdminContext::Machine,
                RepConfig::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            store.user_handle(machine).as_deref(),
            Some("machine-seeded")
        );
        assert_eq!(store.audit_page(None, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_lost_channel_link_race_recovers_only_the_committed_winner() {
        let store = InMemoryStore::new();
        let winner = CreateUser { store: &store }
            .execute(CreateUserCmd::plain(
                "winner",
                Some(("imessage".into(), "+15550198".into())),
                "winner-key",
            ))
            .await
            .unwrap();
        assert_eq!(
            recover_channel_link_race(
                &store,
                "imessage",
                "+15550198",
                StoreError::Conflict("channel link"),
            )
            .await
            .unwrap(),
            winner
        );
        assert!(recover_channel_link_race(
            &store,
            "imessage",
            "+15550000",
            StoreError::Conflict("channel link"),
        )
        .await
        .is_err());
        assert!(recover_channel_link_race(
            &store,
            "imessage",
            "+15550198",
            StoreError::Backend("boom".into()),
        )
        .await
        .is_err());
        let raced = CreateUser { store: &store }
            .create_new(
                CreateUserCmd::plain(
                    "loser",
                    Some(("imessage".into(), "+15550198".into())),
                    "loser-key",
                ),
                &AdminContext::Machine,
                RepConfig::default(),
            )
            .await
            .unwrap();
        assert_eq!(raced, winner);
    }
}
