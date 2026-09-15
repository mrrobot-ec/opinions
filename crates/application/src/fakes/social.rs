#[async_trait]
impl CommentWriter for InMemTx {
    async fn comment(&mut self, id: CommentId) -> Result<Option<CommentRow>, StoreError> {
        let state = self.shared.state.lock();
        Ok(self.comment_view(&state, id))
    }

    async fn comment_for_update(&mut self, id: CommentId) -> Result<CommentRow, StoreError> {
        self.lock_comment_row(id.0).await;
        let state = self.shared.state.lock();
        self.comment_view(&state, id)
            .ok_or(StoreError::NotFound("comment"))
    }

    async fn recent_same_hash(
        &mut self,
        author: UserId,
        hash: &str,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let committed = self
            .shared
            .state
            .lock()
            .comments
            .values()
            .filter(|row| {
                row.author == author
                    && row.created_at >= since
                    && row.body_hash.as_deref() == Some(hash)
            })
            .count();
        let pending = self
            .pending
            .comments
            .iter()
            .filter(|row| row.author == author && row.created_at >= since && row.body_hash == hash)
            .count();
        u32::try_from(committed.saturating_add(pending))
            .map_err(|_| StoreError::Invariant("comment count overflow"))
    }

    async fn author_posts_since(
        &mut self,
        author: UserId,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let committed = self
            .shared
            .state
            .lock()
            .comments
            .values()
            .filter(|row| row.author == author && row.created_at >= since)
            .count();
        let pending = self
            .pending
            .comments
            .iter()
            .filter(|row| row.author == author && row.created_at >= since)
            .count();
        u32::try_from(committed.saturating_add(pending))
            .map_err(|_| StoreError::Invariant("comment count overflow"))
    }

    async fn insert_comment(&mut self, comment: NewComment) -> Result<(), StoreError> {
        if self.pending.comments.iter().any(|row| row.id == comment.id)
            || self
                .shared
                .state
                .lock()
                .comments
                .contains_key(&comment.id.0)
        {
            return Err(StoreError::Conflict("comment"));
        }
        self.pending.comments.push(comment);
        Ok(())
    }

    async fn bump_reply_count(&mut self, parent: CommentId) -> Result<(), StoreError> {
        let state = self.shared.state.lock();
        if self.comment_view(&state, parent).is_none() {
            return Err(StoreError::NotFound("comment"));
        }
        drop(state);
        self.pending.comment_reply_bumps.push(parent.0);
        Ok(())
    }

    async fn insert_comment_vote(
        &mut self,
        comment: CommentId,
        user: UserId,
        value: i16,
    ) -> Result<bool, StoreError> {
        let key = (comment.0, user.0);
        if self
            .pending
            .comment_votes
            .iter()
            .any(|(c, u, _)| (*c, *u) == key)
            || self.shared.state.lock().comment_votes.contains(&key)
        {
            return Ok(false);
        }
        self.pending.comment_votes.push((comment.0, user.0, value));
        Ok(true)
    }

    async fn adjust_comment_score(
        &mut self,
        comment: CommentId,
        delta: i16,
    ) -> Result<i32, StoreError> {
        let state = self.shared.state.lock();
        let current = self
            .comment_view(&state, comment)
            .ok_or(StoreError::NotFound("comment"))?
            .score;
        drop(state);
        let score = current
            .checked_add(i32::from(delta))
            .ok_or(StoreError::Invariant("comment score overflow"))?;
        self.pending.comment_score_updates.push((comment.0, score));
        Ok(score)
    }

    async fn insert_comment_report(
        &mut self,
        comment: CommentId,
        reporter: UserId,
        created_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let key = (comment.0, reporter.0);
        if self
            .pending
            .comment_reports
            .iter()
            .any(|(c, u, _)| (*c, *u) == key)
            || (!self.pending.comment_report_resets.contains(&comment.0)
                && self.shared.state.lock().comment_reports.contains_key(&key))
        {
            return Ok(false);
        }
        self.pending
            .comment_reports
            .push((comment.0, reporter.0, created_at));
        Ok(true)
    }

    async fn comment_report_count(&mut self, comment: CommentId) -> Result<u32, StoreError> {
        let committed = if self.pending.comment_report_resets.contains(&comment.0) {
            0
        } else {
            self.shared
                .state
                .lock()
                .comment_reports
                .keys()
                .filter(|(id, _)| *id == comment.0)
                .count()
        };
        let pending = self
            .pending
            .comment_reports
            .iter()
            .filter(|(id, _, _)| *id == comment.0)
            .count();
        u32::try_from(committed.saturating_add(pending))
            .map_err(|_| StoreError::Invariant("comment report count overflow"))
    }

    async fn reporter_reports_since(
        &mut self,
        reporter: UserId,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let reset_comments = &self.pending.comment_report_resets;
        let committed = self
            .shared
            .state
            .lock()
            .comment_reports
            .iter()
            .filter(|((comment, user), created)| {
                *user == reporter.0 && **created >= since && !reset_comments.contains(comment)
            })
            .count();
        let pending = self
            .pending
            .comment_reports
            .iter()
            .filter(|(_, user, created)| *user == reporter.0 && *created >= since)
            .count();
        u32::try_from(committed.saturating_add(pending))
            .map_err(|_| StoreError::Invariant("comment report count overflow"))
    }

    async fn set_comment_status(
        &mut self,
        comment: CommentId,
        status: ModerationStatus,
    ) -> Result<(), StoreError> {
        let state = self.shared.state.lock();
        if self.comment_view(&state, comment).is_none() {
            return Err(StoreError::NotFound("comment"));
        }
        drop(state);
        self.pending
            .comment_status_updates
            .push((comment.0, status));
        Ok(())
    }

    async fn delete_comment_reports(&mut self, comment: CommentId) -> Result<u32, StoreError> {
        let count = self.comment_report_count(comment).await?;
        self.pending.comment_report_resets.insert(comment.0);
        self.pending
            .comment_reports
            .retain(|(id, _, _)| *id != comment.0);
        Ok(count)
    }
}

#[async_trait]
impl NotifyReader for InMemTx {
    async fn lock_notifier_cursor(&mut self) -> Result<i64, StoreError> {
        if self.notifier_guard.is_none() {
            self.notifier_guard = Some(Arc::clone(&self.shared.notifier_lock).lock_owned().await);
        }
        Ok(self.shared.state.lock().notifier_cursor)
    }

    async fn outbox_events_after(
        &mut self,
        last_seq: i64,
        limit: u32,
    ) -> Result<Vec<OutboxEvent>, StoreError> {
        let state = self.shared.state.lock();
        Ok(state
            .outbox
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                let seq = i64::try_from(index).ok()?.checked_add(1)?;
                (seq > last_seq).then_some(OutboxEvent {
                    seq,
                    event_type: event.event_type.to_string(),
                    aggregate_type: event.aggregate_type.to_string(),
                    aggregate_id: event.aggregate_id,
                    payload: event.payload.clone(),
                })
            })
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .collect())
    }

    async fn resolution_recipients(
        &mut self,
        market: MarketId,
        voided: bool,
    ) -> Result<Vec<ResolutionRecipient>, StoreError> {
        let state = self.shared.state.lock();
        let terminal = if voided {
            RealizationSource::Void
        } else {
            RealizationSource::Settlement
        };
        let mut recipients = BTreeMap::<Uuid, ResolutionRecipient>::new();
        for fact in state
            .realizations
            .iter()
            .filter(|fact| fact.market == market && fact.source == terminal)
        {
            let row = recipients
                .entry(fact.user.0)
                .or_insert(ResolutionRecipient {
                    user: fact.user,
                    held: true,
                    payout_total: MicroUsd(0),
                    realized_delta: MicroUsd(0),
                    voted: false,
                    score_bp: None,
                    side: None,
                });
            row.payout_total = MicroUsd(
                row.payout_total
                    .0
                    .checked_add(fact.payout.0)
                    .ok_or(StoreError::Invariant("notification payout overflow"))?,
            );
            row.realized_delta = MicroUsd(
                row.realized_delta
                    .0
                    .checked_add(fact.realized_delta.0)
                    .ok_or(StoreError::Invariant("notification PnL overflow"))?,
            );
        }
        for vote in state
            .vote_rows
            .values()
            .filter(|vote| vote.market == market)
        {
            let row = recipients
                .entry(vote.user.0)
                .or_insert(ResolutionRecipient {
                    user: vote.user,
                    held: false,
                    payout_total: MicroUsd(0),
                    realized_delta: MicroUsd(0),
                    voted: true,
                    score_bp: None,
                    side: Some(vote.side),
                });
            row.voted = true;
            row.score_bp = vote.score.map(|score| score.score_bp);
            row.side = Some(vote.side);
        }
        Ok(recipients.into_values().collect())
    }

    async fn parent_author(&mut self, comment: CommentId) -> Result<Option<UserId>, StoreError> {
        let state = self.shared.state.lock();
        Ok(state
            .comments
            .get(&comment.0)
            .and_then(|comment| comment.parent)
            .and_then(|parent| state.comments.get(&parent.0))
            .map(|parent| parent.author))
    }

    async fn users_by_handles(&mut self, handles: &[String]) -> Result<Vec<UserId>, StoreError> {
        let state = self.shared.state.lock();
        let mut users = state
            .users
            .iter()
            .filter(|(_, candidate)| {
                handles
                    .iter()
                    .any(|handle| candidate.eq_ignore_ascii_case(handle))
            })
            .map(|(id, _)| UserId(*id))
            .collect::<Vec<_>>();
        users.sort_unstable_by_key(|user| user.0);
        Ok(users)
    }

    async fn notification_count_since(
        &mut self,
        user: UserId,
        notification_type: &str,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let committed = self
            .shared
            .state
            .lock()
            .notifications
            .iter()
            .filter(|row| {
                row.user == user
                    && row.notification_type == notification_type
                    && row.created_at >= since
            })
            .count();
        let pending = self
            .pending
            .notifications
            .iter()
            .filter(|row| {
                row.user == user
                    && row.notification_type == notification_type
                    && row.created_at >= since
            })
            .count();
        u32::try_from(committed.saturating_add(pending))
            .map_err(|_| StoreError::Invariant("notification count overflow"))
    }
}

#[async_trait]
impl NotificationWriter for InMemTx {
    async fn insert_notifications(
        &mut self,
        notifications: &[NewNotification],
    ) -> Result<Vec<NotificationRow>, StoreError> {
        let state = self.shared.state.lock();
        let mut next_id = state
            .notifications
            .iter()
            .map(|row| row.id)
            .chain(self.pending.notifications.iter().map(|row| row.id))
            .max()
            .unwrap_or(0);
        let mut inserted = Vec::new();
        for notification in notifications {
            let duplicate = state
                .notifications
                .iter()
                .chain(&self.pending.notifications)
                .any(|row| {
                    row.user == notification.user && row.source_seq == Some(notification.source_seq)
                });
            if duplicate {
                continue;
            }
            next_id = next_id
                .checked_add(1)
                .ok_or(StoreError::Invariant("notification id overflow"))?;
            let row = NotificationRow {
                id: next_id,
                user: notification.user,
                notification_type: notification.notification_type.clone(),
                market: notification.market,
                payload: notification.payload.clone(),
                source_seq: Some(notification.source_seq),
                read_at: None,
                created_at: notification.created_at,
            };
            self.pending.notifications.push(row.clone());
            inserted.push(row);
        }
        Ok(inserted)
    }

    async fn advance_notifier_cursor(&mut self, seq: i64) -> Result<(), StoreError> {
        self.pending.notifier_cursor = Some(seq);
        Ok(())
    }
}

#[async_trait]
impl SocialQueries for InMemoryStore {
    #[allow(clippy::too_many_lines)]
    async fn comments(
        &self,
        market: MarketId,
        sort: CommentSort,
        viewer: Option<UserId>,
        limit: u32,
        cursor: Option<CommentCursor>,
        now: OffsetDateTime,
    ) -> Result<CommentPage, StoreError> {
        let state = self.shared.state.lock();
        let mut rows: Vec<CommentView> = state
            .comments
            .values()
            .filter(|row| {
                row.market == market
                    && (row.moderation_status == ModerationStatus::Visible
                        || (row.moderation_status == ModerationStatus::Shadow
                            && viewer == Some(row.author)))
            })
            .filter_map(|row| {
                let author_handle = state.users.get(&row.author.0)?.clone();
                Some(CommentView {
                    row: row.clone(),
                    author_handle,
                    hot_score: 0,
                })
            })
            .collect();
        let limit = usize::try_from(limit).map_err(|_| StoreError::Invariant("limit overflow"))?;

        let next = match sort {
            CommentSort::Hot => {
                let (as_of, seek) = match cursor {
                    None => (now, None),
                    Some(CommentCursor::Hot {
                        as_of,
                        hot_score,
                        created_at,
                        id,
                    }) => (as_of, Some((hot_score, created_at, id))),
                    Some(CommentCursor::Recent { .. }) => {
                        return Err(StoreError::Invariant("cursor sort mismatch"));
                    }
                };
                rows.retain(|row| row.row.created_at <= as_of);
                rows.sort_by(|a, b| {
                    b.row
                        .created_at
                        .cmp(&a.row.created_at)
                        .then_with(|| b.row.id.cmp(&a.row.id))
                });
                rows.truncate(500);
                for row in &mut rows {
                    let age = (as_of - row.row.created_at).whole_seconds();
                    row.hot_score = domain::ranking::hot_score(row.row.score, age)
                        .map_err(|_| StoreError::Invariant("future comment in frozen set"))?;
                }
                rows.sort_by(|a, b| {
                    b.hot_score
                        .cmp(&a.hot_score)
                        .then_with(|| b.row.created_at.cmp(&a.row.created_at))
                        .then_with(|| b.row.id.cmp(&a.row.id))
                });
                if let Some((score, created_at, id)) = seek {
                    rows.retain(|row| {
                        (row.hot_score, row.row.created_at, row.row.id) < (score, created_at, id)
                    });
                }
                let has_more = rows.len() > limit;
                rows.truncate(limit);
                has_more
                    .then(|| rows.last())
                    .flatten()
                    .map(|row| CommentCursor::Hot {
                        as_of,
                        hot_score: row.hot_score,
                        created_at: row.row.created_at,
                        id: row.row.id,
                    })
            }
            CommentSort::Recent => {
                let seek = match cursor {
                    None => None,
                    Some(CommentCursor::Recent { created_at, id }) => Some((created_at, id)),
                    Some(CommentCursor::Hot { .. }) => {
                        return Err(StoreError::Invariant("cursor sort mismatch"));
                    }
                };
                rows.sort_by(|a, b| {
                    b.row
                        .created_at
                        .cmp(&a.row.created_at)
                        .then_with(|| b.row.id.cmp(&a.row.id))
                });
                if let Some((created_at, id)) = seek {
                    rows.retain(|row| (row.row.created_at, row.row.id) < (created_at, id));
                }
                let has_more = rows.len() > limit;
                rows.truncate(limit);
                has_more
                    .then(|| rows.last())
                    .flatten()
                    .map(|row| CommentCursor::Recent {
                        created_at: row.row.created_at,
                        id: row.row.id,
                    })
            }
        };
        Ok(CommentPage {
            comments: rows,
            next,
        })
    }

    async fn comment_view(
        &self,
        comment: CommentId,
        viewer: Option<UserId>,
    ) -> Result<CommentView, StoreError> {
        let state = self.shared.state.lock();
        let row = state
            .comments
            .get(&comment.0)
            .filter(|row| {
                row.moderation_status == ModerationStatus::Visible
                    || row.author == viewer.unwrap_or(UserId(Uuid::nil()))
            })
            .ok_or(StoreError::NotFound("comment"))?;
        Ok(CommentView {
            row: row.clone(),
            author_handle: state
                .users
                .get(&row.author.0)
                .cloned()
                .ok_or(StoreError::NotFound("user"))?,
            hot_score: 0,
        })
    }

    async fn holders(&self, market: MarketId, limit: u32) -> Result<MarketHolders, StoreError> {
        let state = self.shared.state.lock();
        let market = state
            .markets
            .get(&market.0)
            .ok_or(StoreError::NotFound("market"))?;
        let side = |outcome: OutcomeId| -> Result<Vec<crate::model::HolderRow>, StoreError> {
            let mut rows = state
                .positions
                .values()
                .filter(|position| position.outcome == outcome && position.cost.0 != 0)
                .map(|position| {
                    let (_, tier) = state
                        .reputation
                        .get(&position.user.0)
                        .copied()
                        .ok_or(StoreError::NotFound("reputation"))?;
                    Ok(crate::model::HolderRow {
                        user: position.user,
                        handle: state
                            .users
                            .get(&position.user.0)
                            .cloned()
                            .ok_or(StoreError::NotFound("user"))?,
                        tier,
                        cost: position.cost,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            rows.sort_unstable_by(|left, right| {
                right
                    .cost
                    .0
                    .cmp(&left.cost.0)
                    .then_with(|| left.user.0.cmp(&right.user.0))
            });
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            Ok(rows)
        };
        Ok(MarketHolders {
            yes: side(market.yes_outcome)?,
            no: side(market.no_outcome)?,
        })
    }

    async fn user_profile(&self, user: UserId) -> Result<UserProfile, StoreError> {
        let state = self.shared.state.lock();
        let handle = state
            .users
            .get(&user.0)
            .cloned()
            .ok_or(StoreError::NotFound("user"))?;
        let created_at = state
            .user_created_at
            .get(&user.0)
            .copied()
            .ok_or(StoreError::NotFound("user"))?;
        let (rep_micro, tier) = state
            .reputation
            .get(&user.0)
            .copied()
            .ok_or(StoreError::NotFound("reputation"))?;
        let scores = state
            .vote_rows
            .values()
            .filter(|vote| vote.user == user)
            .filter_map(|vote| vote.score.map(|score| i64::from(score.score_bp)))
            .collect::<Vec<_>>();
        let markets_scored = u32::try_from(scores.len())
            .map_err(|_| StoreError::Invariant("profile score count overflow"))?;
        let avg_score_bp = (!scores.is_empty())
            .then(|| scores.iter().sum::<i64>() / i64::try_from(scores.len()).unwrap_or(i64::MAX));
        let realized = state
            .realizations
            .iter()
            .filter(|fact| fact.user == user)
            .try_fold(0_i64, |sum, fact| sum.checked_add(fact.realized_delta.0))
            .ok_or(StoreError::Invariant("profile PnL overflow"))?;
        let mut recent_trades = state
            .trades
            .values()
            .filter(|trade| trade.new.user == user)
            .filter_map(|trade| {
                state
                    .markets
                    .get(&trade.new.market.0)
                    .map(|market| ProfileTradeRow {
                        market: market.id,
                        market_ref: market.slug.clone(),
                        side: trade.new.side,
                        action: trade.new.action,
                        collateral_micro: trade.new.gross.0,
                        created_at: trade.created_at,
                        trade_seq: trade.trade_seq,
                    })
            })
            .collect::<Vec<_>>();
        recent_trades.sort_unstable_by_key(|trade| std::cmp::Reverse(trade.created_at));
        recent_trades.truncate(20);
        let mut recent_votes = state
            .vote_rows
            .values()
            .filter(|vote| vote.user == user)
            .filter_map(|vote| {
                let market = state.markets.get(&vote.market.0)?;
                let terminal = matches!(
                    market.state,
                    MarketState::Resolved | MarketState::Paid | MarketState::Voided
                );
                Some(ProfileVoteRow {
                    market: market.id,
                    market_question: market.question.clone(),
                    cast_at: vote.created_at,
                    side: terminal.then_some(vote.side),
                    score_bp: terminal
                        .then_some(vote.score)
                        .flatten()
                        .map(|score| score.score_bp),
                })
            })
            .collect::<Vec<_>>();
        recent_votes.sort_unstable_by_key(|vote| std::cmp::Reverse(vote.cast_at));
        recent_votes.truncate(20);
        Ok(UserProfile {
            user,
            handle,
            created_at,
            rep_micro,
            tier,
            avg_score_bp,
            markets_scored,
            realized_pnl: MicroUsd(realized),
            recent_trades,
            recent_votes,
        })
    }

    async fn reported_comments(
        &self,
        threshold: u32,
        limit: u32,
    ) -> Result<Vec<ReportedCommentRow>, StoreError> {
        let state = self.shared.state.lock();
        let mut rows = Vec::new();
        for comment in state.comments.values() {
            let reports = state
                .comment_reports
                .keys()
                .filter(|(id, _)| *id == comment.id.0)
                .collect::<Vec<_>>();
            let report_count = u32::try_from(reports.len())
                .map_err(|_| StoreError::Invariant("comment report count overflow"))?;
            if report_count < threshold && comment.moderation_status != ModerationStatus::Shadow {
                continue;
            }
            let mut reporters = reports
                .iter()
                .filter_map(|(_, user)| state.users.get(user).cloned())
                .collect::<Vec<_>>();
            reporters.sort();
            rows.push(ReportedCommentRow {
                comment: CommentView {
                    row: comment.clone(),
                    author_handle: state
                        .users
                        .get(&comment.author.0)
                        .cloned()
                        .ok_or(StoreError::NotFound("user"))?,
                    hot_score: 0,
                },
                report_count,
                reporters,
            });
        }
        rows.sort_unstable_by_key(|row| std::cmp::Reverse(row.comment.row.created_at));
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(rows)
    }
}

#[async_trait]
impl NotificationQueries for InMemoryStore {
    async fn notifications(
        &self,
        user: UserId,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<NotificationRow>, StoreError> {
        let state = self.shared.state.lock();
        let mut rows = state
            .notifications
            .iter()
            .filter(|row| row.user == user && before_id.is_none_or(|before| row.id < before))
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_unstable_by_key(|row| std::cmp::Reverse(row.id));
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(rows)
    }

    async fn unread_count(&self, user: UserId) -> Result<u32, StoreError> {
        let count = self
            .shared
            .state
            .lock()
            .notifications
            .iter()
            .filter(|row| row.user == user && row.read_at.is_none())
            .count();
        u32::try_from(count).map_err(|_| StoreError::Invariant("notification count overflow"))
    }

    async fn mark_notifications_read(
        &self,
        user: UserId,
        ids: &[i64],
        now: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let mut state = self.shared.state.lock();
        let mut affected = 0_u32;
        for row in &mut state.notifications {
            if row.user == user && row.read_at.is_none() && ids.contains(&row.id) {
                row.read_at = Some(now);
                affected = affected
                    .checked_add(1)
                    .ok_or(StoreError::Invariant("notification count overflow"))?;
            }
        }
        Ok(affected)
    }
}

