/// Within one transaction: get-or-create account stability, funding, and an
/// atomically blocked over-debit (the fake and Postgres must agree).
async fn assert_double_spend_blocked(lw: &mut (dyn LedgerWriter + '_)) {
    let user = UserId(Uuid::new_v4());
    let ext = lw
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let acct = lw
        .account(OwnerRef::User(user), Currency::Usdc)
        .await
        .unwrap();
    let again = lw
        .account(OwnerRef::User(user), Currency::Usdc)
        .await
        .unwrap();
    assert_eq!(acct, again, "account() must be get-or-create");
    let fees = lw.account(OwnerRef::Fees, Currency::Usdc).await.unwrap();

    lw.ledger_apply(
        TxnKind::Deposit,
        &unique_key("contract-fund"),
        &[
            Entry {
                account: ext,
                amount: MicroUsd(-10),
            },
            Entry {
                account: acct,
                amount: MicroUsd(10),
            },
        ],
    )
    .await
    .unwrap();
    lw.ledger_apply(
        TxnKind::Trade,
        &unique_key("contract-spend"),
        &[
            Entry {
                account: acct,
                amount: MicroUsd(-7),
            },
            Entry {
                account: fees,
                amount: MicroUsd(7),
            },
        ],
    )
    .await
    .unwrap();
    // Only 3 left: the second spend of 6 must be rejected as a whole...
    let over = lw
        .ledger_apply(
            TxnKind::Trade,
            &unique_key("contract-overspend"),
            &[
                Entry {
                    account: acct,
                    amount: MicroUsd(-6),
                },
                Entry {
                    account: fees,
                    amount: MicroUsd(6),
                },
            ],
        )
        .await;
    let was_insufficient = matches!(
        over,
        Err(StoreError::Ledger(LedgerError::InsufficientFunds { .. }))
    );
    assert!(
        was_insufficient,
        "over-debit must surface InsufficientFunds"
    );
    // ...and must not have partially settled: the remaining 3 still spends.
    lw.ledger_apply(
        TxnKind::Trade,
        &unique_key("contract-rest"),
        &[
            Entry {
                account: acct,
                amount: MicroUsd(-3),
            },
            Entry {
                account: fees,
                amount: MicroUsd(3),
            },
        ],
    )
    .await
    .unwrap();
}

/// The plan-shown ledger-writer suite: one tx, double spend blocked, commit;
/// then key visibility after commit and `DuplicateKey` as an invariant error.
pub async fn ledger_writer_contract<S: Store + ?Sized>(store: &S) {
    let key = unique_key("contract-ledger");
    let mut tx = store.trade_tx().await.unwrap();
    tx.serialize_key(&key).await.unwrap();
    assert_double_spend_blocked(tx.as_mut()).await;
    let user = UserId(Uuid::new_v4());
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
                    amount: MicroUsd(-5),
                },
                Entry {
                    account: acct,
                    amount: MicroUsd(5),
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        tx.txn_by_key(&key).await.unwrap(),
        Some(txn),
        "read-your-writes"
    );
    tx.commit().await.unwrap();

    let mut tx2 = store.trade_tx().await.unwrap();
    tx2.serialize_key(&key).await.unwrap();
    assert_eq!(
        tx2.txn_by_key(&key).await.unwrap(),
        Some(txn),
        "committed key must be visible to later transactions"
    );
    let dup = tx2
        .ledger_apply(
            TxnKind::Deposit,
            &key,
            &[
                Entry {
                    account: ext,
                    amount: MicroUsd(-5),
                },
                Entry {
                    account: acct,
                    amount: MicroUsd(5),
                },
            ],
        )
        .await;
    assert!(
        matches!(dup, Err(StoreError::DuplicateKey)),
        "reusing a key inside ledger_apply is an invariant violation, got {dup:?}"
    );
}

struct DriveOutcome {
    txn: Uuid,
    wrote: bool,
}

/// Guard-first deposit: `serialize_key` → `txn_by_key` (hit → replay, write
/// nothing) → miss → write → commit.
async fn drive_guarded_deposit(
    mut tx: Box<dyn TradeTx + '_>,
    user: UserId,
    key: &str,
) -> DriveOutcome {
    tx.serialize_key(key).await.unwrap();
    if let Some(txn) = tx.txn_by_key(key).await.unwrap() {
        // Loser: replay without writing; dropping the tx rolls it back.
        return DriveOutcome { txn, wrote: false };
    }
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
            key,
            &[
                Entry {
                    account: ext,
                    amount: MicroUsd(-5),
                },
                Entry {
                    account: acct,
                    amount: MicroUsd(5),
                },
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    DriveOutcome { txn, wrote: true }
}

/// Two parallel transactions from ONE store race the same idempotency key:
/// exactly one writes, both observe the same ledger transaction.
pub async fn concurrent_duplicate_key_contract<S: Store + ?Sized>(store: &S) {
    let key = unique_key("contract-dup");
    let user = UserId(Uuid::new_v4());
    let (a, b) = tokio::join!(store.trade_tx(), store.trade_tx()); // two parallel txs from ONE store
    let (ra, rb) = tokio::join!(
        drive_guarded_deposit(a.unwrap(), user, &key),
        drive_guarded_deposit(b.unwrap(), user, &key)
    );
    assert!(
        ra.wrote ^ rb.wrote,
        "exactly one of the two racing requests may write (wrote: {} / {})",
        ra.wrote,
        rb.wrote
    );
    assert_eq!(
        ra.txn, rb.txn,
        "both racers must observe the same ledger txn"
    );
}

async fn drive_debit(store: &(impl Store + ?Sized), user: UserId, key: &str) -> bool {
    let mut tx = store.trade_tx().await.unwrap();
    tx.serialize_key(key).await.unwrap();
    let acct = tx
        .account(OwnerRef::User(user), Currency::Usdc)
        .await
        .unwrap();
    let fees = tx.account(OwnerRef::Fees, Currency::Usdc).await.unwrap();
    let applied = tx
        .ledger_apply(
            TxnKind::Trade,
            key,
            &[
                Entry {
                    account: acct,
                    amount: MicroUsd(-7),
                },
                Entry {
                    account: fees,
                    amount: MicroUsd(7),
                },
            ],
        )
        .await;
    if applied.is_ok() {
        tx.commit().await.unwrap();
        true
    } else {
        let insufficient = matches!(
            applied,
            Err(StoreError::Ledger(LedgerError::InsufficientFunds { .. }))
        );
        assert!(
            insufficient,
            "double-spend must fail only for insufficient funds"
        );
        false
    }
}

/// Two concurrent transactions each try to debit 7 from a balance of 10
/// (distinct keys — this is the two-connection double-spend, codex B1):
/// exactly one debit commits.
pub async fn double_spend_two_tx_contract<S: Store + ?Sized>(store: &S) {
    let user = UserId(Uuid::new_v4());
    let mut fund = store.trade_tx().await.unwrap();
    let fund_key = unique_key("contract-ds-fund");
    fund.serialize_key(&fund_key).await.unwrap();
    let ext = fund
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let acct = fund
        .account(OwnerRef::User(user), Currency::Usdc)
        .await
        .unwrap();
    fund.ledger_apply(
        TxnKind::Deposit,
        &fund_key,
        &[
            Entry {
                account: ext,
                amount: MicroUsd(-10),
            },
            Entry {
                account: acct,
                amount: MicroUsd(10),
            },
        ],
    )
    .await
    .unwrap();
    fund.commit().await.unwrap();

    let (key_a, key_b) = (unique_key("contract-ds-a"), unique_key("contract-ds-b"));
    let (a, b) = tokio::join!(
        drive_debit(store, user, &key_a),
        drive_debit(store, user, &key_b)
    );
    assert!(
        a ^ b,
        "exactly one debit must commit (committed: {a} / {b})"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::InMemoryStore;

    #[tokio::test]
    async fn fake_passes_ledger_writer_contract() {
        ledger_writer_contract(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn fake_passes_concurrent_duplicate_key_contract() {
        concurrent_duplicate_key_contract(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn fake_passes_double_spend_two_tx_contract() {
        double_spend_two_tx_contract(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn fake_passes_vote_writer_contract() {
        vote_writer_contract(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn fake_passes_deposit_writer_contract() {
        deposit_writer_contract(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn fake_passes_seeded_settlement_io_contract() {
        seeded_settlement_io_contract(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn fake_passes_integrity_report_contract() {
        integrity_report_contract(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn fake_passes_comment_writer_contract() {
        let store = InMemoryStore::new();
        let now = time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(10);
        let market = store
            .add_market(
                "comment-contract",
                domain::market::MarketState::Live,
                now + time::Duration::days(1),
                now + time::Duration::hours(20),
                MicroShares(1_000_000),
                BasisPoints(100),
            )
            .unwrap()
            .id;
        let author = store.add_user("contract-author", now - time::Duration::days(2), 1);
        let voter = store.add_user("contract-voter", now - time::Duration::days(2), 1);
        comment_writer_contract(&store, market, author, voter, now).await;
    }

    #[tokio::test]
    async fn contracts_hold_through_every_store_factory() {
        // The six non-trade factories must hand out working transactions too:
        // commit an empty tx from each (Task 1.2 gives them their own suites).
        let store = InMemoryStore::new();
        store.vote_tx().await.unwrap().commit().await.unwrap();
        store.resolve_tx().await.unwrap().commit().await.unwrap();
        store.deposit_tx().await.unwrap().commit().await.unwrap();
        store.seed_tx().await.unwrap().commit().await.unwrap();
        store.advance_tx().await.unwrap().commit().await.unwrap();
        store.bootstrap_tx().await.unwrap().commit().await.unwrap();
        store.integrity_tx().await.unwrap().commit().await.unwrap();
        store.comment_tx().await.unwrap().commit().await.unwrap();
    }
}
