/// Comment role contract shared by the fake and `PostgreSQL`: read-your-write,
/// insert-first uniqueness, counters, report epoch reset, and both list sorts.
#[allow(clippy::too_many_lines)]
pub async fn comment_writer_contract<S>(
    store: &S,
    market: MarketId,
    author: UserId,
    voter: UserId,
    now: time::OffsetDateTime,
) where
    S: Store + SocialQueries + ?Sized,
{
    let comment = CommentId(Uuid::new_v4());
    let mut tx = store.comment_tx().await.unwrap();
    tx.serialize_key(&unique_key("comment-contract"))
        .await
        .unwrap();
    tx.lock_user(author).await.unwrap();
    tx.market_for_update(market).await.unwrap();
    assert!(!tx.handle(author).await.unwrap().is_empty());
    assert!(tx
        .user_by_handle("definitely-absent")
        .await
        .unwrap()
        .is_none());
    assert!(tx.user_tier(author).await.unwrap() <= 4);
    tx.insert_comment(NewComment {
        id: comment,
        market,
        author,
        parent: None,
        body: "contract".to_string(),
        body_hash: domain::moderation::body_hash("contract"),
        moderation_status: ModerationStatus::Visible,
        depth: 0,
        created_at: now,
    })
    .await
    .unwrap();
    assert_eq!(tx.author_posts_since(author, now).await.unwrap(), 1);
    assert_eq!(
        tx.recent_same_hash(author, &domain::moderation::body_hash("contract"), now)
            .await
            .unwrap(),
        1
    );
    assert_eq!(tx.comment(comment).await.unwrap().unwrap().score, 0);
    let reply = CommentId(Uuid::from_u128(comment.0.as_u128().saturating_add(1)));
    tx.insert_comment(NewComment {
        id: reply,
        market,
        author: voter,
        parent: Some(comment),
        body: "contract reply".to_string(),
        body_hash: domain::moderation::body_hash("contract reply"),
        moderation_status: ModerationStatus::Visible,
        depth: 1,
        created_at: now,
    })
    .await
    .unwrap();
    tx.bump_reply_count(comment).await.unwrap();
    assert_eq!(tx.comment(comment).await.unwrap().unwrap().reply_count, 1);
    tx.commit().await.unwrap();

    let recent_before_shadow = store
        .comments(
            market,
            CommentSort::Recent,
            None,
            1,
            None,
            now + time::Duration::seconds(1),
        )
        .await
        .unwrap();
    assert_eq!(recent_before_shadow.comments[0].row.id, reply);
    assert!(recent_before_shadow.next.is_some());
    let tied_hot = store
        .comments(market, CommentSort::Hot, None, 2, None, now)
        .await
        .unwrap();
    assert_eq!(
        tied_hot
            .comments
            .iter()
            .map(|row| row.row.id)
            .collect::<Vec<_>>(),
        vec![reply, comment]
    );
    assert_eq!(
        store
            .comments(
                market,
                CommentSort::Recent,
                None,
                10,
                recent_before_shadow.next,
                now + time::Duration::seconds(1),
            )
            .await
            .unwrap()
            .comments[0]
            .row
            .id,
        comment
    );

    let mut tx = store.comment_tx().await.unwrap();
    tx.serialize_key(&unique_key("comment-contract-mutate"))
        .await
        .unwrap();
    assert_eq!(
        tx.comment_for_update(comment)
            .await
            .unwrap()
            .moderation_status,
        ModerationStatus::Visible
    );
    assert_eq!(
        tx.comment_for_update(comment).await.unwrap().id,
        comment,
        "locking an already-held comment row is idempotent"
    );
    assert!(tx.insert_comment_vote(comment, voter, 1).await.unwrap());
    assert!(!tx.insert_comment_vote(comment, voter, -1).await.unwrap());
    assert_eq!(tx.adjust_comment_score(comment, 1).await.unwrap(), 1);
    assert!(tx.insert_comment_report(comment, voter, now).await.unwrap());
    assert!(!tx.insert_comment_report(comment, voter, now).await.unwrap());
    assert_eq!(tx.comment_report_count(comment).await.unwrap(), 1);
    assert_eq!(tx.reporter_reports_since(voter, now).await.unwrap(), 1);
    tx.set_comment_status(comment, ModerationStatus::Shadow)
        .await
        .unwrap();
    assert_eq!(
        tx.comment(comment)
            .await
            .unwrap()
            .unwrap()
            .moderation_status,
        ModerationStatus::Shadow
    );
    assert_eq!(tx.delete_comment_reports(comment).await.unwrap(), 1);
    assert_eq!(tx.comment_report_count(comment).await.unwrap(), 0);
    tx.commit().await.unwrap();

    let recent = store
        .comments(market, CommentSort::Recent, None, 10, None, now)
        .await
        .unwrap();
    assert_eq!(recent.comments.len(), 1);
    let hot = store
        .comments(
            market,
            CommentSort::Hot,
            Some(author),
            1,
            None,
            now + time::Duration::seconds(1),
        )
        .await
        .unwrap();
    assert_eq!(hot.comments.len(), 1);
    assert_eq!(
        (hot.comments[0].row.id, hot.comments[0].row.score),
        (comment, 1)
    );
    assert!(hot.next.is_some());
    let second_hot = store
        .comments(
            market,
            CommentSort::Hot,
            Some(author),
            10,
            hot.next,
            now + time::Duration::seconds(1),
        )
        .await
        .unwrap()
        .comments;
    assert_eq!(second_hot.len(), 1);
    assert_eq!(second_hot[0].row.id, reply);
}

