//! Inbound USDC watcher: depth is measured against finalized roots (D31/D32).
//! Signature idempotency is bound to (user, source address, mint, amount, slot).

use application::error::{AppError, StoreError};
use application::money::ObservedDeposit;
use application::ports::{RailIdentity, Store};
use domain::money::MicroUsd;

/// One finalized-depth inbound transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundTransfer {
    pub signature: String,
    pub slot: u64,
    pub amount_micro: i64,
    pub mint: String,
    pub source_address: String,
    pub dest_address: String,
    pub user: Option<application::model::UserId>,
}

/// Chain reads the watcher needs. Tests supply a recording fake.
#[async_trait::async_trait]
pub trait FinalizedChain: Send + Sync {
    async fn genesis_hash(&self) -> Result<String, StoreError>;
    async fn finalized_slot(&self) -> Result<u64, StoreError>;
    async fn inbound_since(&self, after_slot: u64) -> Result<Vec<InboundTransfer>, StoreError>;
}

/// Process inbound transfers whose depth vs the finalized root meets
/// `deposit_confirmations`.
pub struct InboundWatcher<'a, S: Store, C: FinalizedChain> {
    pub store: &'a S,
    pub chain: &'a C,
    pub rail: &'a RailIdentity,
    pub confirmations: u64,
}

impl<S: Store, C: FinalizedChain> InboundWatcher<'_, S, C> {
    /// # Errors
    /// Store / chain failures; wrong mint is skipped, not an error.
    pub async fn tick(&self) -> Result<u32, AppError> {
        if self.rail.validate().is_err() {
            return Err(StoreError::Invariant("invalid inbound rail identity").into());
        }
        if self.confirmations == 0 {
            return Err(StoreError::Invariant("deposit confirmations must be positive").into());
        }
        if self.chain.genesis_hash().await? != self.rail.genesis_hash {
            return Err(StoreError::Invariant(
                "inbound chain genesis does not match rail identity",
            )
            .into());
        }
        let finalized = self.chain.finalized_slot().await?;
        // Re-read from the durable chain source. Exact observation replay is
        // cheap and lets a prior crash after observation but before admission
        // heal on the next tick without an unsafe in-memory cursor.
        let inbound = self.chain.inbound_since(0).await?;
        let mut booked = 0_u32;
        for xfer in inbound {
            if xfer.mint != self.rail.usdc_mint {
                continue;
            }
            if xfer.dest_address != self.rail.treasury_token_account {
                continue;
            }
            if finalized_depth(finalized, xfer.slot).is_none_or(|depth| depth < self.confirmations)
            {
                continue;
            }
            let observed = ObservedDeposit {
                user: xfer.user,
                amount: MicroUsd(xfer.amount_micro),
                chain_sig: xfer.signature.clone(),
                source_address: xfer.source_address,
                dest_address: xfer.dest_address,
                mint: xfer.mint,
                slot: i64::try_from(xfer.slot)
                    .map_err(|_| StoreError::Invariant("deposit slot exceeds i64"))?,
            };
            let receipt = application::credit_deposit::observe_finalized_on_rail(
                self.store,
                &observed,
                &self.rail.fingerprint(),
            )
            .await?;
            let _ = application::credit_deposit::admit_observed(
                self.store,
                &observed.chain_sig,
                time::OffsetDateTime::now_utc(),
                &application::model::AdminContext::Machine,
                false,
            )
            .await?;
            booked += u32::from(!receipt.replayed);
        }
        Ok(booked)
    }
}

/// Inclusive finalized-root depth: the transfer's own finalized slot is
/// confirmation one. Future slots are not finalized observations.
#[must_use]
pub fn finalized_depth(finalized_root: u64, transfer_slot: u64) -> Option<u64> {
    finalized_root
        .checked_sub(transfer_slot)
        .and_then(|distance| distance.checked_add(1))
}

/// Bind the signature to the observation tuple (user/address/mint/amount/slot).
#[must_use]
pub fn observation_bind(
    user: Option<application::model::UserId>,
    source: &str,
    mint: &str,
    amount: i64,
    slot: u64,
) -> String {
    format!(
        "{}:{source}:{mint}:{amount}:{slot}",
        user.map(|u| u.0.to_string()).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use application::fakes::InMemoryStore;
    use application::model::{OwnerRef, UserId};
    use domain::ledger::Currency;
    use uuid::Uuid;

    struct FakeChain {
        genesis_hash: String,
        finalized: u64,
        inbound: std::sync::Mutex<Vec<InboundTransfer>>,
    }

    #[async_trait::async_trait]
    impl FinalizedChain for FakeChain {
        async fn genesis_hash(&self) -> Result<String, StoreError> {
            Ok(self.genesis_hash.clone())
        }

        async fn finalized_slot(&self) -> Result<u64, StoreError> {
            Ok(self.finalized)
        }

        async fn inbound_since(
            &self,
            _after_slot: u64,
        ) -> Result<Vec<InboundTransfer>, StoreError> {
            Ok(self.inbound.lock().unwrap().clone())
        }
    }

    fn rail() -> RailIdentity {
        RailIdentity {
            genesis_hash: "devnet".into(),
            rpc_endpoints: vec!["a".into(), "b".into(), "c".into()],
            usdc_mint: "usdc".into(),
            decimals: 6,
            treasury_owner: "treasury".into(),
            treasury_token_account: "treasury-usdc".into(),
            commitment: "finalized".into(),
        }
    }

    fn transfer(user: UserId) -> InboundTransfer {
        InboundTransfer {
            signature: "sig-1".into(),
            slot: 99,
            amount_micro: 5_000_000,
            mint: "usdc".into(),
            source_address: "source".into(),
            dest_address: "treasury-usdc".into(),
            user: Some(user),
        }
    }

    #[test]
    fn bind_includes_user_address_mint_amount_and_slot() {
        let user = UserId(Uuid::from_u128(1));
        let a = observation_bind(Some(user), "src", "mint", 5, 9);
        let b = observation_bind(Some(user), "src", "mint", 5, 10);
        let c = observation_bind(None, "src", "mint", 5, 9);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.contains("mint"));
    }

    #[test]
    fn depth_is_inclusive_and_future_slots_are_not_finalized() {
        assert_eq!(finalized_depth(100, 100), Some(1));
        assert_eq!(finalized_depth(100, 99), Some(2));
        assert_eq!(finalized_depth(100, 101), None);
    }

    #[tokio::test]
    async fn watcher_books_only_finalized_depth_and_exact_replay_is_a_noop() {
        let store = InMemoryStore::new();
        let user = UserId(Uuid::new_v4());
        let mut too_shallow = transfer(user);
        too_shallow.signature = "sig-shallow".into();
        too_shallow.slot = 100;
        let mut wrong_mint = transfer(user);
        wrong_mint.signature = "sig-wrong-mint".into();
        wrong_mint.mint = "not-usdc".into();
        let mut wrong_dest = transfer(user);
        wrong_dest.signature = "sig-wrong-dest".into();
        wrong_dest.dest_address = "different-treasury".into();
        let chain = FakeChain {
            genesis_hash: "devnet".into(),
            finalized: 100,
            inbound: std::sync::Mutex::new(vec![
                transfer(user),
                too_shallow,
                wrong_mint,
                wrong_dest,
            ]),
        };
        let rail = rail();
        let watcher = InboundWatcher {
            store: &store,
            chain: &chain,
            rail: &rail,
            confirmations: 2,
        };

        assert_eq!(watcher.tick().await.unwrap(), 1);
        assert_eq!(watcher.tick().await.unwrap(), 0);
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let row = inspect
            .deposit_machine_by_sig("sig-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.rail_fingerprint, rail.fingerprint());
        drop(inspect);
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(5_000_000))
        );
    }

    #[tokio::test]
    async fn signature_reuse_with_a_different_bound_tuple_conflicts() {
        let store = InMemoryStore::new();
        let user = UserId(Uuid::new_v4());
        let chain = FakeChain {
            genesis_hash: "devnet".into(),
            finalized: 100,
            inbound: std::sync::Mutex::new(vec![transfer(user)]),
        };
        let rail = rail();
        let watcher = InboundWatcher {
            store: &store,
            chain: &chain,
            rail: &rail,
            confirmations: 2,
        };
        assert_eq!(watcher.tick().await.unwrap(), 1);
        chain.inbound.lock().unwrap()[0].amount_micro += 1;
        assert!(matches!(
            watcher.tick().await,
            Err(AppError::Store(StoreError::Conflict(
                "deposit observation binding"
            )))
        ));
    }

    #[tokio::test]
    async fn watcher_rejects_a_finalized_root_from_the_wrong_cluster() {
        let store = InMemoryStore::new();
        let user = UserId(Uuid::new_v4());
        let chain = FakeChain {
            genesis_hash: "mainnet".into(),
            finalized: 100,
            inbound: std::sync::Mutex::new(vec![transfer(user)]),
        };
        let rail = rail();
        let watcher = InboundWatcher {
            store: &store,
            chain: &chain,
            rail: &rail,
            confirmations: 2,
        };
        assert!(matches!(
            watcher.tick().await,
            Err(AppError::Store(StoreError::Invariant(
                "inbound chain genesis does not match rail identity"
            )))
        ));
        assert_eq!(store.balance_of(OwnerRef::User(user), Currency::Usdc), None);
    }

    #[tokio::test]
    async fn watcher_rejects_invalid_rail_and_zero_confirmation_policy() {
        let store = InMemoryStore::new();
        let user = UserId(Uuid::new_v4());
        let chain = FakeChain {
            genesis_hash: "devnet".into(),
            finalized: 100,
            inbound: std::sync::Mutex::new(vec![transfer(user)]),
        };
        let mut invalid_rail = rail();
        invalid_rail.decimals = 5;
        assert!(matches!(
            InboundWatcher {
                store: &store,
                chain: &chain,
                rail: &invalid_rail,
                confirmations: 2,
            }
            .tick()
            .await,
            Err(AppError::Store(StoreError::Invariant(
                "invalid inbound rail identity"
            )))
        ));

        let valid_rail = rail();
        assert!(matches!(
            InboundWatcher {
                store: &store,
                chain: &chain,
                rail: &valid_rail,
                confirmations: 0,
            }
            .tick()
            .await,
            Err(AppError::Store(StoreError::Invariant(
                "deposit confirmations must be positive"
            )))
        ));
    }
}
