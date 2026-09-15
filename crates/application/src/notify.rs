//! Pure fanout policy over read ports. Cursor locking, bulk persistence, and
//! post-commit delivery remain adapter responsibilities.

use serde::Deserialize;
use serde_json::json;

use crate::error::StoreError;
use crate::model::{CommentId, MarketId, NewNotification, OutboxEvent, SocialConfig, UserId};
use crate::ports::NotifyReader;

#[derive(Debug, Clone, PartialEq)]
pub struct MaterializeResult {
    pub notifications: Vec<NewNotification>,
    pub dropped_mentions: u32,
}

pub struct NotifyPolicy<'a> {
    pub social: SocialConfig,
    pub admin_handles: &'a [String],
}

#[derive(Deserialize)]
struct ResolutionPayload {
    redemption_yes_micro: i64,
    redemption_no_micro: i64,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct CommentPostedPayload {
    market_id: uuid::Uuid,
    comment_id: uuid::Uuid,
    author_id: uuid::Uuid,
    parent_author_id: Option<uuid::Uuid>,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct MentionPayload {
    comment_id: uuid::Uuid,
    market_id: uuid::Uuid,
    mentioned_user_id: uuid::Uuid,
    author_id: uuid::Uuid,
}

#[derive(Deserialize)]
struct RepPayload {
    user_id: uuid::Uuid,
    market_id: uuid::Uuid,
    rep_micro: i64,
    tier: u8,
    previous_tier: u8,
}

fn notification(
    user: UserId,
    notification_type: &str,
    market: Option<MarketId>,
    payload: serde_json::Value,
    event: &OutboxEvent,
    now: time::OffsetDateTime,
) -> NewNotification {
    NewNotification {
        user,
        notification_type: notification_type.to_string(),
        market,
        payload,
        source_seq: event.seq,
        created_at: now,
    }
}

/// Maps one immutable outbox event to zero or more notification rows.
///
/// # Errors
/// Malformed recognized payloads are invariant failures; reader failures are
/// propagated so the cursor transaction rolls back and retries the event.
#[allow(clippy::too_many_lines)]
pub async fn materialize<R: NotifyReader + ?Sized>(
    reader: &mut R,
    event: &OutboxEvent,
    now: time::OffsetDateTime,
    policy: &NotifyPolicy<'_>,
) -> Result<MaterializeResult, StoreError> {
    let mut notifications = Vec::new();
    let mut dropped_mentions = 0;
    match event.event_type.as_str() {
        "MarketResolved" | "MarketVoided" => {
            let voided = event.event_type == "MarketVoided";
            let payload: ResolutionPayload = serde_json::from_value(event.payload.clone())
                .map_err(|_| StoreError::Invariant("invalid resolution notification event"))?;
            let market = MarketId(event.aggregate_id);
            for recipient in reader.resolution_recipients(market, voided).await? {
                let side = recipient.side.map(|side| match side {
                    domain::amm::Side::Yes => "yes",
                    domain::amm::Side::No => "no",
                });
                let row_payload = json!({
                    "payout_total_micro": recipient.payout_total.0,
                    "redemption_yes_micro": payload.redemption_yes_micro,
                    "redemption_no_micro": payload.redemption_no_micro,
                    "realized_delta_micro": recipient.realized_delta.0,
                    "score_bp": recipient.score_bp,
                    "side": side,
                });
                let notification_type = if voided {
                    "resolution_void"
                } else if recipient.held {
                    "resolution_trade"
                } else {
                    "resolution_vote"
                };
                notifications.push(notification(
                    recipient.user,
                    notification_type,
                    Some(market),
                    row_payload,
                    event,
                    now,
                ));
            }
        }
        "CommentPosted" => {
            let payload: CommentPostedPayload = serde_json::from_value(event.payload.clone())
                .map_err(|_| StoreError::Invariant("invalid comment notification event"))?;
            if let Some(parent_author) = payload.parent_author_id {
                if parent_author != payload.author_id {
                    notifications.push(notification(
                        UserId(parent_author),
                        "comment_reply",
                        Some(MarketId(payload.market_id)),
                        json!({
                            "comment_id": payload.comment_id.to_string(),
                            "author_id": payload.author_id.to_string(),
                        }),
                        event,
                        now,
                    ));
                }
            }
        }
        "MentionCreated" => {
            let payload: MentionPayload = serde_json::from_value(event.payload.clone())
                .map_err(|_| StoreError::Invariant("invalid mention notification event"))?;
            let recipient = UserId(payload.mentioned_user_id);
            let parent_author = reader.parent_author(CommentId(payload.comment_id)).await?;
            if recipient != UserId(payload.author_id) && parent_author != Some(recipient) {
                let since = now - time::Duration::hours(1);
                let recent = reader
                    .notification_count_since(recipient, "mention", since)
                    .await?;
                if recent >= policy.social.mention_notifs_per_hour {
                    dropped_mentions = 1;
                } else {
                    notifications.push(notification(
                        recipient,
                        "mention",
                        Some(MarketId(payload.market_id)),
                        json!({
                            "comment_id": payload.comment_id.to_string(),
                            "author_id": payload.author_id.to_string(),
                        }),
                        event,
                        now,
                    ));
                }
            }
        }
        "RepUpdated" => {
            let payload: RepPayload = serde_json::from_value(event.payload.clone())
                .map_err(|_| StoreError::Invariant("invalid reputation notification event"))?;
            notifications.push(notification(
                UserId(payload.user_id),
                "rep_tier_change",
                Some(MarketId(payload.market_id)),
                json!({
                    "rep_micro": payload.rep_micro,
                    "tier": payload.tier,
                    "previous_tier": payload.previous_tier,
                }),
                event,
                now,
            ));
        }
        "CuratorNeeded" => {
            let users = reader.users_by_handles(policy.admin_handles).await?;
            for user in users {
                notifications.push(notification(
                    user,
                    "curator_needed",
                    Some(MarketId(event.aggregate_id)),
                    json!({"market_id": event.aggregate_id.to_string()}),
                    event,
                    now,
                ));
            }
        }
        "WithdrawalSettled" => {
            let user_id = event
                .payload
                .get("user_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .ok_or(StoreError::Invariant(
                    "invalid withdrawal notification event",
                ))?;
            notifications.push(notification(
                UserId(user_id),
                "withdrawal_settled",
                None,
                event.payload.clone(),
                event,
                now,
            ));
        }
        _ => {}
    }
    Ok(MaterializeResult {
        notifications,
        dropped_mentions,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use domain::money::MicroUsd;

    use super::*;
    use crate::model::ResolutionRecipient;

    #[derive(Default)]
    struct Reader {
        recipients: Vec<ResolutionRecipient>,
        parent_author: Option<UserId>,
        admins: Vec<UserId>,
        mention_count: u32,
    }

    #[async_trait]
    impl NotifyReader for Reader {
        async fn lock_notifier_cursor(&mut self) -> Result<i64, StoreError> {
            Ok(0)
        }

        async fn outbox_events_after(
            &mut self,
            _last_seq: i64,
            _limit: u32,
        ) -> Result<Vec<OutboxEvent>, StoreError> {
            Ok(Vec::new())
        }

        async fn resolution_recipients(
            &mut self,
            _market: MarketId,
            _voided: bool,
        ) -> Result<Vec<ResolutionRecipient>, StoreError> {
            Ok(self.recipients.clone())
        }

        async fn parent_author(
            &mut self,
            _comment: CommentId,
        ) -> Result<Option<UserId>, StoreError> {
            Ok(self.parent_author)
        }

        async fn users_by_handles(
            &mut self,
            _handles: &[String],
        ) -> Result<Vec<UserId>, StoreError> {
            Ok(self.admins.clone())
        }

        async fn notification_count_since(
            &mut self,
            _user: UserId,
            _notification_type: &str,
            _since: time::OffsetDateTime,
        ) -> Result<u32, StoreError> {
            Ok(self.mention_count)
        }
    }

    fn event(
        event_type: &str,
        aggregate_id: uuid::Uuid,
        payload: serde_json::Value,
    ) -> OutboxEvent {
        OutboxEvent {
            seq: 7,
            event_type: event_type.to_string(),
            aggregate_type: "market".to_string(),
            aggregate_id,
            payload,
        }
    }

    fn policy() -> NotifyPolicy<'static> {
        NotifyPolicy {
            social: SocialConfig {
                mention_notifs_per_hour: 1,
                ..SocialConfig::default()
            },
            admin_handles: &[],
        }
    }

    #[tokio::test]
    async fn resolution_fanout_collapses_holder_voter_and_preserves_payload() {
        let market = uuid::Uuid::new_v4();
        let holder = UserId(uuid::Uuid::new_v4());
        let voter = UserId(uuid::Uuid::new_v4());
        let mut reader = Reader {
            recipients: vec![
                ResolutionRecipient {
                    user: holder,
                    held: true,
                    payout_total: MicroUsd(90),
                    realized_delta: MicroUsd(20),
                    voted: true,
                    score_bp: Some(8_000),
                    side: Some(domain::amm::Side::Yes),
                },
                ResolutionRecipient {
                    user: voter,
                    held: false,
                    payout_total: MicroUsd(0),
                    realized_delta: MicroUsd(0),
                    voted: true,
                    score_bp: Some(4_000),
                    side: Some(domain::amm::Side::No),
                },
            ],
            ..Reader::default()
        };
        let result = materialize(
            &mut reader,
            &event(
                "MarketResolved",
                market,
                json!({"redemption_yes_micro": 600_000, "redemption_no_micro": 400_000}),
            ),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert_eq!(result.notifications.len(), 2);
        assert_eq!(
            result.notifications[0].notification_type,
            "resolution_trade"
        );
        assert_eq!(result.notifications[0].payload["payout_total_micro"], 90);
        assert_eq!(result.notifications[0].payload["score_bp"], 8_000);
        assert_eq!(result.notifications[0].payload["side"], "yes");
        assert_eq!(result.notifications[1].notification_type, "resolution_vote");

        let voided = materialize(
            &mut reader,
            &event(
                "MarketVoided",
                market,
                json!({"redemption_yes_micro": 500_000, "redemption_no_micro": 500_000}),
            ),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert!(voided
            .notifications
            .iter()
            .all(|row| row.notification_type == "resolution_void"));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn reply_mentions_brake_tier_admin_and_unknown_paths_are_explicit() {
        let market = uuid::Uuid::new_v4();
        let author = UserId(uuid::Uuid::new_v4());
        let parent = UserId(uuid::Uuid::new_v4());
        let mentioned = UserId(uuid::Uuid::new_v4());
        let comment = uuid::Uuid::new_v4();
        let admin = UserId(uuid::Uuid::new_v4());
        let mut reader = Reader {
            parent_author: Some(parent),
            admins: vec![admin],
            ..Reader::default()
        };
        let reply = materialize(
            &mut reader,
            &event(
                "CommentPosted",
                comment,
                json!({
                    "market_id": market,
                    "comment_id": comment,
                    "author_id": author.0,
                    "parent_author_id": parent.0,
                }),
            ),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert_eq!(reply.notifications[0].user, parent);
        assert_eq!(reply.notifications[0].notification_type, "comment_reply");
        let self_reply = materialize(
            &mut reader,
            &event(
                "CommentPosted",
                comment,
                json!({
                    "market_id": market,
                    "comment_id": comment,
                    "author_id": author.0,
                    "parent_author_id": author.0,
                }),
            ),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert!(self_reply.notifications.is_empty());
        let top_level = materialize(
            &mut reader,
            &event(
                "CommentPosted",
                comment,
                json!({
                    "market_id": market,
                    "comment_id": comment,
                    "author_id": author.0,
                    "parent_author_id": null,
                }),
            ),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert!(top_level.notifications.is_empty());

        for (recipient, expected) in [(parent, 0), (author, 0), (mentioned, 1)] {
            let mention = materialize(
                &mut reader,
                &event(
                    "MentionCreated",
                    comment,
                    json!({
                        "market_id": market,
                        "comment_id": comment,
                        "mentioned_user_id": recipient.0,
                        "author_id": author.0,
                    }),
                ),
                time::OffsetDateTime::UNIX_EPOCH,
                &policy(),
            )
            .await
            .unwrap();
            assert_eq!(mention.notifications.len(), expected);
        }
        reader.mention_count = 1;
        let braked = materialize(
            &mut reader,
            &event(
                "MentionCreated",
                comment,
                json!({
                    "market_id": market,
                    "comment_id": comment,
                    "mentioned_user_id": mentioned.0,
                    "author_id": author.0,
                }),
            ),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert_eq!(
            (braked.notifications.len(), braked.dropped_mentions),
            (0, 1)
        );

        let rep = materialize(
            &mut reader,
            &event(
                "RepUpdated",
                author.0,
                json!({
                    "user_id": author.0,
                    "market_id": market,
                    "rep_micro": 900_000,
                    "tier": 4,
                    "previous_tier": 3,
                }),
            ),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert_eq!(rep.notifications[0].notification_type, "rep_tier_change");
        assert_eq!(rep.notifications[0].payload["previous_tier"], 3);

        let curator = materialize(
            &mut reader,
            &event("CuratorNeeded", market, json!({})),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert_eq!(curator.notifications[0].user, admin);
        let settled = materialize(
            &mut reader,
            &event(
                "WithdrawalSettled",
                author.0,
                json!({
                    "user_id": author.0.to_string(),
                    "amount_micro": 5_000_000,
                }),
            ),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap();
        assert_eq!(
            settled.notifications[0].notification_type,
            "withdrawal_settled"
        );
        assert!(materialize(
            &mut reader,
            &event("Unknown", market, json!({})),
            time::OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .await
        .unwrap()
        .notifications
        .is_empty());
    }

    #[tokio::test]
    async fn recognized_payloads_fail_closed_and_reader_surface_is_total() {
        let mut reader = Reader::default();
        for name in [
            "MarketResolved",
            "CommentPosted",
            "MentionCreated",
            "RepUpdated",
            "WithdrawalSettled",
        ] {
            assert!(materialize(
                &mut reader,
                &event(name, uuid::Uuid::new_v4(), json!({})),
                time::OffsetDateTime::UNIX_EPOCH,
                &policy(),
            )
            .await
            .is_err());
        }
        assert_eq!(reader.lock_notifier_cursor().await.unwrap(), 0);
        assert!(reader.outbox_events_after(0, 128).await.unwrap().is_empty());
    }
}
