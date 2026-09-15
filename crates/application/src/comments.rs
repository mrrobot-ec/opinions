//! Comment commands with one explicit lock chain: request key, user (when
//! required), market, then comment row. Counters only move after their unique
//! fact row was inserted in the same transaction.

use domain::moderation::{ModerationRules, Screen};
use serde_json::json;

use crate::error::AppError;
use crate::model::{
    CommentId, CommentReceipt, CommentReportReceipt, CommentVoteReceipt, Event, MarketId,
    ModerationStatus, NewComment, SocialConfig, UserId,
};
use crate::ports::{Clock, Store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostCommentCmd {
    pub comment: CommentId,
    pub market: MarketId,
    pub author: UserId,
    pub parent: Option<CommentId>,
    pub body: String,
}

pub struct PostComment<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub config: SocialConfig,
}

impl<S: Store, C: Clock> PostComment<'_, S, C> {
    /// Posts one comment and all mention facts atomically.
    ///
    /// # Errors
    /// Returns a typed business rejection or store failure with no partial
    /// comment, reply counter, or event writes.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(&self, cmd: PostCommentCmd) -> Result<CommentReceipt, AppError> {
        let mut tx = self.store.comment_tx().await?;
        tx.serialize_key(&format!("comment:{}", cmd.comment.0))
            .await?;
        if let Some(row) = tx.comment(cmd.comment).await? {
            if row.market != cmd.market || row.author != cmd.author || row.parent != cmd.parent {
                return Err(AppError::Store(crate::error::StoreError::Conflict(
                    "comment id",
                )));
            }
            return Ok(CommentReceipt {
                comment: row.id,
                moderation_status: row.moderation_status,
                replayed: true,
            });
        }
        tx.lock_user(cmd.author).await?;
        tx.market_for_update(cmd.market).await?;
        let (depth, parent_author) = if let Some(parent_id) = cmd.parent {
            let parent = tx.comment_for_update(parent_id).await?;
            if parent.market != cmd.market {
                return Err(AppError::Store(crate::error::StoreError::Conflict(
                    "comment parent market",
                )));
            }
            if parent.moderation_status != ModerationStatus::Visible {
                return Err(AppError::CommentNotVisible);
            }
            let depth = parent.depth.checked_add(1).ok_or(AppError::ThreadTooDeep)?;
            if depth > self.config.max_thread_depth {
                return Err(AppError::ThreadTooDeep);
            }
            (depth, Some(parent.author))
        } else {
            (0, None)
        };
        let author_handle = tx.handle(cmd.author).await?;
        let now = self.clock.now();
        let window = seconds(self.config.spam_window_secs)?;
        let hash = domain::moderation::body_hash(&cmd.body);
        let same_hash = tx.recent_same_hash(cmd.author, &hash, now - window).await?;
        let posts = tx.author_posts_since(cmd.author, now - window).await?;
        let rules = ModerationRules {
            max_comment_len_chars: self.config.max_comment_len_chars,
            max_links: self.config.max_links,
            max_comments_per_window: self.config.max_comments_per_window,
        };
        let moderation_status =
            match domain::moderation::screen(&cmd.body, same_hash, posts, &rules) {
                Screen::Visible => ModerationStatus::Visible,
                Screen::Shadow(_) => ModerationStatus::Shadow,
                Screen::Blocked(reason) => return Err(AppError::CommentBlocked(reason)),
            };
        let body = domain::moderation::sanitize_for_storage(&cmd.body);
        tx.insert_comment(NewComment {
            id: cmd.comment,
            market: cmd.market,
            author: cmd.author,
            parent: cmd.parent,
            body: body.clone(),
            body_hash: hash,
            moderation_status,
            depth,
            created_at: now,
        })
        .await?;
        if let Some(parent) = cmd.parent {
            tx.bump_reply_count(parent).await?;
        }
        tx.append(Event {
            event_type: "CommentPosted",
            aggregate_type: "comment",
            aggregate_id: cmd.comment.0,
            payload: json!({
                "market_id": cmd.market.0.to_string(),
                "comment_id": cmd.comment.0.to_string(),
                "parent_id": cmd.parent.map(|id| id.0.to_string()),
                "author_id": cmd.author.0.to_string(),
                "author_handle": author_handle,
                "parent_author_id": parent_author.map(|id| id.0.to_string()),
            }),
        })
        .await?;
        for handle in domain::mentions::parse_mentions(&body, self.config.max_mentions) {
            if let Some(mentioned) = tx.user_by_handle(&handle).await? {
                tx.append(Event {
                    event_type: "MentionCreated",
                    aggregate_type: "comment",
                    aggregate_id: cmd.comment.0,
                    payload: json!({
                        "comment_id": cmd.comment.0.to_string(),
                        "market_id": cmd.market.0.to_string(),
                        "mentioned_user_id": mentioned.0.to_string(),
                        "author_id": cmd.author.0.to_string(),
                    }),
                })
                .await?;
            }
        }
        tx.commit().await?;
        Ok(CommentReceipt {
            comment: cmd.comment,
            moderation_status,
            replayed: false,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteCommentCmd {
    pub comment: CommentId,
    pub user: UserId,
    pub value: i16,
}

pub struct VoteComment<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store> VoteComment<'_, S> {
    /// # Errors
    /// Rejects values other than -1/+1, duplicate votes, non-visible rows,
    /// and store failures.
    pub async fn execute(&self, cmd: VoteCommentCmd) -> Result<CommentVoteReceipt, AppError> {
        if !matches!(cmd.value, -1 | 1) {
            return Err(AppError::InvalidCommentVote);
        }
        let mut tx = self.store.comment_tx().await?;
        tx.serialize_key(&format!("comment-vote:{}:{}", cmd.comment.0, cmd.user.0))
            .await?;
        let comment = tx.comment_for_update(cmd.comment).await?;
        if comment.moderation_status != ModerationStatus::Visible {
            return Err(AppError::CommentNotVisible);
        }
        if !tx
            .insert_comment_vote(cmd.comment, cmd.user, cmd.value)
            .await?
        {
            return Err(AppError::CommentAlreadyVoted);
        }
        let score = tx.adjust_comment_score(cmd.comment, cmd.value).await?;
        tx.commit().await?;
        Ok(CommentVoteReceipt {
            comment: cmd.comment,
            score,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportCommentCmd {
    pub comment: CommentId,
    pub reporter: UserId,
}

pub struct ReportComment<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub config: SocialConfig,
}

impl<S: Store, C: Clock> ReportComment<'_, S, C> {
    /// # Errors
    /// Enforces the reporter quality floor and velocity under the reporter
    /// lock, then serializes threshold mutation on the comment row.
    pub async fn execute(&self, cmd: ReportCommentCmd) -> Result<CommentReportReceipt, AppError> {
        let mut tx = self.store.comment_tx().await?;
        tx.serialize_key(&format!(
            "comment-report:{}:{}",
            cmd.comment.0, cmd.reporter.0
        ))
        .await?;
        tx.lock_user(cmd.reporter).await?;
        let now = self.clock.now();
        let min_age = seconds(self.config.reporter_min_age_secs)?;
        let created_at = tx.user_created_at(cmd.reporter).await?;
        let tier = tx.user_tier(cmd.reporter).await?;
        if now - created_at < min_age && tier < self.config.reporter_min_tier {
            return Err(AppError::ReporterNotQualified);
        }
        let report_window = seconds(self.config.report_window_secs)?;
        if tx
            .reporter_reports_since(cmd.reporter, now - report_window)
            .await?
            >= self.config.max_reports_per_window
        {
            return Err(AppError::ReportVelocityExceeded);
        }
        let comment = tx.comment_for_update(cmd.comment).await?;
        if comment.moderation_status != ModerationStatus::Visible {
            return Err(AppError::CommentNotVisible);
        }
        if !tx
            .insert_comment_report(cmd.comment, cmd.reporter, now)
            .await?
        {
            return Ok(CommentReportReceipt {
                comment: cmd.comment,
                report_count: tx.comment_report_count(cmd.comment).await?,
                shadowed: false,
                replayed: true,
            });
        }
        let report_count = tx.comment_report_count(cmd.comment).await?;
        let shadowed = report_count >= self.config.report_shadow_threshold;
        if shadowed {
            tx.set_comment_status(cmd.comment, ModerationStatus::Shadow)
                .await?;
            tx.append(Event {
                event_type: "CommentShadowed",
                aggregate_type: "comment",
                aggregate_id: cmd.comment.0,
                payload: json!({
                    "comment_id": cmd.comment.0.to_string(),
                    "market_id": comment.market.0.to_string(),
                    "report_count": report_count,
                }),
            })
            .await?;
        }
        tx.commit().await?;
        Ok(CommentReportReceipt {
            comment: cmd.comment,
            report_count,
            shadowed,
            replayed: false,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModerateCommentAction {
    Restore,
    Shadow,
    Block,
}

pub struct ModerateComment<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store> ModerateComment<'_, S> {
    /// # Errors
    /// Returns store failures; restore atomically resets the report epoch.
    pub async fn execute(
        &self,
        comment: CommentId,
        action: ModerateCommentAction,
    ) -> Result<ModerationStatus, AppError> {
        self.execute_as(comment, action, &crate::model::AdminContext::Machine)
            .await
    }

    /// D26 actor threading: the moderation effect and its audit fact commit
    /// in ONE transaction for admin actors; machine paths stay audit-free.
    ///
    /// # Errors
    /// As [`Self::execute`].
    pub async fn execute_as(
        &self,
        comment: CommentId,
        action: ModerateCommentAction,
        actor: &crate::model::AdminContext,
    ) -> Result<ModerationStatus, AppError> {
        let mut tx = self.store.comment_tx().await?;
        tx.serialize_key(&format!("comment-moderate:{}:{action:?}", comment.0))
            .await?;
        let row = tx.comment_for_update(comment).await?;
        let status = match action {
            ModerateCommentAction::Restore => ModerationStatus::Visible,
            ModerateCommentAction::Shadow => ModerationStatus::Shadow,
            ModerateCommentAction::Block => ModerationStatus::Blocked,
        };
        tx.set_comment_status(comment, status).await?;
        if action == ModerateCommentAction::Restore {
            tx.delete_comment_reports(comment).await?;
        }
        tx.append(Event {
            event_type: "CommentModerated",
            aggregate_type: "comment",
            aggregate_id: comment.0,
            payload: json!({
                "comment_id": comment.0.to_string(),
                "market_id": row.market.0.to_string(),
                "status": match status {
                    ModerationStatus::Visible => "visible",
                    ModerationStatus::Shadow => "shadow",
                    ModerationStatus::Blocked => "blocked",
                },
            }),
        })
        .await?;
        if let Some(audit) = crate::ops::audit::audit_for(
            actor,
            "moderate_comment",
            format!("comment:{}", comment.0),
            Some(json!({ "status": format!("{:?}", row.moderation_status) })),
            Some(json!({ "status": format!("{status:?}"), "action": format!("{action:?}") })),
            None,
        ) {
            tx.audit_insert(audit).await?;
        }
        tx.commit().await?;
        Ok(status)
    }
}

fn seconds(value: u64) -> Result<time::Duration, AppError> {
    i64::try_from(value)
        .map(time::Duration::seconds)
        .map_err(|_| AppError::Overflow)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{AdminContext, AdminRole, CommentCursor, CommentSort};
    use crate::ports::{OpsQueries, SocialQueries};
    use domain::amm::Side;
    use domain::market::MarketState;
    use domain::money::{BasisPoints, MicroShares};
    use time::{Duration, OffsetDateTime};

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn fixture() -> (InMemoryStore, FakeClock, MarketId, UserId, UserId) {
        let store = InMemoryStore::new();
        let market = store
            .add_market(
                "social",
                MarketState::Live,
                t0() + Duration::days(1),
                t0() + Duration::hours(20),
                MicroShares(1_000_000),
                BasisPoints(100),
            )
            .unwrap()
            .id;
        let alice = store.add_user("alice", t0() - Duration::days(10), 1);
        let bob = store.add_user("bob_2", t0() - Duration::days(10), 1);
        (store, FakeClock::at(t0()), market, alice, bob)
    }

    fn post(
        market: MarketId,
        author: UserId,
        parent: Option<CommentId>,
        body: &str,
    ) -> PostCommentCmd {
        PostCommentCmd {
            comment: CommentId(uuid::Uuid::new_v4()),
            market,
            author,
            parent,
            body: body.to_string(),
        }
    }

    #[tokio::test]
    async fn post_replay_mentions_and_reply_counter_are_atomic() {
        let (store, clock, market, alice, bob) = fixture();
        let usecase = PostComment {
            store: &store,
            clock: &clock,
            config: SocialConfig::default(),
        };
        let root = post(market, alice, None, "hello @bob_2 @unknown");
        let first = usecase.execute(root.clone()).await.unwrap();
        assert_eq!(first.moderation_status, ModerationStatus::Visible);
        assert!(!first.replayed);
        let replay = usecase.execute(root.clone()).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.comment, first.comment);

        clock.advance(Duration::seconds(1));
        let reply_receipt = usecase
            .execute(post(market, bob, Some(root.comment), "reply"))
            .await
            .unwrap();
        let page = store
            .comments(market, CommentSort::Recent, None, 10, None, clock.now())
            .await
            .unwrap();
        assert_eq!(page.comments.len(), 2);
        let root_row = page
            .comments
            .iter()
            .find(|row| row.row.id == root.comment)
            .unwrap();
        assert_eq!(root_row.row.reply_count, 1);
        assert_eq!(reply_receipt.moderation_status, ModerationStatus::Visible);
        let events = store.outbox();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type)
                .collect::<Vec<_>>(),
            vec!["CommentPosted", "MentionCreated", "CommentPosted"]
        );
        assert_eq!(events[1].payload["mentioned_user_id"], bob.0.to_string());
        assert_eq!(events[2].payload["parent_author_id"], alice.0.to_string());
    }

    #[tokio::test]
    async fn deterministic_screening_and_author_serialization_hold() {
        let (store, clock, market, alice, _) = fixture();
        let config = SocialConfig {
            max_comment_len_chars: 8,
            max_links: 0,
            max_comments_per_window: 3,
            ..SocialConfig::default()
        };
        let usecase = PostComment {
            store: &store,
            clock: &clock,
            config,
        };
        for (body, reason) in [("\u{200b}", "empty"), ("123456789", "too_long")] {
            assert_eq!(
                usecase.execute(post(market, alice, None, body)).await,
                Err(AppError::CommentBlocked(reason))
            );
        }
        let linked = usecase
            .execute(post(market, alice, None, "http://a"))
            .await
            .unwrap();
        assert_eq!(linked.moderation_status, ModerationStatus::Shadow);

        let one = post(market, alice, None, "same");
        let two = post(market, alice, None, "SAME ");
        let (a, b) = tokio::join!(usecase.execute(one), usecase.execute(two));
        let statuses = [a.unwrap().moderation_status, b.unwrap().moderation_status];
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == ModerationStatus::Visible)
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == ModerationStatus::Shadow)
                .count(),
            1
        );
        assert_eq!(
            usecase.execute(post(market, alice, None, "last")).await,
            Err(AppError::CommentBlocked("rate"))
        );
    }

    #[tokio::test]
    async fn parent_visibility_market_and_depth_are_revalidated() {
        let (store, clock, market, alice, bob) = fixture();
        let other = store
            .add_market(
                "other",
                MarketState::Live,
                t0() + Duration::days(1),
                t0() + Duration::hours(20),
                MicroShares(1_000_000),
                BasisPoints(100),
            )
            .unwrap()
            .id;
        let usecase = PostComment {
            store: &store,
            clock: &clock,
            config: SocialConfig {
                max_thread_depth: 1,
                ..SocialConfig::default()
            },
        };
        let root = post(market, alice, None, "root");
        usecase.execute(root.clone()).await.unwrap();
        assert!(matches!(
            usecase
                .execute(post(other, bob, Some(root.comment), "wrong"))
                .await,
            Err(AppError::Store(crate::error::StoreError::Conflict(_)))
        ));
        let child = post(market, bob, Some(root.comment), "child");
        usecase.execute(child.clone()).await.unwrap();
        assert_eq!(
            usecase
                .execute(post(market, alice, Some(child.comment), "deep"))
                .await,
            Err(AppError::ThreadTooDeep)
        );
        ModerateComment { store: &store }
            .execute_as(
                root.comment,
                ModerateCommentAction::Shadow,
                &AdminContext::Admin {
                    token_digest: "moderator-token".into(),
                    role: AdminRole::Curator,
                },
            )
            .await
            .unwrap();
        assert_eq!(store.audit_page(None, 10).await.unwrap().len(), 1);
        assert_eq!(
            usecase
                .execute(post(market, bob, Some(root.comment), "hidden"))
                .await,
            Err(AppError::CommentNotVisible)
        );

        let collision = PostCommentCmd {
            author: bob,
            ..root
        };
        assert!(matches!(
            usecase.execute(collision).await,
            Err(AppError::Store(crate::error::StoreError::Conflict(_)))
        ));
    }

    #[tokio::test]
    async fn votes_are_insert_first_unique_and_visible_only() {
        let (store, clock, market, alice, bob) = fixture();
        let command = post(market, alice, None, "vote me");
        PostComment {
            store: &store,
            clock: &clock,
            config: SocialConfig::default(),
        }
        .execute(command.clone())
        .await
        .unwrap();
        let usecase = VoteComment { store: &store };
        assert_eq!(
            usecase
                .execute(VoteCommentCmd {
                    comment: command.comment,
                    user: bob,
                    value: 0,
                })
                .await,
            Err(AppError::InvalidCommentVote)
        );
        let receipt = usecase
            .execute(VoteCommentCmd {
                comment: command.comment,
                user: bob,
                value: 1,
            })
            .await
            .unwrap();
        assert_eq!(receipt.score, 1);
        assert_eq!(
            usecase
                .execute(VoteCommentCmd {
                    comment: command.comment,
                    user: bob,
                    value: -1,
                })
                .await,
            Err(AppError::CommentAlreadyVoted)
        );
        ModerateComment { store: &store }
            .execute(command.comment, ModerateCommentAction::Block)
            .await
            .unwrap();
        assert_eq!(
            usecase
                .execute(VoteCommentCmd {
                    comment: command.comment,
                    user: alice,
                    value: -1,
                })
                .await,
            Err(AppError::CommentNotVisible)
        );
    }

    #[tokio::test]
    async fn reporter_floor_velocity_threshold_race_and_restore_epoch_hold() {
        let (store, clock, market, alice, _) = fixture();
        let comment = post(market, alice, None, "report me");
        PostComment {
            store: &store,
            clock: &clock,
            config: SocialConfig::default(),
        }
        .execute(comment.clone())
        .await
        .unwrap();
        let young = store.add_user("young", t0() - Duration::hours(1), 0);
        let tiered = store.add_user("tiered", t0() - Duration::hours(1), 1);
        let old = store.add_user("oldie", t0() - Duration::days(5), 0);
        let config = SocialConfig {
            report_shadow_threshold: 2,
            max_reports_per_window: 1,
            ..SocialConfig::default()
        };
        let reports = ReportComment {
            store: &store,
            clock: &clock,
            config,
        };
        assert_eq!(
            reports
                .execute(ReportCommentCmd {
                    comment: comment.comment,
                    reporter: young,
                })
                .await,
            Err(AppError::ReporterNotQualified)
        );
        let duplicate = reports
            .execute(ReportCommentCmd {
                comment: comment.comment,
                reporter: tiered,
            })
            .await
            .unwrap();
        assert!(!duplicate.replayed);
        let replay = reports
            .execute(ReportCommentCmd {
                comment: comment.comment,
                reporter: tiered,
            })
            .await;
        assert_eq!(replay, Err(AppError::ReportVelocityExceeded));

        let receipt = reports
            .execute(ReportCommentCmd {
                comment: comment.comment,
                reporter: old,
            })
            .await
            .unwrap();
        assert!(receipt.shadowed);
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == "CommentShadowed")
                .count(),
            1
        );
        assert_eq!(
            reports
                .execute(ReportCommentCmd {
                    comment: comment.comment,
                    reporter: store.add_user("third", t0() - Duration::days(5), 0),
                })
                .await,
            Err(AppError::CommentNotVisible)
        );

        ModerateComment { store: &store }
            .execute(comment.comment, ModerateCommentAction::Restore)
            .await
            .unwrap();
        let first_new = reports
            .execute(ReportCommentCmd {
                comment: comment.comment,
                reporter: store.add_user("fresh1", t0() - Duration::days(5), 0),
            })
            .await
            .unwrap();
        assert_eq!((first_new.report_count, first_new.shadowed), (1, false));
        let second_new = reports
            .execute(ReportCommentCmd {
                comment: comment.comment,
                reporter: store.add_user("fresh2", t0() - Duration::days(5), 0),
            })
            .await
            .unwrap();
        assert_eq!((second_new.report_count, second_new.shadowed), (2, true));
    }

    #[tokio::test]
    async fn report_duplicate_is_idempotent_before_velocity_limit() {
        let (store, clock, market, alice, bob) = fixture();
        let comment = post(market, alice, None, "report once");
        PostComment {
            store: &store,
            clock: &clock,
            config: SocialConfig::default(),
        }
        .execute(comment.clone())
        .await
        .unwrap();
        let reports = ReportComment {
            store: &store,
            clock: &clock,
            config: SocialConfig {
                max_reports_per_window: 2,
                ..SocialConfig::default()
            },
        };
        reports
            .execute(ReportCommentCmd {
                comment: comment.comment,
                reporter: bob,
            })
            .await
            .unwrap();
        let replay = reports
            .execute(ReportCommentCmd {
                comment: comment.comment,
                reporter: bob,
            })
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.report_count, 1);
    }

    #[tokio::test]
    async fn parent_shadow_race_and_threshold_race_have_one_winner() {
        let (store, clock, market, alice, bob) = fixture();
        let root = post(market, alice, None, "race root");
        PostComment {
            store: &store,
            clock: &clock,
            config: SocialConfig::default(),
        }
        .execute(root.clone())
        .await
        .unwrap();

        let mut blocker = store.comment_tx().await.unwrap();
        blocker.serialize_key("shadow-race").await.unwrap();
        blocker.comment_for_update(root.comment).await.unwrap();
        let racing_store = store.clone();
        let racing_post = post(market, bob, Some(root.comment), "racing reply");
        let reply = tokio::spawn(async move {
            let racing_clock = FakeClock::at(t0());
            PostComment {
                store: &racing_store,
                clock: &racing_clock,
                config: SocialConfig::default(),
            }
            .execute(racing_post)
            .await
        });
        tokio::task::yield_now().await;
        blocker
            .set_comment_status(root.comment, ModerationStatus::Shadow)
            .await
            .unwrap();
        blocker.commit().await.unwrap();
        assert_eq!(reply.await.unwrap(), Err(AppError::CommentNotVisible));

        ModerateComment { store: &store }
            .execute(root.comment, ModerateCommentAction::Restore)
            .await
            .unwrap();
        let r1 = store.add_user("racer1", t0() - Duration::days(5), 0);
        let r2 = store.add_user("racer2", t0() - Duration::days(5), 0);
        let config = SocialConfig {
            report_shadow_threshold: 2,
            ..SocialConfig::default()
        };
        let reports = ReportComment {
            store: &store,
            clock: &clock,
            config,
        };
        let (one, two) = tokio::join!(
            reports.execute(ReportCommentCmd {
                comment: root.comment,
                reporter: r1,
            }),
            reports.execute(ReportCommentCmd {
                comment: root.comment,
                reporter: r2,
            })
        );
        let receipts = [one.unwrap(), two.unwrap()];
        assert_eq!(
            receipts.iter().filter(|receipt| receipt.shadowed).count(),
            1
        );
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == "CommentShadowed")
                .count(),
            1
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn pagination_freezes_hot_candidates_and_enforces_visibility() {
        let (store, clock, market, alice, bob) = fixture();
        let post_uc = PostComment {
            store: &store,
            clock: &clock,
            config: SocialConfig::default(),
        };
        let first = post(market, alice, None, "first");
        post_uc.execute(first.clone()).await.unwrap();
        clock.advance(Duration::seconds(1));
        let second = post(market, bob, None, "second");
        post_uc.execute(second.clone()).await.unwrap();
        clock.advance(Duration::seconds(1));
        let shadow = post(market, alice, None, "https://a https://b https://c");
        post_uc.execute(shadow.clone()).await.unwrap();

        let public = store
            .comments(market, CommentSort::Recent, None, 10, None, clock.now())
            .await
            .unwrap();
        assert_eq!(public.comments.len(), 2);
        let recent_cursor = store
            .comments(market, CommentSort::Recent, None, 1, None, clock.now())
            .await
            .unwrap()
            .next;
        let author = store
            .comments(
                market,
                CommentSort::Recent,
                Some(alice),
                10,
                None,
                clock.now(),
            )
            .await
            .unwrap();
        assert_eq!(author.comments.len(), 3);

        let page_as_of = clock.now();
        let page1 = store
            .comments(market, CommentSort::Hot, None, 1, None, clock.now())
            .await
            .unwrap();
        let cursor = page1.next.unwrap();
        assert!(matches!(
            cursor,
            CommentCursor::Hot { as_of, .. } if as_of == page_as_of
        ));
        clock.advance(Duration::hours(1));
        post_uc
            .execute(post(market, bob, None, "late insert"))
            .await
            .unwrap();
        let page2 = store
            .comments(
                market,
                CommentSort::Hot,
                None,
                10,
                Some(cursor),
                clock.now(),
            )
            .await
            .unwrap();
        assert!(page2
            .comments
            .iter()
            .all(|row| row.row.created_at <= page_as_of));
        assert!(!page2
            .comments
            .iter()
            .any(|row| row.row.body == "late insert"));
        assert!(store
            .comments(
                market,
                CommentSort::Recent,
                None,
                1,
                Some(cursor),
                clock.now(),
            )
            .await
            .is_err());
        assert!(store
            .comments(
                market,
                CommentSort::Hot,
                None,
                1,
                recent_cursor,
                clock.now(),
            )
            .await
            .is_err());
        let recent_page_1 = store
            .comments(market, CommentSort::Recent, None, 1, None, clock.now())
            .await
            .unwrap();
        let recent_page_2 = store
            .comments(
                market,
                CommentSort::Recent,
                None,
                10,
                recent_page_1.next,
                clock.now(),
            )
            .await
            .unwrap();
        assert!(recent_page_2.comments.iter().all(|row| {
            row.row.created_at <= recent_page_1.comments[0].row.created_at
                && row.row.id != recent_page_1.comments[0].row.id
        }));
        store.record_vote(alice, market, Side::Yes);
    }

    #[test]
    fn oversized_duration_is_rejected() {
        assert_eq!(seconds(u64::MAX), Err(AppError::Overflow));
    }
}
