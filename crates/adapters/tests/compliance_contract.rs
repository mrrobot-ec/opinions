//! W2 Postgres compliance contract. Runs against its own scratch database
//! derived from `DATABASE_URL`, so a stray run can never truncate another
//! lane's schema. It cannot skip: an absent database FAILS the suite.
//!
//! Every `PgComplianceTx` / `PgComplianceAdminTx` method is exercised against
//! real SQL: the D33 screening algebra round-trip, the D34 AML leg union over
//! decisions + deposits + withdrawals + converted lots, the phone uniqueness
//! and atomic-attempt contract, the durable inbox, and the generic
//! `money_command_proposals` authority.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use adapters::pg::{PgComplianceStore, PgStore};
use application::error::StoreError;
use application::model::{AdminAction, AdminRole, UserId, UserStatus};
use application::money::admin::{
    ComplianceAdminStore, FrozenFundsLicense, InboxRecord, MoneyProposal, ProposalStatus,
};
use application::money::aml::{AmlDirection, AmlFlag, AmlKind, AmlLeg};
use application::money::kyc::KycEvent;
use application::money::phone_verification::{PhoneVerificationRow, PHONE_MAX_ATTEMPTS};
use application::money::sanctions::SanctionScreening;
use application::money::self_exclusion::{SelfExclusion, UserDepositLimit};
use application::money::{ComplianceDecision, ComplianceStore};
use application::ports::ScreenVerdict;
use application::ports::Store;
use serde_json::json;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

mod common;

async fn pg_pool() -> sqlx::PgPool {
    // Isolation without silence: this suite TRUNCATEs shared tables, so it runs
    // against its own scratch database derived from DATABASE_URL rather than the
    // caller's. There is no skip path: every setup failure PANICS (see
    // tests/common), because a Pg suite that silently passes without a database
    // is worse than none.
    static ONCE: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
    let url = ONCE
        .get_or_init(|| async {
            let base = common::database_url();
            let (root, _) = base
                .rsplit_once('/')
                .expect("DATABASE_URL must name a database");
            let name = "opinions_suite_compliance";
            let admin = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&base)
                .await
                .expect("connect to DATABASE_URL");
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "drop database if exists {name} with (force)"
            )))
            .execute(&admin)
            .await;
            sqlx::query(sqlx::AssertSqlSafe(format!("create database {name}")))
                .execute(&admin)
                .await
                .expect("create the suite scratch database");
            admin.close().await;
            let url = format!("{root}/{name}");
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .expect("connect to the suite scratch database");
            MIGRATOR
                .run(&pool)
                .await
                .expect("apply migrations to the suite scratch database");
            pool.close().await;
            url
        })
        .await
        .clone();
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to the suite scratch database")
}

fn pg_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn reset(pool: &sqlx::PgPool) {
    sqlx::query(
        "truncate table users, kyc_events, sanction_screenings, aml_flags, self_exclusions, \
         user_deposit_limits, phone_verifications, compliance_decisions, \
         money_command_proposals, admin_actions, deposits, withdrawals, credit_grant_lots, \
         ledger_entries, ledger_transactions, ledger_accounts restart identity cascade",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("delete from config_entries where key like 'w2-contract:%'")
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_user(pool: &sqlx::PgPool, handle: &str) -> UserId {
    let user = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle, kyc_tier, status) values ($1,$2,0,'active')")
        .bind(user.0)
        .bind(format!("{handle}-{}", user.0))
        .execute(pool)
        .await
        .unwrap();
    user
}

/// Balanced two-leg withdrawal txn: the ledger triggers reject an empty or
/// non-zero-sum transaction, so a compliance fixture must book real entries.
async fn seed_txn(pool: &sqlx::PgPool, key: &str, from: Uuid, to: Uuid, amount: i64) -> Uuid {
    // The non-empty / zero-sum assertions are deferred constraint triggers:
    // the txn row and both legs have to land in one transaction.
    let mut tx = pool.begin().await.unwrap();
    let txn: Uuid = sqlx::query_scalar(
        "insert into ledger_transactions (kind, idempotency_key) values ('withdrawal',$1) \
         returning id",
    )
    .bind(key)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    for (account, delta) in [(from, -amount), (to, amount)] {
        sqlx::query(
            "insert into ledger_entries (txn_id, account_id, amount_micro) values ($1,$2,$3)",
        )
        .bind(txn)
        .bind(account)
        .bind(delta)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    txn
}

async fn seed_accounts(pool: &sqlx::PgPool, user: UserId) -> (Uuid, Uuid) {
    let user_account: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type, owner_id, currency) \
         values ('user',$1,'usdc') returning id",
    )
    .bind(user.0)
    .fetch_one(pool)
    .await
    .unwrap();
    let withheld: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type, owner_id, currency) \
         values ('withheld',null,'usdc') returning id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    (user_account, withheld)
}

fn t0() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_100)
}

fn clear_at(now: OffsetDateTime) -> ScreenVerdict {
    ScreenVerdict::Clear {
        checked_at: now,
        expires_at: now + Duration::hours(24),
        policy_version: "geo-7".into(),
    }
}

#[tokio::test]
async fn pg_compliance_facts_round_trip_through_the_d33_algebra() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;
    let store = PgComplianceStore::from_pool(pool.clone());
    let user = seed_user(&pool, "facts").await;
    let now = t0();

    let mut tx = ComplianceStore::compliance_tx(&store).await.unwrap();
    let row = tx.lock_user(user).await.unwrap();
    assert_eq!(row.kyc_tier, 0);
    assert_eq!(row.status, "active");
    assert!(matches!(
        tx.lock_user(UserId(Uuid::new_v4())).await,
        Err(StoreError::NotFound("user"))
    ));
    assert!(tx.latest_kyc(user).await.unwrap().is_none());

    tx.insert_kyc_event(KycEvent {
        id: Uuid::new_v4(),
        user,
        from_tier: Some(0),
        to_tier: 2,
        provider_ref: Some("persona-1".into()),
        at: now,
        valid_until: Some(now + Duration::days(30)),
        policy_version: "kyc-3".into(),
        payload: json!({ "source": "webhook" }),
    })
    .await
    .unwrap();
    tx.set_kyc_tier(user, 2).await.unwrap();
    let latest = tx.latest_kyc(user).await.unwrap().unwrap();
    assert_eq!(latest.to_tier, 2);
    assert_eq!(latest.from_tier, Some(0));
    assert_eq!(latest.policy_version, "kyc-3");
    assert_eq!(latest.valid_until, Some(now + Duration::days(30)));
    assert_eq!(tx.lock_user(user).await.unwrap().kyc_tier, 2);

    // A KYC payload that is not an object still stores; the horizon is then
    // unrecoverable, so the reader reports "no validity horizon".
    tx.insert_kyc_event(KycEvent {
        id: Uuid::new_v4(),
        user,
        from_tier: Some(2),
        to_tier: 1,
        provider_ref: None,
        at: now + Duration::minutes(1),
        valid_until: Some(now + Duration::days(1)),
        policy_version: "kyc-3".into(),
        payload: json!("scalar-payload"),
    })
    .await
    .unwrap();
    let scalar = tx.latest_kyc(user).await.unwrap().unwrap();
    assert_eq!(scalar.to_tier, 1);
    assert_eq!(scalar.valid_until, None);
    assert_eq!(scalar.policy_version, "1");

    assert!(tx.latest_screening(user, "geo").await.unwrap().is_none());
    tx.insert_screening(SanctionScreening {
        id: Uuid::new_v4(),
        user,
        context: "geo".into(),
        verdict: clear_at(now),
        raw_ref: Some("maxmind".into()),
    })
    .await
    .unwrap();
    tx.insert_screening(SanctionScreening {
        id: Uuid::new_v4(),
        user,
        context: "sanctions".into(),
        verdict: ScreenVerdict::Hit,
        raw_ref: None,
    })
    .await
    .unwrap();
    let geo = tx.latest_screening(user, "geo").await.unwrap().unwrap();
    assert!(matches!(
        geo.verdict,
        ScreenVerdict::Clear { ref policy_version, .. } if policy_version == "geo-7"
    ));
    assert_eq!(geo.raw_ref.as_deref(), Some("maxmind"));
    let sanctions = tx
        .latest_screening(user, "sanctions")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sanctions.verdict, ScreenVerdict::Hit);

    tx.insert_decision(ComplianceDecision {
        id: Uuid::new_v4(),
        subject_type: "user".into(),
        subject_id: user.0,
        kind: "kyc_event".into(),
        actor: "machine".into(),
        at: now,
        payload: json!({ "to_tier": 2 }),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let decisions: i64 =
        sqlx::query_scalar("select count(*) from compliance_decisions where kind = 'kyc_event'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(decisions, 1);
}

#[tokio::test]
async fn pg_aml_legs_union_decisions_deposits_withdrawals_and_converted_lots() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;
    let store = PgComplianceStore::from_pool(pool.clone());
    let user = seed_user(&pool, "aml").await;
    let other = seed_user(&pool, "aml-other").await;
    let now = t0();
    let since = now - Duration::hours(24);

    let mut tx = ComplianceStore::compliance_tx(&store).await.unwrap();
    tx.record_aml_leg(AmlLeg {
        id: Uuid::new_v4(),
        user,
        dest: "dest-a".into(),
        amount_micro: 499_000_000,
        at: now,
        direction: AmlDirection::Withdrawal,
    })
    .await
    .unwrap();
    tx.record_aml_leg(AmlLeg {
        id: Uuid::new_v4(),
        user: other,
        dest: "dest-a".into(),
        amount_micro: 250_000_000,
        at: now,
        direction: AmlDirection::Deposit,
    })
    .await
    .unwrap();
    // A leg at a different dest must drop out of a per-dest read.
    tx.record_aml_leg(AmlLeg {
        id: Uuid::new_v4(),
        user,
        dest: "dest-elsewhere".into(),
        amount_micro: 300_000_000,
        at: now,
        direction: AmlDirection::Withdrawal,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // A decision row whose payload is not a leg is skipped, not guessed at:
    // an unreadable fact must never inflate a structuring count.
    sqlx::query(
        "insert into compliance_decisions (subject_type, subject_id, kind, actor, at, payload)
         values ('aml_leg',$1,'aml_leg','machine',$2,'{\"partial\":true}'::jsonb)",
    )
    .bind(user.0)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    // Chain-observed deposit, an unmatched (user-less) inflow that must be
    // skipped, and a refunded deposit that must not count.
    for (handle, amount, status, owner) in [
        ("sig-a", 25_000_000_i64, "observed_finalized", Some(user)),
        ("sig-b", 40_000_000, "observed_finalized", None),
        ("sig-c", 90_000_000, "refunded", Some(user)),
    ] {
        sqlx::query(
            "insert into deposits (user_id, chain_sig, amount_micro, status, created_at, \
             source_address) values ($1,$2,$3,$4,$5,'dest-a')",
        )
        .bind(owner.map(|u| u.0))
        .bind(handle)
        .bind(amount)
        .bind(status)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }

    let (user_account, withheld) = seed_accounts(&pool, user).await;
    let hold = seed_txn(&pool, "hold-1", user_account, withheld, 499_000_000).await;
    let settle = seed_txn(&pool, "settle-1", withheld, user_account, 499_000_000).await;
    let denied_hold = seed_txn(&pool, "hold-2", user_account, withheld, 10_000_000).await;
    let release = seed_txn(&pool, "release-2", withheld, user_account, 10_000_000).await;
    sqlx::query(
        "insert into withdrawals (user_id, dest_address, amount_micro, status, review_state, \
         send_state, hold_tx_id, settle_tx_id, requested_at) \
         values ($1,'dest-a',499000000,'settled','approved','finalized',$2,$3,$4)",
    )
    .bind(user.0)
    .bind(hold)
    .bind(settle)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into withdrawals (user_id, dest_address, amount_micro, status, review_state, \
         send_state, hold_tx_id, release_tx_id, requested_at) \
         values ($1,'dest-b',10000000,'denied','screening','unsent',$2,$3,$4)",
    )
    .bind(user.0)
    .bind(denied_hold)
    .bind(release)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "insert into credit_grant_lots (user_id, source, amount_micro, granted_at, grant_class, \
         policy_version, converted_at) values ($1,'signup',5000000,$2,'real_money','v1',$3)",
    )
    .bind(user.0)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let mut tx = ComplianceStore::compliance_tx(&store).await.unwrap();
    let user_legs = tx.list_aml_legs(user, since).await.unwrap();
    // two recorded legs + observed deposit + settled withdrawal + convert
    assert_eq!(user_legs.len(), 5);
    assert!(user_legs
        .iter()
        .any(|leg| leg.direction == AmlDirection::Deposit && leg.amount_micro == 25_000_000));
    assert!(user_legs
        .iter()
        .any(|leg| leg.dest.starts_with("convert:") && leg.amount_micro == 5_000_000));
    assert!(!user_legs.iter().any(|leg| leg.dest == "dest-b"));
    assert!(!user_legs.iter().any(|leg| leg.amount_micro == 90_000_000));

    let dest_legs = tx.list_dest_aml_legs("dest-a", since).await.unwrap();
    // both recorded legs (two users) + observed deposit + settled withdrawal
    assert_eq!(dest_legs.len(), 4);
    assert!(dest_legs.iter().any(|leg| leg.user == other));

    let flag = AmlFlag {
        id: Uuid::new_v4(),
        user,
        rule: AmlKind::Structuring,
        window_label: "24h".into(),
        evidence: json!({ "structuring_user": 4 }),
        open: true,
        at: now,
    };
    tx.insert_aml_flag(flag.clone()).await.unwrap();
    let open = tx.open_aml_flags(user).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].rule, AmlKind::Structuring);
    assert_eq!(open[0].window_label, "24h");
    tx.commit().await.unwrap();

    let mut admin = store.admin_tx().await.unwrap();
    assert!(admin.get_aml_flag(flag.id).await.unwrap().open);
    let cleared = admin.clear_aml_flag(flag.id).await.unwrap();
    assert!(!cleared.open);
    assert!(admin.open_aml_flags(user).await.unwrap().is_empty());
    assert!(matches!(
        admin.get_aml_flag(Uuid::new_v4()).await,
        Err(StoreError::NotFound("aml flag"))
    ));
    assert_eq!(
        admin.settled_dests(user).await.unwrap(),
        vec!["dest-a".to_string()]
    );
    admin
        .record_settled_dest(user, "dest-z".into())
        .await
        .unwrap();
    admin.commit().await.unwrap();
}

#[tokio::test]
async fn pg_statuses_exclusions_limits_and_phone_verification() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;
    let store = PgComplianceStore::from_store(&PgStore::from_pool(pool.clone()));
    let user = seed_user(&pool, "status").await;
    let rival = seed_user(&pool, "rival").await;
    let now = t0();

    let mut tx = store.admin_tx().await.unwrap();
    assert_eq!(
        tx.set_user_status(user, UserStatus::ShadowLimited)
            .await
            .unwrap(),
        UserStatus::ShadowLimited
    );
    assert_eq!(tx.lock_user(user).await.unwrap().status, "shadow_limited");
    tx.set_user_status(user, UserStatus::Active).await.unwrap();

    assert!(tx.active_self_exclusion(user).await.unwrap().is_none());
    let exclusion = SelfExclusion {
        id: Uuid::new_v4(),
        user,
        starts_at: now,
        cooling_off_until: now + Duration::hours(24),
        lifted_at: None,
    };
    tx.insert_self_exclusion(exclusion.clone()).await.unwrap();
    assert_eq!(
        tx.active_self_exclusion(user).await.unwrap().unwrap().id,
        exclusion.id
    );
    assert_eq!(
        tx.get_self_exclusion(exclusion.id)
            .await
            .unwrap()
            .cooling_off_until,
        now + Duration::hours(24)
    );
    let lifted = tx
        .lift_self_exclusion(exclusion.id, now + Duration::hours(25))
        .await
        .unwrap();
    assert_eq!(lifted.lifted_at, Some(now + Duration::hours(25)));
    assert!(tx.active_self_exclusion(user).await.unwrap().is_none());
    assert!(matches!(
        tx.get_self_exclusion(Uuid::new_v4()).await,
        Err(StoreError::NotFound("self exclusion"))
    ));

    assert!(tx.get_deposit_limit(user).await.unwrap().is_none());
    tx.upsert_deposit_limit(UserDepositLimit {
        user,
        limit_micro: 50_000_000,
        pending_limit_micro: None,
        pending_effective_at: None,
        updated_at: now,
    })
    .await
    .unwrap();
    tx.upsert_deposit_limit(UserDepositLimit {
        user,
        limit_micro: 50_000_000,
        pending_limit_micro: Some(900_000_000),
        pending_effective_at: Some(now + Duration::hours(24)),
        updated_at: now,
    })
    .await
    .unwrap();
    let limit = tx.get_deposit_limit(user).await.unwrap().unwrap();
    assert_eq!(limit.limit_micro, 50_000_000);
    assert_eq!(limit.pending_limit_micro, Some(900_000_000));
    assert_eq!(limit.pending_effective_at, Some(now + Duration::hours(24)));

    // Phone: one row per (number hmac, key version); the second account
    // presenting the same number is refused.
    assert!(tx.active_phone_challenge(user).await.unwrap().is_none());
    assert!(tx.phone_by_hmac("hmac-a", 1).await.unwrap().is_none());
    let challenge = PhoneVerificationRow {
        id: Uuid::new_v4(),
        user,
        number_hmac: "hmac-a".into(),
        hmac_key_version: 1,
        challenge: Some("hashed-code".into()),
        expires_at: Some(now + Duration::minutes(10)),
        attempts: 0,
        verified_at: None,
        provider_ref: Some("sandbox".into()),
    };
    tx.insert_phone_challenge(challenge.clone()).await.unwrap();
    tx.commit().await.unwrap();

    // Two accounts, one number: the loser's transaction is poisoned by the
    // unique violation, so the race is settled by rolling it back entirely.
    let mut loser = store.admin_tx().await.unwrap();
    assert!(matches!(
        loser
            .insert_phone_challenge(PhoneVerificationRow {
                id: Uuid::new_v4(),
                user: rival,
                ..challenge.clone()
            })
            .await,
        Err(StoreError::Conflict("phone number already bound"))
    ));
    drop(loser);

    let mut tx = store.admin_tx().await.unwrap();
    // A different key version is a distinct row (rotation stays possible).
    tx.insert_phone_challenge(PhoneVerificationRow {
        id: Uuid::new_v4(),
        user: rival,
        hmac_key_version: 2,
        expires_at: Some(now + Duration::minutes(5)),
        ..challenge.clone()
    })
    .await
    .unwrap();
    assert_eq!(
        tx.active_phone_challenge(user).await.unwrap().unwrap().id,
        challenge.id
    );
    assert_eq!(
        tx.phone_by_hmac("hmac-a", 1).await.unwrap().unwrap().user,
        user
    );
    assert!(tx.verified_phone_for_user(user).await.unwrap().is_none());
    // Re-arming this account's own unverified row keeps the binding and
    // resets attempts, so an expired code never locks the number out.
    tx.consume_phone_attempt(challenge.id).await.unwrap();
    let rearmed = tx
        .refresh_phone_challenge(challenge.id, "second-digest", now + Duration::minutes(20))
        .await
        .unwrap();
    assert_eq!(rearmed.id, challenge.id);
    assert_eq!(rearmed.attempts, 0);
    assert_eq!(rearmed.challenge.as_deref(), Some("second-digest"));
    assert_eq!(rearmed.expires_at, Some(now + Duration::minutes(20)));
    assert!(matches!(
        tx.refresh_phone_challenge(Uuid::new_v4(), "x", now).await,
        Err(StoreError::NotFound("phone challenge"))
    ));
    for expected in 1..=PHONE_MAX_ATTEMPTS {
        assert_eq!(
            tx.consume_phone_attempt(challenge.id)
                .await
                .unwrap()
                .attempts,
            expected
        );
    }
    assert!(matches!(
        tx.consume_phone_attempt(challenge.id).await,
        Err(StoreError::Conflict("phone challenge exhausted"))
    ));
    let verified = tx
        .mark_phone_verified(challenge.id, now + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(verified.verified_at, Some(now + Duration::minutes(1)));
    assert!(verified.challenge.is_none());
    assert_eq!(
        tx.verified_phone_for_user(user).await.unwrap().unwrap().id,
        challenge.id
    );
    // A verified binding is never re-armed: the row is the account's proof.
    assert!(matches!(
        tx.refresh_phone_challenge(challenge.id, "third", now).await,
        Err(StoreError::NotFound("phone challenge"))
    ));
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn pg_inbox_proposals_licenses_config_and_audit() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;
    let store = PgComplianceStore::from_pool(pool.clone());
    let user = seed_user(&pool, "inbox").await;
    let now = t0();

    let mut tx = store.admin_tx().await.unwrap();
    assert!(tx.inbox_get("persona", "evt-1").await.unwrap().is_none());
    let record = InboxRecord {
        provider: "persona".into(),
        event_id: "evt-1".into(),
        payload_hash: "hash-1".into(),
        payload: json!({ "event": "approved" }),
        user_id: Some(user),
        received_at: now,
    };
    tx.inbox_insert(record.clone()).await.unwrap();
    let stored = tx.inbox_get("persona", "evt-1").await.unwrap().unwrap();
    assert_eq!(stored.payload_hash, "hash-1");
    assert_eq!(stored.payload, json!({ "event": "approved" }));
    assert_eq!(stored.user_id, Some(user));
    assert_eq!(stored.received_at, now);
    tx.commit().await.unwrap();

    let mut duplicate = store.admin_tx().await.unwrap();
    assert!(matches!(
        duplicate.inbox_insert(record).await,
        Err(StoreError::Conflict("inbox event"))
    ));
    drop(duplicate);

    let mut tx = store.admin_tx().await.unwrap();
    // A record with no resolved user reads back as `None`, never as a guess.
    tx.inbox_insert(InboxRecord {
        provider: "persona".into(),
        event_id: "evt-2".into(),
        payload_hash: "hash-2".into(),
        payload: json!({}),
        user_id: None,
        received_at: now,
    })
    .await
    .unwrap();
    assert!(tx
        .inbox_get("persona", "evt-2")
        .await
        .unwrap()
        .unwrap()
        .user_id
        .is_none());

    assert!(tx
        .get_proposal_by_replay("replay-1")
        .await
        .unwrap()
        .is_none());
    let proposal = MoneyProposal {
        id: Uuid::new_v4(),
        kind: "clear_aml".into(),
        subject_id: user.0,
        payload_hash: "hash-p".into(),
        proposer_token_id: "fin-1".into(),
        confirmer_token_id: None,
        reason: "reviewed".into(),
        status: ProposalStatus::Pending,
        confirm_not_before: now,
        expires_at: now + Duration::minutes(15),
        replay_key: "replay-1".into(),
        created_at: now,
    };
    tx.insert_proposal(proposal.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let mut replayed = store.admin_tx().await.unwrap();
    assert!(matches!(
        replayed
            .insert_proposal(MoneyProposal {
                id: Uuid::new_v4(),
                ..proposal.clone()
            })
            .await,
        Err(StoreError::Conflict("proposal replay"))
    ));
    drop(replayed);

    let mut tx = store.admin_tx().await.unwrap();
    let by_replay = tx
        .get_proposal_by_replay("replay-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_replay.id, proposal.id);
    assert_eq!(by_replay.status, ProposalStatus::Pending);
    assert_eq!(by_replay.confirmer_token_id, None);
    let confirmed = tx
        .confirm_proposal(proposal.id, "root-1", now + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(confirmed.status, ProposalStatus::Confirmed);
    assert_eq!(confirmed.confirmer_token_id.as_deref(), Some("root-1"));
    assert_eq!(
        tx.get_proposal(proposal.id).await.unwrap().reason,
        "reviewed"
    );
    assert!(matches!(
        tx.get_proposal(Uuid::new_v4()).await,
        Err(StoreError::NotFound("proposal"))
    ));
    tx.commit().await.unwrap();

    // A zero-width confirm window is refused by the schema, and a schema
    // refusal is an integrity error — never silently reported as a replay.
    let mut invalid = store.admin_tx().await.unwrap();
    assert!(matches!(
        invalid
            .insert_proposal(MoneyProposal {
                id: Uuid::new_v4(),
                replay_key: "replay-2".into(),
                expires_at: now,
                confirm_not_before: now,
                ..proposal.clone()
            })
            .await,
        Err(StoreError::Integrity(_))
    ));
    drop(invalid);

    let mut tx = store.admin_tx().await.unwrap();

    let license = FrozenFundsLicense {
        id: Uuid::new_v4(),
        user,
        dest: "counsel-escrow".into(),
        amount_micro: 1_250_000,
    };
    tx.insert_frozen_license(license.clone()).await.unwrap();
    assert_eq!(tx.get_frozen_license(license.id).await.unwrap(), license);
    assert!(matches!(
        tx.get_frozen_license(Uuid::new_v4()).await,
        Err(StoreError::NotFound("frozen license"))
    ));

    assert!(tx
        .config_i64("w2-contract:missing")
        .await
        .unwrap()
        .is_none());
    assert!(tx
        .config_json("w2-contract:missing")
        .await
        .unwrap()
        .is_none());
    tx.set_config_i64("w2-contract:cap", 25_000_000)
        .await
        .unwrap();
    assert_eq!(
        tx.config_i64("w2-contract:cap").await.unwrap(),
        Some(25_000_000)
    );
    tx.set_config_json("w2-contract:allowset", json!(["CA", "NY"]))
        .await
        .unwrap();
    assert_eq!(
        tx.config_json("w2-contract:allowset").await.unwrap(),
        Some(json!(["CA", "NY"]))
    );
    // A non-integer value is not coerced into an i64 reading.
    assert!(tx
        .config_i64("w2-contract:allowset")
        .await
        .unwrap()
        .is_none());

    tx.audit_insert(AdminAction {
        actor_role: AdminRole::Superadmin,
        actor_token_digest: "root-1".into(),
        action: "confirm_clear_aml".into(),
        subject: format!("user:{}", user.0),
        before: None,
        after: Some(json!({ "open": false })),
        reason: Some("reviewed".into()),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let audits: i64 = sqlx::query_scalar("select count(*) from admin_actions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn the_money_effect_transaction_shares_one_proposal_authority_with_the_admin_surface() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;
    let compliance = PgComplianceStore::from_pool(pool.clone());
    let store = PgStore::from_pool(pool.clone());
    let user = seed_user(&pool, "shared-authority").await;
    let now = t0();

    let proposal = MoneyProposal {
        id: Uuid::new_v4(),
        kind: "manual_deposit_admission".into(),
        subject_id: user.0,
        payload_hash: "hash-d".into(),
        proposer_token_id: "fin-1".into(),
        confirmer_token_id: None,
        reason: "observation matched".into(),
        status: ProposalStatus::Pending,
        confirm_not_before: now,
        expires_at: now + Duration::minutes(15),
        replay_key: format!("deposit-admit:{}", user.0),
        created_at: now,
    };
    // The admin surface proposes...
    let mut admin = compliance.admin_tx().await.unwrap();
    admin.insert_proposal(proposal.clone()).await.unwrap();
    admin.commit().await.unwrap();

    // ...and the money-effect transaction confirms it in the SAME tx that
    // will carry the economic effect, reading the same rows.
    let mut money = store.deposit_admission_tx().await.unwrap();
    assert_eq!(
        money
            .get_proposal_by_replay(&proposal.replay_key)
            .await
            .unwrap()
            .unwrap()
            .id,
        proposal.id
    );
    assert_eq!(
        money.get_proposal(proposal.id).await.unwrap().status,
        ProposalStatus::Pending
    );
    let confirmed = money
        .confirm_proposal(proposal.id, "root-1", now + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(confirmed.status, ProposalStatus::Confirmed);
    assert_eq!(confirmed.confirmer_token_id.as_deref(), Some("root-1"));
    // Confirmation is a CAS on `pending`: the second confirmer moves nothing.
    let second = money
        .confirm_proposal(proposal.id, "root-2", now + Duration::minutes(2))
        .await
        .unwrap();
    assert_eq!(second.confirmer_token_id.as_deref(), Some("root-1"));
    // A fresh proposal opened from inside the money transaction lands in the
    // same authority the admin surface reads.
    money
        .insert_proposal(MoneyProposal {
            id: Uuid::new_v4(),
            replay_key: format!("deposit-refund:{}", user.0),
            ..proposal.clone()
        })
        .await
        .unwrap();
    assert!(matches!(
        money.get_proposal(Uuid::new_v4()).await,
        Err(StoreError::NotFound("proposal"))
    ));
    money.commit().await.unwrap();

    let mut admin = compliance.admin_tx().await.unwrap();
    assert!(admin
        .get_proposal_by_replay(&format!("deposit-refund:{}", user.0))
        .await
        .unwrap()
        .is_some());
    admin.commit().await.unwrap();
}
