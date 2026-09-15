/// Creates a bare market row through `MarketWriter` (in `Draft`) and returns
/// its id — the FK anchor for the vote-writer suite.
async fn insert_bare_market<S: Store + ?Sized>(store: &S) -> MarketId {
    let mut tx = store.seed_tx().await.unwrap();
    let key = unique_key("contract-market");
    tx.serialize_key(&key).await.unwrap();
    let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let market = tx
        .insert_market(NewMarket {
            id: MarketId(Uuid::new_v4()),
            slug: format!("contract-{key}"),
            min_votes_to_resolve: 3,
            closes_at: now + time::Duration::hours(2),
            tally_hidden_at: now + time::Duration::hours(1),
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    market
}

async fn insert_contract_user<S: Store + ?Sized>(store: &S) -> UserId {
    let mut tx = store.bootstrap_tx().await.unwrap();
    let key = unique_key("contract-user");
    tx.serialize_key(&key).await.unwrap();
    let user = tx.insert_user(&key).await.unwrap();
    tx.commit().await.unwrap();
    user
}

/// `VoteWriter`: strictly increasing seq, read-your-writes by key, committed
/// visibility, and the one-vote-per-(user, market) conflict.
pub async fn vote_writer_contract<S: Store + ?Sized>(store: &S) {
    let market = insert_bare_market(store).await;
    let user = insert_contract_user(store).await;
    let key = unique_key("contract-vote");
    let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();

    let mut tx = store.vote_tx().await.unwrap();
    tx.serialize_key(&key).await.unwrap();
    let seq = tx.allocate_vote_seq(market).await.unwrap();
    let vote_id = tx
        .insert_vote(NewVote {
            market,
            user,
            side: Side::Yes,
            crowd_guess_pct: 55,
            seq,
            idempotency_key: key.clone(),
            created_at: now,
            cast_ip: None,
            device_hash: None,
        })
        .await
        .unwrap();
    let seen = tx.vote_by_key(&key, now).await.unwrap().unwrap();
    assert_eq!(seen.vote_id, vote_id, "read-your-writes by key");
    assert_eq!(seen.seq, Some(seq));
    tx.commit().await.unwrap();

    let mut tx2 = store.vote_tx().await.unwrap();
    tx2.serialize_key(&unique_key("contract-vote2"))
        .await
        .unwrap();
    assert_eq!(
        tx2.vote_by_key(&key, now).await.unwrap().unwrap().vote_id,
        vote_id,
        "committed vote visible by key"
    );
    let next_seq = tx2.allocate_vote_seq(market).await.unwrap();
    assert!(
        next_seq > seq,
        "seq strictly increases ({next_seq} > {seq})"
    );
    let dup = tx2
        .insert_vote(NewVote {
            market,
            user,
            side: Side::No,
            crowd_guess_pct: 40,
            seq: next_seq,
            idempotency_key: unique_key("contract-vote-dup"),
            created_at: now,
            cast_ip: None,
            device_hash: None,
        })
        .await;
    assert!(
        matches!(dup, Err(StoreError::Conflict(_))),
        "second vote by the same user must conflict, got {dup:?}"
    );
}

/// `DepositWriter`: signature dedupe read-your-writes, committed visibility,
/// and the duplicate-signature conflict.
pub async fn deposit_writer_contract<S: Store + ?Sized>(store: &S) {
    let user = insert_contract_user(store).await;
    let sig = unique_key("contract-sig");
    let key = unique_key("contract-dep");

    let mut tx = store.deposit_tx().await.unwrap();
    tx.serialize_key(&key).await.unwrap();
    assert_eq!(tx.deposit_by_sig(&sig).await.unwrap(), None);
    let ext = tx
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let acct = tx
        .account(OwnerRef::User(user), Currency::Usdc)
        .await
        .unwrap();
    let txn = tx
        .ledger_apply(
            TxnKind::Deposit,
            &key,
            &[
                Entry {
                    account: ext,
                    amount: MicroUsd(-9),
                },
                Entry {
                    account: acct,
                    amount: MicroUsd(9),
                },
            ],
        )
        .await
        .unwrap();
    let deposit = tx
        .insert_deposit(NewDeposit {
            user,
            amount: MicroUsd(9),
            chain_sig: sig.clone(),
            ledger_txn: txn,
        })
        .await
        .unwrap();
    assert_eq!(
        tx.deposit_by_sig(&sig).await.unwrap(),
        Some(deposit),
        "read-your-writes by signature"
    );
    tx.commit().await.unwrap();

    let mut tx2 = store.deposit_tx().await.unwrap();
    tx2.serialize_key(&unique_key("contract-dep2"))
        .await
        .unwrap();
    assert_eq!(
        tx2.deposit_by_sig(&sig).await.unwrap(),
        Some(deposit),
        "committed deposit visible by signature"
    );
    let dup = tx2
        .insert_deposit(NewDeposit {
            user,
            amount: MicroUsd(9),
            chain_sig: sig,
            ledger_txn: txn,
        })
        .await;
    assert!(
        matches!(dup, Err(StoreError::Conflict(_))),
        "duplicate chain signature must conflict, got {dup:?}"
    );
}

/// Seed → settlement wiring: after a real `EnsureGenesis` + `SeedMarket`,
/// `SettlementIo` must present the pool inventory as holdings (S per side on
/// the pool's own account) against an escrow balance of exactly S.
pub async fn seeded_settlement_io_contract<S: Store + MarketQueries + ?Sized>(store: &S) {
    EnsureGenesis { store }
        .execute(EnsureGenesisCmd {
            currency: Currency::Usdc,
            amount: MicroUsd(2_000_000),
        })
        .await
        .unwrap();
    let _ = genesis_key(Currency::Usdc); // suite exercises the deterministic-key path above
    let market = MarketId(Uuid::new_v4());
    let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let clock = crate::fakes::FakeClock::at(now);
    SeedMarket {
        store,
        clock: &clock,
        rep_config: crate::model::RepConfig::default(),
        lp_kill_config: crate::model::LpKillConfig::default(),
    }
    .execute(SeedMarketCmd {
        market_id: market,
        slug: format!("contract-settle-{}", market.0),
        min_votes_to_resolve: 3,
        closes_at: now + time::Duration::hours(2),
        tally_hidden_at: now + time::Duration::hours(1),
        fee: BasisPoints(100),
        seed: MicroUsd(1_000_000),
        idempotency_key: unique_key("contract-seed"),
        force: false,
    })
    .await
    .unwrap();

    let mut tx = store.resolve_tx().await.unwrap();
    tx.serialize_key(&unique_key("contract-settle"))
        .await
        .unwrap();
    assert_eq!(
        tx.escrow_balance(market).await.unwrap(),
        MicroUsd(1_000_000),
        "escrow holds exactly S"
    );
    let holdings = tx.holdings(market).await.unwrap();
    let yes: i64 = holdings
        .iter()
        .filter(|holding| matches!(holding.side, Side::Yes))
        .map(|holding| holding.shares.0)
        .sum();
    let no: i64 = holdings
        .iter()
        .filter(|holding| matches!(holding.side, Side::No))
        .map(|holding| holding.shares.0)
        .sum();
    assert_eq!(
        (yes, no),
        (1_000_000, 1_000_000),
        "holdings present the full pool inventory per side"
    );
    assert_eq!(
        tx.open_interest(market).await.unwrap(),
        MicroUsd(0),
        "no user positions yet"
    );
    assert_eq!(tx.integrity_report(market).await.unwrap(), None);
    let due_at = now + time::Duration::minutes(5);
    tx.set_integrity_due_at(market, Some(due_at)).await.unwrap();
    assert_eq!(
        holdings.len(),
        2,
        "pool inventory rides the pool's own account, one holding per side"
    );
    assert_eq!(MicroShares(yes), MicroShares(1_000_000));
    tx.commit().await.unwrap();
    assert_eq!(
        store
            .market_by_ref(&market.0.to_string())
            .await
            .unwrap()
            .integrity_due_at,
        Some(due_at)
    );
    let mut clear_due = store.resolve_tx().await.unwrap();
    clear_due
        .serialize_key(&unique_key("contract-clear-due"))
        .await
        .unwrap();
    clear_due.set_integrity_due_at(market, None).await.unwrap();
    clear_due.commit().await.unwrap();
    assert_eq!(
        store
            .market_by_ref(&market.0.to_string())
            .await
            .unwrap()
            .integrity_due_at,
        None
    );
}

async fn set_resolving<S: Store + ?Sized>(store: &S, market: MarketId) {
    let mut tx = store.resolve_tx().await.unwrap();
    tx.serialize_key(&unique_key("integrity-resolving"))
        .await
        .unwrap();
    tx.market_for_update(market).await.unwrap();
    SettlementIo::set_market_state(tx.as_mut(), market, domain::market::MarketState::Resolving)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn insert_contract_report<S: Store + ?Sized>(
    store: &S,
    market: MarketId,
    verdict: domain::integrity::Verdict,
    created_at: time::OffsetDateTime,
) {
    let mut tx = store.integrity_tx().await.unwrap();
    tx.serialize_key(&unique_key("integrity-extra-report"))
        .await
        .unwrap();
    assert!(tx
        .insert_integrity_report(&IntegrityReportRow {
            market,
            checks: serde_json::json!([{"name":"contract","flagged": verdict == domain::integrity::Verdict::Flag}]),
            verdict,
            created_at,
        })
        .await
        .unwrap());
    tx.commit().await.unwrap();
}

/// Integrity report transactions aggregate metadata, expose their own
/// insert, and converge on one immutable report across retries.
pub async fn integrity_report_contract<S: Store + MarketQueries + ?Sized>(store: &S) {
    let market = insert_bare_market(store).await;
    let user = insert_contract_user(store).await;
    let cast_at = time::OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
    let vote_key = unique_key("integrity-vote");
    let mut vote_tx = store.vote_tx().await.unwrap();
    vote_tx.serialize_key(&vote_key).await.unwrap();
    let seq = vote_tx.allocate_vote_seq(market).await.unwrap();
    vote_tx
        .insert_vote(NewVote {
            market,
            user,
            side: Side::Yes,
            crowd_guess_pct: 50,
            seq,
            idempotency_key: vote_key,
            created_at: cast_at,
            cast_ip: Some("2001:db8::1".parse().unwrap()),
            device_hash: Some("hashed-device".to_string()),
        })
        .await
        .unwrap();
    vote_tx.commit().await.unwrap();

    let key = unique_key("integrity-report");
    let mut tx = store.integrity_tx().await.unwrap();
    tx.serialize_key(&key).await.unwrap();
    assert_eq!(tx.market_for_update(market).await.unwrap().id, market);
    let stats = tx
        .vote_stats(market, IntegritySweepConfig::default())
        .await
        .unwrap();
    assert_eq!(stats.total, 1);
    assert_eq!(stats.subnet_observed, 1);
    assert_eq!(stats.top_subnet_count, 1);
    assert_eq!(stats.device_observed, 1);
    assert_eq!(stats.top_device_count, 1);
    let report = IntegrityReportRow {
        market,
        checks: serde_json::json!([{"name":"contract","flagged":false}]),
        verdict: domain::integrity::Verdict::Pass,
        created_at: cast_at,
    };
    assert!(tx.insert_integrity_report(&report).await.unwrap());
    assert!(!tx.insert_integrity_report(&report).await.unwrap());
    assert_eq!(
        tx.integrity_report(market).await.unwrap(),
        Some(report.clone())
    );
    tx.commit().await.unwrap();

    let mut replay = store.integrity_tx().await.unwrap();
    replay.serialize_key(&key).await.unwrap();
    assert!(!replay.insert_integrity_report(&report).await.unwrap());
    assert_eq!(replay.integrity_report(market).await.unwrap(), Some(report));
    drop(replay);

    set_resolving(store, market).await;

    let flagged_market = insert_bare_market(store).await;
    insert_contract_report(
        store,
        flagged_market,
        domain::integrity::Verdict::Flag,
        cast_at,
    )
    .await;
    set_resolving(store, flagged_market).await;

    let no_report_market = insert_bare_market(store).await;
    set_resolving(store, no_report_market).await;

    let inbox = store.flagged_markets().await.unwrap();
    let pass = inbox.iter().find(|row| row.market.id == market).unwrap();
    assert_eq!(
        pass.report.as_ref().unwrap().verdict,
        domain::integrity::Verdict::Pass
    );
    let flag = inbox
        .iter()
        .find(|row| row.market.id == flagged_market)
        .unwrap();
    assert_eq!(
        flag.report.as_ref().unwrap().verdict,
        domain::integrity::Verdict::Flag
    );
    assert!(inbox
        .iter()
        .find(|row| row.market.id == no_report_market)
        .unwrap()
        .report
        .is_none());
}

