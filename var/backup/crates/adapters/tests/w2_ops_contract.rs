//! W2 Pg contract suite (plan D26/D27/D30): audit atomicity, the receivables
//! subledger, dual-control rows, durable ops-job commands, publication
//! commands, and the repeatable-read invariant snapshot — each test on a
//! FRESH database (stronger than per-test truncation). It cannot skip: an
//! absent `DATABASE_URL` FAILS the suite (see `tests/common`).
#![allow(clippy::unwrap_used)]

use adapters::pg::PgStore;
use application::credit_deposit::{CreditDeposit, CreditDepositCmd};
use application::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
use application::error::StoreError;
use application::model::{
    AdminAction, AdminContext, AdminRole, MarketId, MarketUnwind, ProposalStatus, Receivable,
    ReceivableMovement, ReceivableMovementKind, UnwindStage, UserId,
};
use application::ops::audit::OpsPolicy;
use application::ops::receivable_collection::{WriteOffCmd, WriteOffReceivable};
use application::ports::{
    Clock, OpsJobCommand, OpsJobStatus, OpsQueries, PublicationCommand, PublicationCommandStatus,
    RemedialCreditProposal, Store, WithdrawalEligibility,
};
use domain::money::MicroUsd;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

mod common;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

struct TestDb {
    store: PgStore,
    control: PgPool,
    database: String,
}

impl TestDb {
    async fn new(label: &str) -> Self {
        let url = common::database_url();
        let control = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let database = format!("opinions_w2_{label}_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("create database {database}"))
            .execute(&control)
            .await
            .unwrap();
        let (base, query) = url
            .split_once('?')
            .map_or((url.as_str(), None), |parts| (parts.0, Some(parts.1)));
        let root = base.rsplit_once('/').unwrap().0;
        let test_url = query.map_or_else(
            || format!("{root}/{database}"),
            |query| format!("{root}/{database}?{query}"),
        );
        let store = PgStore::connect(&test_url).await.unwrap();
        MIGRATOR.run(store.pool_handle()).await.unwrap();
        Self {
            store,
            control,
            database,
        }
    }

    async fn cleanup(self) {
        self.store.pool_handle().close().await;
        sqlx::query(&format!("drop database {} with (force)", self.database))
            .execute(&self.control)
            .await
            .unwrap();
        self.control.close().await;
    }

    async fn user(&self, handle: &str) -> UserId {
        let id = Uuid::new_v4();
        sqlx::query("insert into users (id, handle) values ($1, $2)")
            .bind(id)
            .bind(handle)
            .execute(self.store.pool_handle())
            .await
            .unwrap();
        UserId(id)
    }

    async fn market(&self, slug: &str) -> MarketId {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into markets (id, slug, question, status, min_votes_to_resolve, opens_at, closes_at, tally_hidden_at)
             values ($1, $2, $2, 'voided', 3, now(), now(), now())",
        )
        .bind(id)
        .bind(slug)
        .execute(self.store.pool_handle())
        .await
        .unwrap();
        for idx in 0_i16..2 {
            sqlx::query("insert into outcomes (id, market_id, idx, label) values ($1, $2, $3, $4)")
                .bind(Uuid::new_v4())
                .bind(id)
                .bind(idx)
                .bind(if idx == 0 { "yes" } else { "no" })
                .execute(self.store.pool_handle())
                .await
                .unwrap();
        }
        MarketId(id)
    }

    /// Opens a receivable (opened movement included) whose origin reversal
    /// transaction really exists in the ledger with a matching negative
    /// House leg — identity 7 reconciles it.
    async fn open_receivable(&self, user: UserId, market: MarketId, opened: i64) -> (Uuid, Uuid) {
        // Fund the house so the shortfall booking has cash to move.
        EnsureGenesis { store: &self.store }
            .execute(EnsureGenesisCmd {
                currency: domain::ledger::Currency::Usdc,
                amount: MicroUsd(opened * 2),
            })
            .await
            .unwrap();
        let mut tx = self.store.unwind_tx().await.unwrap();
        tx.serialize_key(&format!("recv-fixture:{}", user.0))
            .await
            .unwrap();
        let house = tx
            .account(
                application::model::OwnerRef::House,
                domain::ledger::Currency::Usdc,
            )
            .await
            .unwrap();
        let user_account = tx
            .account(
                application::model::OwnerRef::User(user),
                domain::ledger::Currency::Usdc,
            )
            .await
            .unwrap();
        // The origin reversal: house covers the user's unpayable debit.
        let reversal = tx
            .ledger_apply(
                domain::ledger::TxnKind::Reversal,
                &format!("unwind-fixture:{}", user.0),
                &[
                    domain::ledger::Entry {
                        account: house,
                        amount: MicroUsd(-opened),
                    },
                    domain::ledger::Entry {
                        account: user_account,
                        amount: MicroUsd(opened),
                    },
                ],
            )
            .await
            .unwrap();
        // An admin audit anchors the movement's audit_id FK.
        tx.audit_insert(AdminAction {
            actor_role: AdminRole::Finance,
            actor_token_digest: "digest-finance".into(),
            action: "unwind_confirm".into(),
            subject: format!("market:{}", market.0),
            before: None,
            after: None,
            reason: Some("fixture".into()),
        })
        .await
        .unwrap();
        let receivable = Receivable {
            id: Uuid::new_v4(),
            market,
            user,
            origin_reversal_txn: reversal,
            opened_micro: opened,
        };
        tx.insert_receivable(receivable).await.unwrap();
        tx.insert_receivable_movement(ReceivableMovement {
            id: Uuid::new_v4(),
            receivable: receivable.id,
            kind: ReceivableMovementKind::Opened,
            amount_micro: opened,
            actor: "digest-finance".into(),
            cash_txn: None,
            idempotency_key: format!("open:{}", receivable.id),
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (receivable.id, reversal)
    }
}

struct WallClock;
impl Clock for WallClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// A clock pinned ahead of the dual-control delay.
struct SkewedClock(time::Duration);
impl Clock for SkewedClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc() + self.0
    }
}

fn finance() -> AdminContext {
    AdminContext::Admin {
        token_digest: "digest-finance".into(),
        role: AdminRole::Finance,
    }
}

fn superadmin() -> AdminContext {
    AdminContext::Admin {
        token_digest: "digest-superadmin".into(),
        role: AdminRole::Superadmin,
    }
}

#[tokio::test]
async fn pg_audit_rows_commit_with_their_transaction_and_page_back() {
    let db = TestDb::new("audit").await;
    // Uncommitted audit rows are invisible (atomicity with the effect).
    {
        let mut tx = db.store.ops_audit_tx().await.unwrap();
        tx.audit_insert(AdminAction {
            actor_role: AdminRole::Ops,
            actor_token_digest: "digest-ops".into(),
            action: "dropped".into(),
            subject: "market:none".into(),
            before: None,
            after: None,
            reason: None,
        })
        .await
        .unwrap();
        drop(tx); // rollback
    }
    assert!(db.store.audit_page(None, 10).await.unwrap().is_empty());
    let mut tx = db.store.ops_audit_tx().await.unwrap();
    tx.audit_insert(AdminAction {
        actor_role: AdminRole::Ops,
        actor_token_digest: "digest-ops".into(),
        action: "kept".into(),
        subject: "market:some".into(),
        before: None,
        after: Some(serde_json::json!({ "ok": true })),
        reason: Some("reason".into()),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let page = db.store.audit_page(None, 10).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].action.action, "kept");
    assert_eq!(page[0].action.actor_token_digest, "digest-ops");
    db.cleanup().await;
}

#[tokio::test]
async fn pg_receivable_lifecycle_auto_collects_on_deposit_and_reconciles() {
    let db = TestDb::new("recv").await;
    let user = db.user("debtor").await;
    // Phase 7 D32 admission gates the deposit: the debtor needs full KYC and
    // fresh Clear geo/sanctions or the $200 lands in compliance hold and
    // nothing auto-collects.
    let now = OffsetDateTime::now_utc();
    sqlx::query("update users set kyc_tier = 2 where id = $1")
        .bind(user.0)
        .execute(db.store.pool_handle())
        .await
        .unwrap();
    for context in ["geo", "sanctions"] {
        sqlx::query(
            r"insert into sanction_screenings
                (id, user_id, context, verdict, checked_at, expires_at, policy_version)
               values ($1, $2, $3, 'clear', $4, $5, 'test')",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(user.0)
        .bind(context)
        .bind(now)
        .bind(now + time::Duration::hours(4))
        .execute(db.store.pool_handle())
        .await
        .unwrap();
    }
    let market = db.market("recv-market").await;
    let (receivable, reversal) = db.open_receivable(user, market, 50_000_000).await;

    // Blocked at zero cash.
    let view = db.store.withdrawal_eligibility(user).await.unwrap();
    assert!(!view.eligible);
    assert_eq!(view.open_receivables_micro, 50_000_000);

    // Identity 7 reconciles the fixture's origin reversal transaction.
    let report = application::integrity::invariant_sweep::run(&db.store)
        .await
        .unwrap();
    assert!(report.pass, "{:?}", report.identities);

    // A $200 deposit auto-collects the $50 receivable in the SAME tx.
    let receipt = CreditDeposit { store: &db.store }
        .execute_as(
            CreditDepositCmd {
                user,
                amount: MicroUsd(200_000_000),
                chain_sig: "faucet:pg-1".into(),
                idempotency_key: "faucet:pg-1".into(),
            },
            &finance(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.collected_micro, 50_000_000);
    let after = db.store.withdrawal_eligibility(user).await.unwrap();
    assert!(after.eligible);
    assert_eq!(after.open_receivables_micro, 0);
    // 50M fixture credit + 200M deposit - 50M collected.
    assert_eq!(after.cash_micro, 200_000_000);
    // The deposit's audit fact landed atomically too.
    let page = db.store.audit_page(None, 10).await.unwrap();
    assert!(page.iter().any(|row| row.action.action == "faucet_deposit"));
    // Reconciliation: collected equals opened for the origin txn.
    let mut invariants = db.store.invariant_read_tx().await.unwrap();
    let recon = invariants.receivable_reconciliation().await.unwrap();
    let row = recon
        .iter()
        .find(|row| row.origin_reversal_txn == reversal)
        .unwrap();
    assert_eq!(row.opened_micro, 50_000_000);
    assert_eq!(row.house_shortfall_micro, 50_000_000);
    assert_eq!(row.collected_micro, 50_000_000);
    drop(invariants);
    let report = application::integrity::invariant_sweep::run(&db.store)
        .await
        .unwrap();
    assert!(report.pass, "{:?}", report.identities);
    let _ = receivable;
    db.cleanup().await;
}

#[tokio::test]
async fn pg_write_off_dual_control_is_schema_enforced() {
    let db = TestDb::new("writeoff").await;
    let user = db.user("debtor").await;
    let market = db.market("wo-market").await;
    let (receivable, _) = db.open_receivable(user, market, 40_000_000).await;

    let propose = WriteOffReceivable {
        store: &db.store,
        clock: &WallClock,
        policy: OpsPolicy::default(),
        actor: finance(),
    };
    let proposal = propose
        .propose(WriteOffCmd {
            receivable,
            reason: "uncollectable".into(),
            idempotency_key: "wo-pg-1".into(),
        })
        .await
        .unwrap();
    assert_eq!(proposal.status, ProposalStatus::Pending);
    // Same-principal confirm refused in the use case; the DB CHECK is the
    // backstop (confirmer <> proposer).
    let db_check = sqlx::query(
        "update receivable_write_off_proposals set confirmer_token_id = proposer_token_id where id = $1",
    )
    .bind(proposal.id)
    .execute(db.store.pool_handle())
    .await;
    assert!(db_check.is_err(), "schema enforces distinct principals");
    // Distinct principal + elapsed delay confirms.
    let confirm = WriteOffReceivable {
        store: &db.store,
        clock: &SkewedClock(time::Duration::seconds(61)),
        policy: OpsPolicy::default(),
        actor: superadmin(),
    };
    let confirmed = confirm
        .confirm(WriteOffCmd {
            receivable,
            reason: "uncollectable".into(),
            idempotency_key: "wo-pg-1".into(),
        })
        .await
        .unwrap();
    assert_eq!(confirmed.status, ProposalStatus::Confirmed);
    let view = db.store.withdrawal_eligibility(user).await.unwrap();
    assert!(view.eligible, "written off: {view:?}");
    db.cleanup().await;
}

#[tokio::test]
async fn pg_remedial_and_job_command_rows_round_trip() {
    let db = TestDb::new("remedial").await;
    let user = db.user("victim").await;
    let market = db.market("rem-market").await;
    let mut tx = db.store.unwind_tx().await.unwrap();
    let proposal = RemedialCreditProposal {
        id: Uuid::new_v4(),
        market,
        user,
        idempotency_key: "rem-pg-1".into(),
        amount_micro: 25_000_000,
        proposer_token_id: "digest-finance".into(),
        confirmer_token_id: None,
        reason: "goodwill".into(),
        status: ProposalStatus::Pending,
        confirm_not_before: OffsetDateTime::now_utc(),
    };
    tx.insert_remedial(proposal.clone()).await.unwrap();
    tx.commit().await.unwrap();
    // A duplicate key aborts ITS OWN transaction only.
    let mut dup_tx = db.store.unwind_tx().await.unwrap();
    let duplicate = dup_tx.insert_remedial(proposal.clone()).await.unwrap_err();
    assert_eq!(duplicate, StoreError::Conflict("remedial proposal"));
    drop(dup_tx);
    let mut tx = db.store.unwind_tx().await.unwrap();
    let fetched = tx.remedial_by_key("rem-pg-1").await.unwrap().unwrap();
    assert_eq!(fetched, proposal);
    let mut settled = proposal.clone();
    settled.status = ProposalStatus::Confirmed;
    settled.confirmer_token_id = Some("digest-superadmin".into());
    tx.save_remedial(&settled).await.unwrap();
    assert_eq!(
        tx.remedial_credited_for_market(market).await.unwrap(),
        25_000_000
    );
    // Job commands: insert → exactly-once claim under FOR UPDATE SKIP LOCKED.
    let command = OpsJobCommand {
        id: Uuid::new_v4(),
        kind: "refanout".into(),
        subject: format!("market:{}", market.0),
        idempotency_key: "job-pg-1".into(),
        requested_by: "digest-ops".into(),
        status: OpsJobStatus::Pending,
        attempts: 0,
        lease_expires_at: None,
        error: None,
    };
    tx.insert_job_command(command.clone()).await.unwrap();
    tx.commit().await.unwrap();
    let now = OffsetDateTime::now_utc();
    let mut claim = db.store.unwind_tx().await.unwrap();
    let due = claim
        .due_job_commands(now, now + time::Duration::seconds(60), 10)
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].status, OpsJobStatus::Executing);
    assert_eq!(due[0].attempts, 1);
    claim.commit().await.unwrap();
    let mut second = db.store.unwind_tx().await.unwrap();
    let empty = second
        .due_job_commands(now, now + time::Duration::seconds(60), 10)
        .await
        .unwrap();
    assert!(empty.is_empty(), "lease holds until it lapses");
    second.commit().await.unwrap();
    db.cleanup().await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pg_manual_ops_port_covers_every_durable_lookup_and_failure_contract() {
    let db = TestDb::new("ops_ports").await;
    let user = db.user("ops-port-user").await;
    let market = db.market("ops-port-market").await;
    let now = OffsetDateTime::now_utc();

    let mut tx = db.store.unwind_tx().await.unwrap();
    assert!(tx.unwind_for_update(market).await.unwrap().is_none());
    let unwind = MarketUnwind {
        market,
        unwind_key: "unwind-port-key".into(),
        stage: UnwindStage::Proposed,
        proposer_token_id: "superadmin-a".into(),
        confirmer_token_id: None,
        reason: "wrong fraud decision".into(),
        confirm_not_before: now,
        reversal_txn: None,
    };
    tx.insert_unwind(unwind.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.store.unwind_tx().await.unwrap();
    assert_eq!(
        tx.unwind_for_update(market).await.unwrap(),
        Some(unwind.clone())
    );
    let mut applied = unwind.clone();
    applied.stage = UnwindStage::Applied;
    applied.confirmer_token_id = Some("finance-b".into());
    applied.reversal_txn = Some(Uuid::new_v4());
    tx.save_unwind(&applied).await.unwrap();
    tx.commit().await.unwrap();

    let mut duplicate = db.store.unwind_tx().await.unwrap();
    assert_eq!(
        duplicate.insert_unwind(unwind.clone()).await.unwrap_err(),
        StoreError::Conflict("market unwind")
    );
    drop(duplicate);
    let mut bad_foreign_key = db.store.unwind_tx().await.unwrap();
    let mut invalid_unwind = unwind.clone();
    invalid_unwind.market = MarketId(Uuid::new_v4());
    invalid_unwind.unwind_key = "unwind-bad-foreign-key".into();
    assert!(matches!(
        bad_foreign_key.insert_unwind(invalid_unwind).await,
        Err(StoreError::Integrity(_))
    ));
    drop(bad_foreign_key);
    let mut missing = db.store.unwind_tx().await.unwrap();
    let mut absent = unwind.clone();
    absent.market = MarketId(Uuid::new_v4());
    assert_eq!(
        missing.save_unwind(&absent).await.unwrap_err(),
        StoreError::Invariant("save for unknown unwind")
    );
    drop(missing);

    EnsureGenesis { store: &db.store }
        .execute(EnsureGenesisCmd {
            currency: domain::ledger::Currency::Usdc,
            amount: MicroUsd(100_000_000),
        })
        .await
        .unwrap();
    let mut ledger = db.store.unwind_tx().await.unwrap();
    let house = ledger
        .account(
            application::model::OwnerRef::House,
            domain::ledger::Currency::Usdc,
        )
        .await
        .unwrap();
    let pool = ledger
        .account(
            application::model::OwnerRef::MarketPool(market),
            domain::ledger::Currency::Usdc,
        )
        .await
        .unwrap();
    let seed_txn = ledger
        .ledger_apply(
            domain::ledger::TxnKind::Seed,
            "ops-port-seed",
            &[
                domain::ledger::Entry {
                    account: house,
                    amount: MicroUsd(-1_000_000),
                },
                domain::ledger::Entry {
                    account: pool,
                    amount: MicroUsd(1_000_000),
                },
            ],
        )
        .await
        .unwrap();
    ledger.commit().await.unwrap();
    let yes_outcome: Uuid =
        sqlx::query_scalar("select id from outcomes where market_id = $1 and idx = 0")
            .bind(market.0)
            .fetch_one(db.store.pool_handle())
            .await
            .unwrap();
    sqlx::query(
        "insert into positions (user_id, outcome_id, shares_micro, cost_micro, realized_pnl_micro)
         values ($1, $2, 7, 11, -3)",
    )
    .bind(user.0)
    .bind(yes_outcome)
    .execute(db.store.pool_handle())
    .await
    .unwrap();
    let mut tx = db.store.unwind_tx().await.unwrap();
    let facts = tx.market_ledger_txns(market).await.unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].txn, seed_txn);
    assert_eq!(facts[0].kind, domain::ledger::TxnKind::Seed);
    assert_eq!(facts[0].entries.len(), 2);
    let positions = tx.positions_for_market(market).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].shares, domain::money::MicroShares(7));
    let reversed_entry = Uuid::new_v4();
    let reversal_txn = Uuid::new_v4();
    tx.record_reversal(reversed_entry, reversal_txn, market)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut duplicate = db.store.unwind_tx().await.unwrap();
    assert_eq!(
        duplicate
            .record_reversal(reversed_entry, Uuid::new_v4(), market)
            .await
            .unwrap_err(),
        StoreError::Conflict("reversal lineage")
    );
    drop(duplicate);

    let mut bad_foreign_key = db.store.unwind_tx().await.unwrap();
    assert!(matches!(
        bad_foreign_key
            .record_reversal(Uuid::new_v4(), Uuid::new_v4(), MarketId(Uuid::new_v4()))
            .await,
        Err(StoreError::Integrity(_))
    ));
    drop(bad_foreign_key);

    let (receivable, origin_reversal) = db.open_receivable(user, market, 9_000_000).await;
    let mut tx = db.store.unwind_tx().await.unwrap();
    assert_eq!(tx.open_receivables_total().await.unwrap(), 9_000_000);
    let outstanding = tx.receivable_by_id(receivable).await.unwrap().unwrap();
    assert_eq!(outstanding.outstanding_micro, 9_000_000);
    let duplicate_receivable = Receivable {
        id: Uuid::new_v4(),
        market,
        user,
        origin_reversal_txn: origin_reversal,
        opened_micro: 1,
    };
    assert_eq!(
        tx.insert_receivable(duplicate_receivable)
            .await
            .unwrap_err(),
        StoreError::Conflict("receivable")
    );
    drop(tx);

    let mut bad_foreign_key = db.store.unwind_tx().await.unwrap();
    assert!(matches!(
        bad_foreign_key
            .insert_receivable(Receivable {
                id: Uuid::new_v4(),
                market: MarketId(Uuid::new_v4()),
                user,
                origin_reversal_txn: Uuid::new_v4(),
                opened_micro: 1,
            })
            .await,
        Err(StoreError::Integrity(_))
    ));
    drop(bad_foreign_key);

    let write_off = application::ports::WriteOffProposal {
        id: Uuid::new_v4(),
        receivable,
        idempotency_key: "ops-port-write-off".into(),
        amount_micro: 1_000_000,
        proposer_token_id: "finance-a".into(),
        confirmer_token_id: None,
        reason: "uncollectable".into(),
        status: ProposalStatus::Pending,
        confirm_not_before: now,
    };
    let mut tx = db.store.unwind_tx().await.unwrap();
    assert!(tx
        .write_off_for_update(write_off.id)
        .await
        .unwrap()
        .is_none());
    tx.insert_write_off(write_off.clone()).await.unwrap();
    assert_eq!(
        tx.write_off_for_update(write_off.id).await.unwrap(),
        Some(write_off.clone())
    );
    assert_eq!(
        tx.write_off_by_key(&write_off.idempotency_key)
            .await
            .unwrap(),
        Some(write_off.clone())
    );
    tx.commit().await.unwrap();
    let mut duplicate = db.store.unwind_tx().await.unwrap();
    assert_eq!(
        duplicate
            .insert_write_off(write_off.clone())
            .await
            .unwrap_err(),
        StoreError::Conflict("write-off proposal")
    );
    drop(duplicate);
    let mut bad_foreign_key = db.store.unwind_tx().await.unwrap();
    let mut invalid_write_off = write_off.clone();
    invalid_write_off.id = Uuid::new_v4();
    invalid_write_off.receivable = Uuid::new_v4();
    invalid_write_off.idempotency_key = "ops-port-write-off-bad-fk".into();
    assert!(matches!(
        bad_foreign_key.insert_write_off(invalid_write_off).await,
        Err(StoreError::Integrity(_))
    ));
    drop(bad_foreign_key);
    let mut tx = db.store.unwind_tx().await.unwrap();
    let mut confirmed_write_off = write_off.clone();
    confirmed_write_off.status = ProposalStatus::Confirmed;
    confirmed_write_off.confirmer_token_id = Some("superadmin-b".into());
    tx.save_write_off(&confirmed_write_off).await.unwrap();
    tx.insert_receivable_movement(ReceivableMovement {
        id: Uuid::new_v4(),
        receivable,
        kind: ReceivableMovementKind::WrittenOff,
        amount_micro: 1_000_000,
        actor: "superadmin-b".into(),
        cash_txn: None,
        idempotency_key: "ops-port-written-off".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        tx.written_off_since(OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap(),
        1_000_000
    );
    tx.commit().await.unwrap();
    let mut missing = db.store.unwind_tx().await.unwrap();
    let mut absent_write_off = write_off.clone();
    absent_write_off.id = Uuid::new_v4();
    assert_eq!(
        missing.save_write_off(&absent_write_off).await.unwrap_err(),
        StoreError::Invariant("save for unknown write-off")
    );
    drop(missing);

    let remedial = RemedialCreditProposal {
        id: Uuid::new_v4(),
        market,
        user,
        idempotency_key: "ops-port-remedial".into(),
        amount_micro: 2_000_000,
        proposer_token_id: "finance-a".into(),
        confirmer_token_id: None,
        reason: "goodwill".into(),
        status: ProposalStatus::Pending,
        confirm_not_before: now,
    };
    let mut tx = db.store.unwind_tx().await.unwrap();
    assert!(tx.remedial_for_update(remedial.id).await.unwrap().is_none());
    tx.insert_remedial(remedial.clone()).await.unwrap();
    assert_eq!(
        tx.remedial_for_update(remedial.id).await.unwrap(),
        Some(remedial.clone())
    );
    tx.commit().await.unwrap();
    let mut tx = db.store.unwind_tx().await.unwrap();
    let mut confirmed_remedial = remedial.clone();
    confirmed_remedial.status = ProposalStatus::Confirmed;
    confirmed_remedial.confirmer_token_id = Some("superadmin-b".into());
    tx.save_remedial(&confirmed_remedial).await.unwrap();
    assert_eq!(
        tx.remedial_credited_since(OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap(),
        2_000_000
    );
    tx.commit().await.unwrap();
    let mut bad_foreign_key = db.store.unwind_tx().await.unwrap();
    let mut invalid_remedial = remedial.clone();
    invalid_remedial.id = Uuid::new_v4();
    invalid_remedial.idempotency_key = "ops-port-remedial-bad-fk".into();
    invalid_remedial.market = MarketId(Uuid::new_v4());
    let foreign_key_error = bad_foreign_key
        .insert_remedial(invalid_remedial)
        .await
        .unwrap_err();
    assert!(matches!(foreign_key_error, StoreError::Integrity(_)));
    drop(bad_foreign_key);

    let command = OpsJobCommand {
        id: Uuid::new_v4(),
        kind: "replay_job".into(),
        subject: format!("market:{}", market.0),
        idempotency_key: "ops-port-job".into(),
        requested_by: "ops-a".into(),
        status: OpsJobStatus::Pending,
        attempts: 0,
        lease_expires_at: None,
        error: None,
    };
    let mut tx = db.store.unwind_tx().await.unwrap();
    assert!(tx
        .job_command_by_key("missing-job")
        .await
        .unwrap()
        .is_none());
    tx.insert_job_command(command.clone()).await.unwrap();
    assert_eq!(
        tx.job_command_by_key(&command.idempotency_key)
            .await
            .unwrap(),
        Some(command.clone())
    );
    tx.commit().await.unwrap();
    let mut duplicate = db.store.unwind_tx().await.unwrap();
    assert_eq!(
        duplicate
            .insert_job_command(command.clone())
            .await
            .unwrap_err(),
        StoreError::Conflict("ops job command")
    );
    drop(duplicate);
    let mut bad_check = db.store.unwind_tx().await.unwrap();
    let mut invalid_job = command.clone();
    invalid_job.id = Uuid::new_v4();
    invalid_job.idempotency_key = "ops-port-job-invalid-kind".into();
    invalid_job.kind = "invalid-kind".into();
    assert!(matches!(
        bad_check.insert_job_command(invalid_job).await,
        Err(StoreError::Integrity(_))
    ));
    drop(bad_check);
    let mut tx = db.store.unwind_tx().await.unwrap();
    let mut done = command.clone();
    done.status = OpsJobStatus::Done;
    done.attempts = 1;
    tx.save_job_command(&done).await.unwrap();
    tx.commit().await.unwrap();
    let mut missing = db.store.unwind_tx().await.unwrap();
    let mut absent_job = command;
    absent_job.id = Uuid::new_v4();
    assert_eq!(
        missing.save_job_command(&absent_job).await.unwrap_err(),
        StoreError::Invariant("save for unknown job command")
    );
    drop(missing);
    db.cleanup().await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pg_publication_commands_enforce_one_open_per_draft() {
    let db = TestDb::new("pubcmd").await;
    let draft_id = Uuid::new_v4();
    sqlx::query(
        "insert into market_drafts (id, tier, source, status, question, description, video_script,
                                    slug, seed_micro, fee_bps, open_secs, hidden_window_secs,
                                    min_votes_to_resolve, expires_at)
         values ($1, 'flash', 'template', 'approved', 'q?', 'd', 'v', 'pubcmd-slug',
                 100000000, 100, 3600, 300, 3, now() + interval '1 hour')",
    )
    .bind(draft_id)
    .execute(db.store.pool_handle())
    .await
    .unwrap();
    let draft = application::model::DraftId(draft_id);
    let mut tx = db.store.content_tx().await.unwrap();
    tx.lock_admission().await.unwrap();
    tx.lock_publication(draft).await.unwrap();
    tx.lock_publication_queue().await.unwrap();
    assert!(tx
        .publication_command_by_draft(draft)
        .await
        .unwrap()
        .is_none());
    assert!(tx
        .publication_command_by_key("missing-publication-command")
        .await
        .unwrap()
        .is_none());
    let command = PublicationCommand {
        id: Uuid::new_v4(),
        draft,
        idempotency_key: "pn-pg-1".into(),
        requested_by: "digest-curator".into(),
        status: PublicationCommandStatus::Pending,
        attempts: 0,
        lease_expires_at: None,
        result_market: None,
        error: None,
    };
    tx.insert_publication_command(command.clone())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut lookup = db.store.content_tx().await.unwrap();
    assert_eq!(
        lookup.publication_command_by_draft(draft).await.unwrap(),
        Some(command.clone())
    );
    assert_eq!(
        lookup.publication_command_by_key("pn-pg-1").await.unwrap(),
        Some(command.clone())
    );
    drop(lookup);
    // Second OPEN command for the same draft: unique partial index.
    let mut tx = db.store.content_tx().await.unwrap();
    let second = PublicationCommand {
        id: Uuid::new_v4(),
        idempotency_key: "pn-pg-2".into(),
        ..command.clone()
    };
    let refused = tx.insert_publication_command(second).await.unwrap_err();
    assert_eq!(refused, StoreError::Conflict("publication command"));
    drop(tx);
    // Claim + settle; a settled command frees the draft slot.
    let mut tx = db.store.content_tx().await.unwrap();
    let due = tx
        .due_publication_commands(OffsetDateTime::now_utc(), 10)
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    let mut settled = due[0].clone();
    settled.status = PublicationCommandStatus::Done;
    settled.result_market = None;
    tx.save_publication_command(&settled).await.unwrap();
    tx.commit().await.unwrap();
    let latest = db
        .store
        .publication_command_for_draft(draft)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.status, PublicationCommandStatus::Done);

    let missing_draft = application::model::DraftId(Uuid::new_v4());
    let mut broken = db.store.content_tx().await.unwrap();
    let foreign_key = broken
        .insert_publication_command(PublicationCommand {
            id: Uuid::new_v4(),
            draft: missing_draft,
            idempotency_key: "pn-bad-draft".into(),
            ..command.clone()
        })
        .await
        .unwrap_err();
    assert!(matches!(foreign_key, StoreError::Integrity(_)));
    drop(broken);

    let second_draft_id = Uuid::new_v4();
    sqlx::query(
        "insert into market_drafts (id, tier, source, status, question, description, video_script,
                                    slug, seed_micro, fee_bps, open_secs, hidden_window_secs,
                                    min_votes_to_resolve, expires_at)
         values ($1, 'flash', 'template', 'approved', 'q?', 'd', 'v', $2,
                 100000000, 100, 3600, 300, 3, now() + interval '1 hour')",
    )
    .bind(second_draft_id)
    .bind(format!("pubcmd-second-{second_draft_id}"))
    .execute(db.store.pool_handle())
    .await
    .unwrap();
    let mut duplicate_key = db.store.content_tx().await.unwrap();
    let refused = duplicate_key
        .insert_publication_command(PublicationCommand {
            id: Uuid::new_v4(),
            draft: application::model::DraftId(second_draft_id),
            idempotency_key: command.idempotency_key.clone(),
            ..command
        })
        .await
        .unwrap_err();
    assert_eq!(refused, StoreError::Conflict("publication command key"));
    drop(duplicate_key);
    db.cleanup().await;
}

#[tokio::test]
async fn pg_invariant_snapshot_is_read_only_and_detects_a_planted_mismatch() {
    let db = TestDb::new("invariants").await;
    // Fresh migrated DB passes the whole suite.
    let report = application::integrity::invariant_sweep::run(&db.store)
        .await
        .unwrap();
    assert!(report.pass, "{:?}", report.identities);
    EnsureGenesis { store: &db.store }
        .execute(EnsureGenesisCmd {
            currency: domain::ledger::Currency::Usdc,
            amount: MicroUsd(10_000_000),
        })
        .await
        .unwrap();
    let account: Uuid = sqlx::query_scalar("select id from ledger_accounts limit 1")
        .fetch_one(db.store.pool_handle())
        .await
        .unwrap();
    let rogue_txn = Uuid::new_v4();
    let mut connection = db.store.pool_handle().acquire().await.unwrap();
    sqlx::query("set session_replication_role = replica")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query(
        "insert into ledger_transactions (id, kind, idempotency_key)
         values ($1, 'deposit', $2)",
    )
    .bind(rogue_txn)
    .bind(format!("rogue-{rogue_txn}"))
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query("insert into ledger_entries (txn_id, account_id, amount_micro) values ($1, $2, 7)")
        .bind(rogue_txn)
        .bind(account)
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("set session_replication_role = origin")
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    let mut snapshot = db.store.invariant_read_tx().await.unwrap();
    assert!(snapshot
        .unbalanced_txns()
        .await
        .unwrap()
        .iter()
        .any(|row| row.txn == rogue_txn && row.sum_micro == 7));
    drop(snapshot);
    // A receivable with NO matching house shortfall leg breaks identity 7.
    let user = db.user("phantom").await;
    let market = db.market("phantom-market").await;
    sqlx::query(
        "insert into receivables (id, market_id, user_id, origin_reversal_txn_id, opened_micro)
         values ($1, $2, $3, $4, 1000000)",
    )
    .bind(Uuid::new_v4())
    .bind(market.0)
    .bind(user.0)
    .bind(Uuid::new_v4())
    .execute(db.store.pool_handle())
    .await
    .unwrap();
    let report = application::integrity::invariant_sweep::run(&db.store)
        .await
        .unwrap();
    assert!(!report.pass, "planted mismatch must be detected");
    let identity7 = report
        .identities
        .iter()
        .find(|identity| identity.identity == "receivables_reconcile")
        .unwrap();
    assert!(!identity7.pass);
    // The snapshot is READ ONLY: writes through it are impossible by
    // construction (no commit surface); the transaction closes on drop.
    let mut snapshot = db.store.invariant_read_tx().await.unwrap();
    let _ = snapshot.as_of().await.unwrap();
    drop(snapshot);
    db.cleanup().await;
}

#[tokio::test]
async fn pg_user_overrides_ride_the_bootstrap_transaction() {
    let db = TestDb::new("overrides").await;
    let backdate = OffsetDateTime::now_utc() - time::Duration::days(80);
    let user = application::create_user::CreateUser { store: &db.store }
        .execute_as(
            application::create_user::CreateUserCmd {
                handle: "aged".into(),
                channel: None,
                idempotency_key: "aged-1".into(),
                created_at_override: Some(backdate),
                rep_seed_micro: Some(400_000),
            },
            &superadmin(),
            application::model::RepConfig::default(),
        )
        .await
        .unwrap();
    let (created_at, rep, tier): (OffsetDateTime, i64, i32) = sqlx::query_as(
        "select u.created_at, r.rep_micro, r.tier
           from users u join reputation r on r.user_id = u.id where u.id = $1",
    )
    .bind(user.0)
    .fetch_one(db.store.pool_handle())
    .await
    .unwrap();
    assert!((created_at - backdate).abs() < time::Duration::seconds(1));
    assert_eq!(rep, 400_000);
    assert_eq!(tier, 2, "400k micro is tier 2 (wash-pair inequality)");
    // The override was audited.
    let page = db.store.audit_page(None, 10).await.unwrap();
    assert!(page
        .iter()
        .any(|row| row.action.action == "create_user_override"));
    db.cleanup().await;
}
