//! `CastVote` — the oracle write path (D4: voting is the price of trading).
//! Same guard-first sequence as `PlaceTrade` rule 1: `serialize_key` →
//! replay-check (`vote_by_key`) → `market_for_update` → validate under the
//! row lock. Voting continues through the D22 frozen window (trading does
//! not): the gate is `Live | Closing` and `clock.now() < closes_at` — a
//! lagging `Closing` row must not accept a post-cutoff oracle write (P1R2).

use domain::amm::Side;
use domain::market::MarketState;
use serde_json::json;

use crate::error::{AppError, StoreError};
use crate::model::{Event, MarketId, NewVote, UserId, VoteIntegrityConfig, VoteReceipt};
use crate::ports::{Clock, Store};

#[derive(Debug, Clone)]
pub struct CastVoteCmd {
    pub market: MarketId,
    pub user: UserId,
    pub side: Side,
    /// The voter's estimate of the crowd's YES share, 0..=100.
    pub crowd_guess_pct: u8,
    pub idempotency_key: String,
    pub cast_ip: Option<std::net::IpAddr>,
    pub device_hash: Option<String>,
}

pub struct CastVote<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub config: VoteIntegrityConfig,
}

impl<S: Store, C: Clock> CastVote<'_, S, C> {
    /// # Errors
    /// [`AppError::InvalidCrowdGuess`], [`AppError::MarketNotOpen`],
    /// [`AppError::VotingClosed`], [`AppError::AlreadyVoted`], and store
    /// failures. On any error nothing became observable.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(&self, cmd: CastVoteCmd) -> Result<VoteReceipt, AppError> {
        if cmd.crowd_guess_pct > 100 {
            return Err(AppError::InvalidCrowdGuess);
        }
        // Guard-first (rule 1 shape): key lock → replay check → row lock.
        let mut tx = self.store.vote_tx().await?;
        tx.serialize_key(&cmd.idempotency_key).await?;
        let fingerprint = crate::ops::config::vote_fingerprint(
            cmd.market,
            cmd.user,
            cmd.side,
            cmd.crowd_guess_pct,
        );
        if let Some(mut receipt) = tx
            .vote_by_key(&cmd.idempotency_key, self.clock.now())
            .await?
        {
            // Replay precedence (D25): fingerprint compare FIRST; a match
            // replays with NO pause check, a mismatch is a typed 409.
            let stored = tx.request_fingerprint(&cmd.idempotency_key).await?;
            crate::ops::config::validate_replay_fingerprint(stored.as_deref(), &fingerprint)?;
            receipt.replayed = true;
            return Ok(receipt);
        }
        tx.lock_user(cmd.user).await?;
        tx.convert_then_collect(cmd.user, self.clock.now(), &cmd.idempotency_key)
            .await?;
        crate::money::enforcement::enforce_money_mutation(
            tx.as_mut(),
            cmd.user,
            crate::money::enforcement::MoneyMutation::CastVote,
            0,
            self.clock.now(),
        )
        .await?;
        let market = tx.market_for_update(cmd.market).await?;
        if !matches!(market.state, MarketState::Live | MarketState::Closing) {
            return Err(AppError::MarketNotOpen);
        }
        // Clock-under-lock (P1R2): a lagging Closing row must not accept a
        // post-cutoff oracle write. Voting continues through the D22 frozen
        // window — tally_hidden_at is deliberately not consulted here.
        let now = self.clock.now();
        if now >= market.closes_at {
            return Err(AppError::VotingClosed);
        }
        if !tx.user_has_channel(cmd.user, "imessage").await? {
            return Err(AppError::PhoneVerificationRequired);
        }
        let window = i64::try_from(self.config.window_secs).map_err(|_| AppError::Overflow)?;
        let count = tx
            .votes_count_since(cmd.user, now - time::Duration::seconds(window))
            .await?;
        if count >= self.config.max_votes_per_window {
            return Err(AppError::VoteVelocityExceeded);
        }
        let near_close =
            i64::try_from(self.config.near_close_secs).map_err(|_| AppError::Overflow)?;
        let min_age =
            i64::try_from(self.config.min_account_age_secs).map_err(|_| AppError::Overflow)?;
        if now >= market.closes_at - time::Duration::seconds(near_close)
            && now - tx.user_created_at(cmd.user).await? < time::Duration::seconds(min_age)
        {
            return Err(AppError::AccountTooYoungNearClose);
        }
        // D25 fence point: ONLY the voting fence — a trading pause can never
        // delay votes. Acquired after the row locks, before the first write.
        tx.acquire_shared_fences(&[crate::ops::config::voting_market_fence(cmd.market)])
            .await?;
        // Auto-expiry read (grok r2 f2): a Live-set pause is void once the
        // hidden window starts — the operator cannot freeze the final public
        // tally and ride D22; no admin action required.
        if now < market.tally_hidden_at
            && crate::ops::config::pause_in_force(
                tx.fence_config_value(&format!("voting_paused:{}", cmd.market.0))
                    .await?
                    .as_ref(),
            )
        {
            return Err(AppError::VotingPaused);
        }
        tx.save_request_fingerprint(&cmd.idempotency_key, &fingerprint)
            .await?;
        let seq = tx.allocate_vote_seq(cmd.market).await?;
        let vote_id = tx
            .insert_vote(NewVote {
                market: cmd.market,
                user: cmd.user,
                side: cmd.side,
                crowd_guess_pct: cmd.crowd_guess_pct,
                seq,
                idempotency_key: cmd.idempotency_key.clone(),
                created_at: now,
                cast_ip: cmd.cast_ip,
                device_hash: cmd.device_hash,
            })
            .await
            .map_err(map_vote_write_error)?;
        tx.append(Event {
            event_type: "VoteCast",
            aggregate_type: "market",
            aggregate_id: cmd.market.0,
            payload: json!({
                "market_id": cmd.market.0.to_string(),
            }),
        })
        .await?;
        // A racing duplicate can also surface at commit (unique-index shape);
        // map it to the same business error.
        tx.commit().await.map_err(map_vote_write_error)?;
        Ok(VoteReceipt {
            vote_id,
            market: cmd.market,
            user: cmd.user,
            side: cmd.side,
            crowd_guess_pct: cmd.crowd_guess_pct,
            seq: (now < market.tally_hidden_at
                || matches!(
                    market.state,
                    MarketState::Resolved | MarketState::Paid | MarketState::Voided
                ))
            .then_some(seq),
            replayed: false,
        })
    }
}

fn map_vote_write_error(error: StoreError) -> AppError {
    match error {
        StoreError::Conflict("vote") => AppError::AlreadyVoted,
        other => AppError::Store(other),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::ports::MarketQueries;
    use domain::money::{BasisPoints, MicroShares};
    use time::{Duration, OffsetDateTime};

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn vote_write_errors_preserve_business_and_backend_meaning() {
        assert_eq!(
            map_vote_write_error(StoreError::Conflict("vote")),
            AppError::AlreadyVoted
        );
        assert_eq!(
            map_vote_write_error(StoreError::Backend("offline".to_string())),
            AppError::Store(StoreError::Backend("offline".to_string()))
        );
    }

    fn live_market(store: &InMemoryStore) -> MarketId {
        store
            .add_market(
                "votable",
                MarketState::Live,
                t0() + Duration::hours(2), // closes_at
                t0() + Duration::hours(1), // tally_hidden_at
                MicroShares(100_000_000),
                BasisPoints(100),
            )
            .unwrap()
            .id
    }

    fn vote_cmd(market: MarketId, user: UserId, key: &str) -> CastVoteCmd {
        CastVoteCmd {
            market,
            user,
            side: Side::Yes,
            crowd_guess_pct: 60,
            idempotency_key: key.to_string(),
            cast_ip: None,
            device_hash: None,
        }
    }

    fn linked_user(store: &InMemoryStore) -> UserId {
        let user = UserId(uuid::Uuid::new_v4());
        store.link_user_channel("imessage", &user.0.to_string(), user);
        user
    }

    #[tokio::test]
    async fn vote_records_row_tally_and_event() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let receipt = uc.execute(vote_cmd(market, user, "vote-1")).await.unwrap();
        assert_eq!(receipt.seq, Some(1));
        assert!(!receipt.replayed);
        assert_eq!(receipt.side, Side::Yes);
        assert!(store.user_voted(user, market).await.unwrap());
        let events = store.outbox();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "VoteCast");
        assert_eq!(events[0].aggregate_id, market.0);
    }

    #[tokio::test]
    async fn replay_returns_original_receipt_and_writes_nothing() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let first = uc.execute(vote_cmd(market, user, "vote-1")).await.unwrap();
        let snapshot = store.snapshot();
        let second = uc.execute(vote_cmd(market, user, "vote-1")).await.unwrap();
        assert!(second.replayed);
        assert_eq!(
            VoteReceipt {
                replayed: false,
                ..second
            },
            first
        );
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn seq_is_strictly_increasing_across_voters() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let a = uc
            .execute(vote_cmd(market, linked_user(&store), "vote-a"))
            .await
            .unwrap();
        let b = uc
            .execute(vote_cmd(market, linked_user(&store), "vote-b"))
            .await
            .unwrap();
        assert_eq!((a.seq, b.seq), (Some(1), Some(2)));
    }

    #[tokio::test]
    async fn one_vote_per_user_market() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        uc.execute(vote_cmd(market, user, "vote-1")).await.unwrap();
        let snapshot = store.snapshot();
        let err = uc
            .execute(vote_cmd(market, user, "vote-2")) // different key, same pair
            .await
            .unwrap_err();
        assert_eq!(err, AppError::AlreadyVoted);
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn concurrent_same_pair_commits_exactly_once() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let (a, b) = tokio::join!(
            uc.execute(vote_cmd(market, user, "race-a")),
            uc.execute(vote_cmd(market, user, "race-b"))
        );
        assert!(
            a.is_ok() ^ b.is_ok(),
            "exactly one racing vote may commit: {a:?} / {b:?}"
        );
        let err = if a.is_err() { a } else { b }.unwrap_err();
        assert_eq!(err, AppError::AlreadyVoted);
    }

    #[tokio::test]
    async fn voting_continues_through_the_frozen_window() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let clock = FakeClock::at(t0() + Duration::minutes(90)); // past tally_hidden_at
        store.set_market_state(market, MarketState::Closing);
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let receipt = uc
            .execute(vote_cmd(market, linked_user(&store), "late-vote"))
            .await
            .unwrap();
        assert_eq!(receipt.seq, None);
    }

    #[tokio::test]
    async fn lagging_closing_row_rejects_post_cutoff_votes() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        store.set_market_state(market, MarketState::Closing);
        let clock = FakeClock::at(t0() + Duration::hours(2)); // now == closes_at
        let snapshot = store.snapshot();
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let err = uc
            .execute(vote_cmd(market, UserId(uuid::Uuid::new_v4()), "too-late"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::VotingClosed);
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn closed_market_rejects_votes() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        store.set_market_state(market, MarketState::Closed);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let err = uc
            .execute(vote_cmd(market, UserId(uuid::Uuid::new_v4()), "closed"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::MarketNotOpen);
    }

    #[tokio::test]
    async fn crowd_guess_above_100_is_rejected() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let mut cmd = vote_cmd(market, UserId(uuid::Uuid::new_v4()), "bad-guess");
        cmd.crowd_guess_pct = 101;
        let err = uc.execute(cmd).await.unwrap_err();
        assert_eq!(err, AppError::InvalidCrowdGuess);
    }

    #[tokio::test]
    async fn phone_link_is_required() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = UserId(uuid::Uuid::new_v4());
        let clock = FakeClock::at(t0());
        let error = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        }
        .execute(vote_cmd(market, user, "unlinked"))
        .await
        .unwrap_err();
        assert_eq!(error, AppError::PhoneVerificationRequired);
    }

    #[tokio::test]
    async fn a_non_phone_channel_does_not_satisfy_identity_rule() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = UserId(uuid::Uuid::new_v4());
        store.link_user_channel("email", "person@example.test", user);
        let error = CastVote {
            store: &store,
            clock: &FakeClock::at(t0()),
            config: VoteIntegrityConfig::default(),
        }
        .execute(vote_cmd(market, user, "wrong-channel"))
        .await
        .unwrap_err();
        assert_eq!(error, AppError::PhoneVerificationRequired);
    }

    #[tokio::test]
    async fn global_velocity_cap_is_atomic_across_markets() {
        let store = InMemoryStore::new();
        let first_market = live_market(&store);
        let second_market = store
            .add_market(
                "votable-two",
                MarketState::Live,
                t0() + Duration::hours(2),
                t0() + Duration::hours(1),
                MicroShares(100_000_000),
                BasisPoints(100),
            )
            .unwrap()
            .id;
        let user = linked_user(&store);
        let clock = FakeClock::at(t0());
        let use_case = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig {
                max_votes_per_window: 1,
                ..VoteIntegrityConfig::default()
            },
        };
        let (a, b) = tokio::join!(
            use_case.execute(vote_cmd(first_market, user, "velocity-a")),
            use_case.execute(vote_cmd(second_market, user, "velocity-b"))
        );
        assert!(a.is_ok() ^ b.is_ok());
        let error = if a.is_err() { a } else { b }.unwrap_err();
        assert_eq!(error, AppError::VoteVelocityExceeded);
    }

    #[tokio::test]
    async fn velocity_allows_max_minus_one_then_rejects_at_max() {
        let store = InMemoryStore::new();
        let markets: Vec<_> = ["velocity-one", "velocity-two", "velocity-three"]
            .into_iter()
            .map(|slug| {
                store
                    .add_market(
                        slug,
                        MarketState::Live,
                        t0() + Duration::hours(2),
                        t0() + Duration::hours(1),
                        MicroShares(100_000_000),
                        BasisPoints(100),
                    )
                    .unwrap()
                    .id
            })
            .collect();
        let user = linked_user(&store);
        let clock = FakeClock::at(t0());
        let use_case = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig {
                max_votes_per_window: 2,
                ..VoteIntegrityConfig::default()
            },
        };
        use_case
            .execute(vote_cmd(markets[0], user, "velocity-first"))
            .await
            .unwrap();
        use_case
            .execute(vote_cmd(markets[1], user, "velocity-at-max"))
            .await
            .unwrap();
        assert_eq!(
            use_case
                .execute(vote_cmd(markets[2], user, "velocity-over"))
                .await
                .unwrap_err(),
            AppError::VoteVelocityExceeded
        );
    }

    #[tokio::test]
    async fn replay_short_circuits_after_the_user_reaches_the_cap() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        let clock = FakeClock::at(t0());
        let use_case = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig {
                max_votes_per_window: 1,
                ..VoteIntegrityConfig::default()
            },
        };
        use_case
            .execute(vote_cmd(market, user, "capped-replay"))
            .await
            .unwrap();
        let replay = use_case
            .execute(vote_cmd(market, user, "capped-replay"))
            .await
            .unwrap();
        assert!(replay.replayed);
    }

    #[tokio::test]
    async fn young_account_boundary_is_inclusive() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        store.set_user_created_at(user, t0() - Duration::hours(1));
        let clock = FakeClock::at(t0() + Duration::minutes(110));
        let error = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        }
        .execute(vote_cmd(market, user, "young-near"))
        .await
        .unwrap_err();
        assert_eq!(error, AppError::AccountTooYoungNearClose);

        let early_market = store
            .add_market(
                "young-early",
                MarketState::Live,
                t0() + Duration::hours(4),
                t0() + Duration::hours(3),
                MicroShares(100_000_000),
                BasisPoints(100),
            )
            .unwrap()
            .id;
        CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        }
        .execute(vote_cmd(early_market, user, "young-early"))
        .await
        .unwrap();

        let old_user = linked_user(&store);
        store.set_user_created_at(old_user, t0() - Duration::hours(100));
        CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        }
        .execute(vote_cmd(market, old_user, "old-near"))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn hidden_seq_reappears_after_resolution_on_replay() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        let clock = FakeClock::at(t0() + Duration::minutes(90));
        store.set_market_state(market, MarketState::Closing);
        let use_case = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let first = use_case
            .execute(vote_cmd(market, user, "hidden-replay"))
            .await
            .unwrap();
        assert_eq!(first.seq, None);
        store.set_market_state(market, MarketState::Paid);
        let replay = use_case
            .execute(vote_cmd(market, user, "hidden-replay"))
            .await
            .unwrap();
        assert_eq!(replay.seq, Some(1));
        assert!(replay.replayed);
    }

    // ---- D25 fence point: voting fence only + auto-expiry + replay ----

    #[tokio::test]
    async fn votes_proceed_through_a_trading_pause() {
        // A trading pause can never delay votes: CastVote takes ONLY the
        // voting-market fence and never reads the trading pause keys.
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        store.set_config_value("trading_paused", serde_json::json!(true));
        store.set_config_value(
            &format!("market_paused:{}", market.0),
            serde_json::json!(true),
        );
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        uc.execute(vote_cmd(market, user, "vote-through-pause"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_voting_pause_is_423_replay_precedes_it_and_it_auto_expires() {
        let store = InMemoryStore::new();
        let market = live_market(&store); // closes +2h, hidden +1h
        let voter = linked_user(&store);
        let paused_voter = linked_user(&store);
        let late_voter = linked_user(&store);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        let first = uc
            .execute(vote_cmd(market, voter, "vote-before"))
            .await
            .unwrap();

        store.set_config_value(
            &format!("voting_paused:{}", market.0),
            serde_json::json!(true),
        );
        let snapshot = store.snapshot();
        let err = uc
            .execute(vote_cmd(market, paused_voter, "vote-paused"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::VotingPaused);
        assert_eq!(snapshot, store.snapshot(), "423 leaves nothing observable");
        // Replay precedence: the pre-pause vote replays with NO pause check.
        let replay = uc
            .execute(vote_cmd(market, voter, "vote-before"))
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.vote_id, first.vote_id);

        // Auto-expiry (grok r2 f2): once `now ≥ tally_hidden_at` the
        // Live-set pause is void with NO admin action — the operator cannot
        // freeze the final public tally and ride D22.
        clock.set(t0() + Duration::hours(1));
        let receipt = uc
            .execute(vote_cmd(market, late_voter, "vote-after-expiry"))
            .await
            .unwrap();
        assert_eq!(
            receipt.seq, None,
            "hidden window hides the tally, not the vote"
        );
    }

    #[tokio::test]
    async fn the_same_key_with_a_different_payload_is_a_409_conflict() {
        let store = InMemoryStore::new();
        let market = live_market(&store);
        let user = linked_user(&store);
        let clock = FakeClock::at(t0());
        let uc = CastVote {
            store: &store,
            clock: &clock,
            config: VoteIntegrityConfig::default(),
        };
        uc.execute(vote_cmd(market, user, "vote-fp")).await.unwrap();
        let mut different = vote_cmd(market, user, "vote-fp");
        different.side = Side::No;
        let err = uc.execute(different).await.unwrap_err();
        assert_eq!(err, AppError::IdempotencyConflict);
        let mut guess = vote_cmd(market, user, "vote-fp");
        guess.crowd_guess_pct = 61;
        let err = uc.execute(guess).await.unwrap_err();
        assert_eq!(err, AppError::IdempotencyConflict);
        let replay = uc.execute(vote_cmd(market, user, "vote-fp")).await.unwrap();
        assert!(replay.replayed);
    }
}
