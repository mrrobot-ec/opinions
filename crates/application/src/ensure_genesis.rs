//! `EnsureGenesis` — idempotent External→House capitalization (codex B5):
//! the house cannot SEED a market from a zero balance. Key is deterministic
//! (`genesis:house:<currency>`), so re-running the seed script is safe. The
//! amount comes from the caller (`GENESIS_HOUSE_MICRO` env is read by
//! `seed.rs` in Task 1.5; demo default $10,000).

use domain::ledger::{Currency, Entry, TxnKind};
use domain::money::MicroUsd;
use serde_json::json;

use crate::error::AppError;
use crate::model::{Event, OwnerRef};
use crate::ports::Store;

#[derive(Debug, Clone, Copy)]
pub struct EnsureGenesisCmd {
    pub currency: Currency,
    pub amount: MicroUsd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenesisReceipt {
    pub ledger_txn: uuid::Uuid,
    pub replayed: bool,
}

/// The deterministic idempotency key for one currency's genesis.
#[must_use]
pub fn genesis_key(currency: Currency) -> String {
    let name = match currency {
        Currency::Usdc => "usdc",
        Currency::UsdcCredit => "usdc_credit",
    };
    format!("genesis:house:{name}")
}

pub struct EnsureGenesis<'a, S: Store + ?Sized> {
    pub store: &'a S,
}

impl<S: Store + ?Sized> EnsureGenesis<'_, S> {
    /// # Errors
    /// Store failures only; a replay returns the original transaction.
    pub async fn execute(&self, cmd: EnsureGenesisCmd) -> Result<GenesisReceipt, AppError> {
        let key = genesis_key(cmd.currency);
        let mut tx = self.store.bootstrap_tx().await?;
        tx.serialize_key(&key).await?;
        if let Some(txn) = tx.txn_by_key(&key).await? {
            return Ok(GenesisReceipt {
                ledger_txn: txn,
                replayed: true,
            });
        }
        let external = tx.account(OwnerRef::External, cmd.currency).await?;
        let house = tx.account(OwnerRef::House, cmd.currency).await?;
        let ledger_txn = tx
            .ledger_apply(
                TxnKind::Deposit,
                &key,
                &[
                    Entry {
                        account: external,
                        amount: MicroUsd(-cmd.amount.0),
                    },
                    Entry {
                        account: house,
                        amount: cmd.amount,
                    },
                ],
            )
            .await?;
        tx.append(Event {
            event_type: "GenesisEnsured",
            aggregate_type: "house",
            aggregate_id: house.0,
            payload: json!({
                "currency": format!("{:?}", cmd.currency),
                "amount_micro": cmd.amount.0,
            }),
        })
        .await?;
        tx.commit().await?;
        Ok(GenesisReceipt {
            ledger_txn,
            replayed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::InMemoryStore;

    #[tokio::test]
    async fn genesis_capitalizes_house_exactly_once() {
        let store = InMemoryStore::new();
        let uc = EnsureGenesis { store: &store };
        let cmd = EnsureGenesisCmd {
            currency: Currency::Usdc,
            amount: MicroUsd(10_000_000_000),
        };
        let first = uc.execute(cmd).await.unwrap();
        assert!(!first.replayed);
        assert_eq!(
            store.balance_of(OwnerRef::House, Currency::Usdc),
            Some(MicroUsd(10_000_000_000))
        );
        let snapshot = store.snapshot();
        let second = uc.execute(cmd).await.unwrap();
        assert!(second.replayed);
        assert_eq!(second.ledger_txn, first.ledger_txn);
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn concurrent_genesis_serializes_on_the_deterministic_key() {
        let store = InMemoryStore::new();
        let uc = EnsureGenesis { store: &store };
        let cmd = EnsureGenesisCmd {
            currency: Currency::Usdc,
            amount: MicroUsd(5_000),
        };
        let (a, b) = tokio::join!(uc.execute(cmd), uc.execute(cmd));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(a.replayed ^ b.replayed);
        assert_eq!(a.ledger_txn, b.ledger_txn);
        assert_eq!(
            store.balance_of(OwnerRef::House, Currency::Usdc),
            Some(MicroUsd(5_000))
        );
    }

    #[test]
    fn genesis_keys_are_currency_specific() {
        assert_eq!(genesis_key(Currency::Usdc), "genesis:house:usdc");
        assert_eq!(
            genesis_key(Currency::UsdcCredit),
            "genesis:house:usdc_credit"
        );
    }
}
