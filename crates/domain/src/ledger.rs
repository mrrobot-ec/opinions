use crate::money::MicroUsd;
use std::collections::HashMap;
use thiserror::Error;

/// Identity of one ledger account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountId(pub uuid::Uuid);

/// Who owns an account. `External` is the contra class representing the
/// outside world: exactly one External account exists per currency, and it is
/// the only class allowed to go negative (its negative balance equals net
/// system inflow of that currency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnerType {
    User,
    Pool,
    Fees,
    House,
    Escrow,
    External,
    /// Queued withdrawal cash (`User → Withheld`). Singleton per currency.
    Withheld,
    /// Observed but not yet admitted deposits. Singleton per currency.
    DepositSuspense,
    /// Pre-funded bonus-conversion reserve. Singleton per currency.
    BonusReserve,
}

/// Currency is a domain dimension, not just a DB column: cash and
/// non-withdrawable bonus credits never mix inside one transaction leg set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Currency {
    Usdc,
    UsdcCredit,
}

/// One signed, nonzero movement on an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub account: AccountId,
    pub amount: MicroUsd,
}

/// Business meaning of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnKind {
    Deposit,
    Trade,
    Payout,
    Withdrawal,
    Seed,
    Reversal,
    CreditGrant,
    CreditConvert,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("entries sum to {sum_micro} micro, not zero")]
    Unbalanced { sum_micro: i128 },
    #[error("entries for {currency:?} sum to {sum_micro} micro, not zero")]
    UnbalancedCurrency { currency: Currency, sum_micro: i128 },
    #[error("a transaction needs at least two entries")]
    TooFewEntries,
    #[error("zero-amount entries are not allowed")]
    ZeroEntry,
    #[error("insufficient funds on account {account:?}")]
    InsufficientFunds { account: AccountId },
    #[error("entry references an account that was never opened")]
    UnknownAccount,
    #[error("an External account already exists for {currency:?}")]
    DuplicateExternal { currency: Currency },
    #[error("a {owner:?} account already exists for {currency:?}")]
    DuplicateSingleton {
        owner: OwnerType,
        currency: Currency,
    },
    #[error("arithmetic overflow")]
    Overflow,
}

/// A structurally valid double-entry transaction: ≥ 2 entries, all nonzero,
/// overall sum exactly zero in `i128`. The stronger per-currency validation
/// lives in [`Balances::apply`], which knows each account's currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    kind: TxnKind,
    entries: Vec<Entry>,
}

impl Transaction {
    /// # Errors
    ///
    /// Returns [`LedgerError::TooFewEntries`] for fewer than two entries,
    /// [`LedgerError::ZeroEntry`] if any entry amount is zero, and
    /// [`LedgerError::Unbalanced`] if all entries do not sum to exactly zero.
    pub fn new(kind: TxnKind, entries: Vec<Entry>) -> Result<Self, LedgerError> {
        if entries.len() < 2 {
            return Err(LedgerError::TooFewEntries);
        }
        if entries.iter().any(|e| e.amount.0 == 0) {
            return Err(LedgerError::ZeroEntry);
        }
        let sum: i128 = entries.iter().map(|e| i128::from(e.amount.0)).sum();
        if sum != 0 {
            return Err(LedgerError::Unbalanced { sum_micro: sum });
        }
        Ok(Self { kind, entries })
    }

    #[must_use]
    pub fn kind(&self) -> TxnKind {
        self.kind
    }

    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

/// In-memory balance map for domain logic and tests.
#[derive(Debug, Default, Clone)]
pub struct Balances {
    accounts: HashMap<AccountId, (OwnerType, Currency, i64)>,
}

fn is_money_singleton(owner: OwnerType) -> bool {
    matches!(
        owner,
        OwnerType::Withheld | OwnerType::DepositSuspense | OwnerType::BonusReserve
    )
}

impl Balances {
    /// Opens an account with a zero balance; a no-op if `account` is already
    /// open. There is no genesis backdoor: every funding path is an ordinary
    /// balanced transaction against the External account of its currency.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError::DuplicateExternal`] when opening a second
    /// External account for the same currency, or
    /// [`LedgerError::DuplicateSingleton`] for a second `Withheld` /
    /// `DepositSuspense` / `BonusReserve` of the same currency.
    pub fn open(
        &mut self,
        account: AccountId,
        owner: OwnerType,
        currency: Currency,
    ) -> Result<(), LedgerError> {
        if self.accounts.contains_key(&account) {
            return Ok(());
        }
        if owner == OwnerType::External
            && self
                .accounts
                .values()
                .any(|(o, c, _)| *o == OwnerType::External && *c == currency)
        {
            return Err(LedgerError::DuplicateExternal { currency });
        }
        if is_money_singleton(owner)
            && self
                .accounts
                .values()
                .any(|(o, c, _)| *o == owner && *c == currency)
        {
            return Err(LedgerError::DuplicateSingleton { owner, currency });
        }
        self.accounts.insert(account, (owner, currency, 0));
        Ok(())
    }

    /// Applies a transaction atomically: on any error nothing is written.
    ///
    /// Pipeline: (1) aggregate entry amounts per account in checked `i128`
    /// (duplicate accounts in one transaction are legal but must be summed
    /// before validation); (2) validate that entries sum to exactly zero
    /// within every currency touched; (3) compute every post-balance with
    /// checked arithmetic, rejecting a negative result on any non-External
    /// account; (4) write.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError::UnknownAccount`] if an entry references an
    /// unopened account, [`LedgerError::UnbalancedCurrency`] if any touched
    /// currency does not sum to zero, [`LedgerError::InsufficientFunds`] if
    /// any internal account would go below zero, and
    /// [`LedgerError::Overflow`] if a post-balance cannot fit an `i64`.
    pub fn apply(&mut self, txn: &Transaction) -> Result<(), LedgerError> {
        let mut deltas: HashMap<AccountId, i128> = HashMap::new();
        for e in txn.entries() {
            let d = deltas.entry(e.account).or_insert(0);
            *d = d
                .checked_add(i128::from(e.amount.0))
                .ok_or(LedgerError::Overflow)?;
        }

        let mut per_currency: HashMap<Currency, i128> = HashMap::new();
        for (account, delta) in &deltas {
            let (_, currency, _) = self
                .accounts
                .get(account)
                .ok_or(LedgerError::UnknownAccount)?;
            let s = per_currency.entry(*currency).or_insert(0);
            *s = s.checked_add(*delta).ok_or(LedgerError::Overflow)?;
        }
        for (currency, sum_micro) in per_currency {
            if sum_micro != 0 {
                return Err(LedgerError::UnbalancedCurrency {
                    currency,
                    sum_micro,
                });
            }
        }

        let mut posts: Vec<(AccountId, i64)> = Vec::with_capacity(deltas.len());
        for (account, delta) in &deltas {
            let (owner, _, balance) = self
                .accounts
                .get(account)
                .ok_or(LedgerError::UnknownAccount)?;
            let post = i128::from(*balance)
                .checked_add(*delta)
                .ok_or(LedgerError::Overflow)?;
            let post = i64::try_from(post).map_err(|_| LedgerError::Overflow)?;
            if post < 0 && *owner != OwnerType::External {
                return Err(LedgerError::InsufficientFunds { account: *account });
            }
            posts.push((*account, post));
        }

        for (account, post) in posts {
            if let Some(slot) = self.accounts.get_mut(&account) {
                slot.2 = post;
            }
        }
        Ok(())
    }

    /// Current balance of an account, or `None` if it was never opened.
    #[must_use]
    pub fn balance(&self, account: AccountId) -> Option<MicroUsd> {
        self.accounts.get(&account).map(|(_, _, b)| MicroUsd(*b))
    }

    /// Sum over internal (non-External) accounts of `currency`. Invariant:
    /// `internal_total(c) == -balance(external(c))` for every currency, always
    /// — every accepted transaction is per-currency zero-sum, so the total
    /// always fits an `i64` (it mirrors one External account's balance).
    #[must_use]
    pub fn internal_total(&self, currency: Currency) -> MicroUsd {
        let sum: i128 = self
            .accounts
            .values()
            .filter(|(o, c, _)| *c == currency && *o != OwnerType::External)
            .map(|(_, _, b)| i128::from(*b))
            .sum();
        MicroUsd(i64::try_from(sum).unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::money::MicroUsd;
    use proptest::prelude::*;
    use uuid::Uuid;

    fn acct() -> AccountId {
        AccountId(Uuid::new_v4())
    }

    #[test]
    fn unbalanced_transaction_rejected() {
        let e = vec![
            Entry {
                account: acct(),
                amount: MicroUsd(-5),
            },
            Entry {
                account: acct(),
                amount: MicroUsd(4),
            },
        ];
        assert!(matches!(
            Transaction::new(TxnKind::Trade, e),
            Err(LedgerError::Unbalanced { sum_micro: -1 })
        ));
    }

    #[test]
    fn transaction_exposes_its_kind_and_entries() {
        let (from, to) = (acct(), acct());
        let entries = vec![
            Entry {
                account: from,
                amount: MicroUsd(-5),
            },
            Entry {
                account: to,
                amount: MicroUsd(5),
            },
        ];
        let txn = Transaction::new(TxnKind::Payout, entries.clone()).unwrap();

        assert_eq!(txn.kind(), TxnKind::Payout);
        assert_eq!(txn.entries(), entries);
    }

    #[test]
    fn apply_is_atomic_on_insufficient_funds() {
        let (a, b) = (acct(), acct());
        let mut bal = Balances::default();
        bal.open(a, OwnerType::User, Currency::Usdc).unwrap();
        bal.open(b, OwnerType::Fees, Currency::Usdc).unwrap();
        let txn = Transaction::new(
            TxnKind::Trade,
            vec![
                Entry {
                    account: a,
                    amount: MicroUsd(-10),
                },
                Entry {
                    account: b,
                    amount: MicroUsd(10),
                },
            ],
        )
        .unwrap();
        assert!(matches!(
            bal.apply(&txn),
            Err(LedgerError::InsufficientFunds { .. })
        ));
        assert_eq!(bal.balance(a).unwrap(), MicroUsd(0)); // nothing applied
        assert_eq!(bal.balance(b).unwrap(), MicroUsd(0));
    }

    #[test]
    fn duplicate_account_entries_are_aggregated_before_validation() {
        // R2/codex B1: A:-7, A:-6, B:+13 against A=10 must fail as a whole,
        // not partially settle from stale per-entry balances.
        let (ext, a, b) = (acct(), acct(), acct());
        let mut bal = Balances::default();
        bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
        bal.open(a, OwnerType::User, Currency::Usdc).unwrap();
        bal.open(b, OwnerType::User, Currency::Usdc).unwrap();
        bal.apply(
            &Transaction::new(
                TxnKind::Deposit,
                vec![
                    Entry {
                        account: ext,
                        amount: MicroUsd(-10),
                    },
                    Entry {
                        account: a,
                        amount: MicroUsd(10),
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
        let txn = Transaction::new(
            TxnKind::Trade,
            vec![
                Entry {
                    account: a,
                    amount: MicroUsd(-7),
                },
                Entry {
                    account: a,
                    amount: MicroUsd(-6),
                },
                Entry {
                    account: b,
                    amount: MicroUsd(13),
                },
            ],
        )
        .unwrap();
        assert!(matches!(
            bal.apply(&txn),
            Err(LedgerError::InsufficientFunds { .. })
        ));
        assert_eq!(bal.balance(a).unwrap(), MicroUsd(10)); // untouched
        assert_eq!(bal.balance(b).unwrap(), MicroUsd(0));
    }

    #[test]
    fn cross_currency_two_leg_transaction_rejected() {
        // Overall sum is zero but converts credit into cash — must be UnbalancedCurrency.
        let (uc, ucash) = (acct(), acct());
        let mut bal = Balances::default();
        bal.open(uc, OwnerType::User, Currency::UsdcCredit).unwrap();
        bal.open(ucash, OwnerType::User, Currency::Usdc).unwrap();
        let txn = Transaction::new(
            TxnKind::CreditConvert,
            vec![
                Entry {
                    account: uc,
                    amount: MicroUsd(-500),
                },
                Entry {
                    account: ucash,
                    amount: MicroUsd(500),
                },
            ],
        )
        .unwrap();
        assert!(matches!(
            bal.apply(&txn),
            Err(LedgerError::UnbalancedCurrency { .. })
        ));
    }

    #[test]
    fn deposit_from_external_is_representable() {
        let (ext, user) = (acct(), acct());
        let mut bal = Balances::default();
        bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
        bal.open(user, OwnerType::User, Currency::Usdc).unwrap();
        let txn = Transaction::new(
            TxnKind::Deposit,
            vec![
                Entry {
                    account: ext,
                    amount: MicroUsd(-1_000_000),
                },
                Entry {
                    account: user,
                    amount: MicroUsd(1_000_000),
                },
            ],
        )
        .unwrap();
        bal.apply(&txn).unwrap(); // external may go negative — this must NOT be InsufficientFunds
        assert_eq!(bal.balance(user).unwrap(), MicroUsd(1_000_000));
        assert_eq!(bal.balance(ext).unwrap(), MicroUsd(-1_000_000));
        assert_eq!(bal.internal_total(Currency::Usdc), MicroUsd(1_000_000));
    }

    #[test]
    fn four_leg_credit_conversion_is_legal() {
        // user_credit → external_credit (burn) + external_cash → user_cash (issue),
        // each currency internally balanced.
        let (ext_credit, ext_cash, user_credit, user_cash) = (acct(), acct(), acct(), acct());
        let mut bal = Balances::default();
        bal.open(ext_credit, OwnerType::External, Currency::UsdcCredit)
            .unwrap();
        bal.open(ext_cash, OwnerType::External, Currency::Usdc)
            .unwrap();
        bal.open(user_credit, OwnerType::User, Currency::UsdcCredit)
            .unwrap();
        bal.open(user_cash, OwnerType::User, Currency::Usdc)
            .unwrap();
        bal.apply(
            &Transaction::new(
                TxnKind::CreditGrant,
                vec![
                    Entry {
                        account: ext_credit,
                        amount: MicroUsd(-500),
                    },
                    Entry {
                        account: user_credit,
                        amount: MicroUsd(500),
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
        bal.apply(
            &Transaction::new(
                TxnKind::CreditConvert,
                vec![
                    Entry {
                        account: user_credit,
                        amount: MicroUsd(-500),
                    },
                    Entry {
                        account: ext_credit,
                        amount: MicroUsd(500),
                    },
                    Entry {
                        account: ext_cash,
                        amount: MicroUsd(-500),
                    },
                    Entry {
                        account: user_cash,
                        amount: MicroUsd(500),
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(bal.balance(user_credit).unwrap(), MicroUsd(0));
        assert_eq!(bal.balance(user_cash).unwrap(), MicroUsd(500));
        assert_eq!(bal.balance(ext_credit).unwrap(), MicroUsd(0));
        assert_eq!(bal.balance(ext_cash).unwrap(), MicroUsd(-500));
        assert_eq!(bal.internal_total(Currency::UsdcCredit), MicroUsd(0));
        assert_eq!(bal.internal_total(Currency::Usdc), MicroUsd(500));
    }

    #[test]
    fn second_external_for_same_currency_rejected() {
        let mut bal = Balances::default();
        bal.open(acct(), OwnerType::External, Currency::Usdc)
            .unwrap();
        assert!(matches!(
            bal.open(acct(), OwnerType::External, Currency::Usdc),
            Err(LedgerError::DuplicateExternal {
                currency: Currency::Usdc
            })
        ));
        // a different currency is fine, and re-opening an existing id is a no-op
        let ext_credit = acct();
        bal.open(ext_credit, OwnerType::External, Currency::UsdcCredit)
            .unwrap();
        bal.open(ext_credit, OwnerType::External, Currency::UsdcCredit)
            .unwrap();
    }

    #[test]
    fn money_singletons_are_one_per_currency_and_never_negative() {
        for owner in [
            OwnerType::Withheld,
            OwnerType::DepositSuspense,
            OwnerType::BonusReserve,
        ] {
            let mut bal = Balances::default();
            let first = acct();
            let ext = acct();
            bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
            bal.open(first, owner, Currency::Usdc).unwrap();
            assert!(matches!(
                bal.open(acct(), owner, Currency::Usdc),
                Err(LedgerError::DuplicateSingleton {
                    owner: got,
                    currency: Currency::Usdc
                }) if got == owner
            ));
            bal.open(first, owner, Currency::Usdc).unwrap();
            let overdraw = Transaction::new(
                TxnKind::Withdrawal,
                vec![
                    Entry {
                        account: first,
                        amount: MicroUsd(-1),
                    },
                    Entry {
                        account: ext,
                        amount: MicroUsd(1),
                    },
                ],
            )
            .unwrap();
            assert!(matches!(
                bal.apply(&overdraw),
                Err(LedgerError::InsufficientFunds { .. })
            ));
            let fund = Transaction::new(
                TxnKind::Deposit,
                vec![
                    Entry {
                        account: ext,
                        amount: MicroUsd(-10),
                    },
                    Entry {
                        account: first,
                        amount: MicroUsd(10),
                    },
                ],
            )
            .unwrap();
            bal.apply(&fund).unwrap();
            assert_eq!(bal.balance(first), Some(MicroUsd(10)));
        }
    }

    #[test]
    fn structural_error_arms() {
        // TooFewEntries
        assert!(matches!(
            Transaction::new(
                TxnKind::Trade,
                vec![Entry {
                    account: acct(),
                    amount: MicroUsd(5)
                }]
            ),
            Err(LedgerError::TooFewEntries)
        ));
        // ZeroEntry
        assert!(matches!(
            Transaction::new(
                TxnKind::Trade,
                vec![
                    Entry {
                        account: acct(),
                        amount: MicroUsd(0)
                    },
                    Entry {
                        account: acct(),
                        amount: MicroUsd(0)
                    }
                ]
            ),
            Err(LedgerError::ZeroEntry)
        ));
    }

    #[test]
    fn unknown_account_rejected() {
        let (known, unknown) = (acct(), acct());
        let mut bal = Balances::default();
        bal.open(known, OwnerType::User, Currency::Usdc).unwrap();
        let txn = Transaction::new(
            TxnKind::Trade,
            vec![
                Entry {
                    account: known,
                    amount: MicroUsd(-5),
                },
                Entry {
                    account: unknown,
                    amount: MicroUsd(5),
                },
            ],
        )
        .unwrap();
        assert!(matches!(bal.apply(&txn), Err(LedgerError::UnknownAccount)));
        assert_eq!(bal.balance(known).unwrap(), MicroUsd(0));
        assert_eq!(bal.balance(unknown), None);
    }

    #[test]
    fn overflow_on_account_at_i64_max() {
        let (ext, user) = (acct(), acct());
        let mut bal = Balances::default();
        bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
        bal.open(user, OwnerType::User, Currency::Usdc).unwrap();
        bal.apply(
            &Transaction::new(
                TxnKind::Deposit,
                vec![
                    Entry {
                        account: ext,
                        amount: MicroUsd(-i64::MAX),
                    },
                    Entry {
                        account: user,
                        amount: MicroUsd(i64::MAX),
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
        let txn = Transaction::new(
            TxnKind::Deposit,
            vec![
                Entry {
                    account: ext,
                    amount: MicroUsd(-1),
                },
                Entry {
                    account: user,
                    amount: MicroUsd(1),
                },
            ],
        )
        .unwrap();
        assert!(matches!(bal.apply(&txn), Err(LedgerError::Overflow)));
        // atomic: nothing moved
        assert_eq!(bal.balance(user).unwrap(), MicroUsd(i64::MAX));
        assert_eq!(bal.balance(ext).unwrap(), MicroUsd(-i64::MAX));
    }

    proptest! {
        /// Repeated-account multi-entry transactions: conservation + atomicity hold
        /// when one account appears in several legs of the same transaction.
        #[test]
        fn conservation_with_repeated_account_entries(
            deposit in 1i64..1_000_000_000,
            moves in proptest::collection::vec((0usize..4, 0usize..4, 1i64..500_000, 1i64..500_000), 1..50),
        ) {
            let ext = acct();
            let accts: Vec<AccountId> = (0..4).map(|_| acct()).collect();
            let mut bal = Balances::default();
            bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
            for a in &accts { bal.open(*a, OwnerType::User, Currency::Usdc).unwrap(); }
            bal.apply(&Transaction::new(TxnKind::Deposit, vec![
                Entry { account: ext, amount: MicroUsd(-deposit) },
                Entry { account: accts[0], amount: MicroUsd(deposit) },
            ]).unwrap()).unwrap();
            for (from, to, a1, a2) in moves {
                if from == to { continue; }
                let txn = Transaction::new(TxnKind::Trade, vec![
                    Entry { account: accts[from], amount: MicroUsd(-a1) },
                    Entry { account: accts[from], amount: MicroUsd(-a2) },
                    Entry { account: accts[to],   amount: MicroUsd(a1 + a2) },
                ]).unwrap();
                let before = bal.balance(accts[from]).unwrap();
                if bal.apply(&txn).is_err() {
                    // atomicity: a rejected transaction moved nothing
                    prop_assert_eq!(bal.balance(accts[from]).unwrap(), before);
                }
            }
            prop_assert_eq!(bal.internal_total(Currency::Usdc).0, deposit);
            prop_assert_eq!(bal.balance(ext).unwrap().0, -deposit);
            for a in &accts { prop_assert!(bal.balance(*a).unwrap().0 >= 0); }
        }

        /// Conservation: after any accepted transaction sequence, internal_total == -balance(external),
        /// and no internal account is ever negative.
        #[test]
        fn conservation_over_random_transfers(deposit in 1i64..1_000_000_000, moves in proptest::collection::vec((0usize..4, 0usize..4, 1i64..1_000_000), 1..50)) {
            let ext = acct();
            let accts: Vec<AccountId> = (0..4).map(|_| acct()).collect();
            let mut bal = Balances::default();
            bal.open(ext, OwnerType::External, Currency::Usdc).unwrap();
            for a in &accts { bal.open(*a, OwnerType::User, Currency::Usdc).unwrap(); }
            let fund = Transaction::new(TxnKind::Deposit, vec![
                Entry { account: ext, amount: MicroUsd(-deposit) },
                Entry { account: accts[0], amount: MicroUsd(deposit) },
            ]).unwrap();
            bal.apply(&fund).unwrap();
            for (from, to, amt) in moves {
                if from == to { continue; }
                if let Ok(txn) = Transaction::new(TxnKind::Trade, vec![
                    Entry { account: accts[from], amount: MicroUsd(-amt) },
                    Entry { account: accts[to],   amount: MicroUsd(amt)  },
                ]) { let _ = bal.apply(&txn); }
            }
            prop_assert_eq!(bal.internal_total(Currency::Usdc).0, deposit);
            prop_assert_eq!(bal.balance(ext).unwrap().0, -deposit);
            for a in &accts { prop_assert!(bal.balance(*a).unwrap().0 >= 0); }
        }
    }
}
