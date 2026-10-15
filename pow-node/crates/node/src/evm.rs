//! REVM's database interface over an owned snapshot of Breeja state.
//!
//! Construct the snapshot while briefly holding the chain's read lock, then
//! release the lock before execution. Committing here changes only the snapshot;
//! the miner installs it in live state after sealing the block.

use std::{collections::HashMap, convert::Infallible};

use alloy::primitives::{Address, B256, TxKind, U256, keccak256};
use primitives::node::{Account, BlockState};
use revm::{
    Database, DatabaseCommit,
    context_interface::cfg::GasParams,
    primitives::{AddressMap, StorageKey, StorageValue},
    primitives::{eip3860, hardfork::SpecId},
    state::{Account as RevmAccount, AccountInfo, Bytecode},
};

pub const EVM_SPEC: SpecId = SpecId::SHANGHAI;

/// Shanghai's up-front gas cost for a legacy call or creation transaction.
pub fn intrinsic_gas(kind: TxKind, input: &[u8]) -> Result<u64, anyhow::Error> {
    if kind.is_create() && input.len() > eip3860::MAX_INITCODE_SIZE {
        anyhow::bail!(
            "initcode exceeds Shanghai's {} byte limit",
            eip3860::MAX_INITCODE_SIZE
        );
    }
    Ok(GasParams::new_spec(EVM_SPEC)
        .initial_tx_gas(input, kind.is_create(), 0, 0, 0, None)
        .initial_total_gas())
}

/// A private working copy of accounts, analyzed code, and recent block hashes.
#[derive(Debug, Clone, Default)]
pub struct BreejaDb {
    pub accounts: HashMap<Address, Account>,
    code_by_hash: HashMap<B256, Bytecode>,
    block_hashes: HashMap<u64, B256>,
}

impl BreejaDb {
    pub fn new(accounts: HashMap<Address, Account>, block_hashes: HashMap<u64, B256>) -> Self {
        let mut code_by_hash = HashMap::new();
        for account in accounts.values().filter(|account| !account.code.is_empty()) {
            // Analyze identical bytecode once per snapshot, regardless of how
            // many contracts use it. Raw Bytes in accounts stay VM-independent.
            code_by_hash
                .entry(keccak256(&account.code))
                .or_insert_with(|| Bytecode::new_legacy(account.code.clone()));
        }
        Self {
            accounts,
            code_by_hash,
            block_hashes,
        }
    }

    pub fn from_state(state: &BlockState) -> Self {
        Self::new(state.accounts_snapshot(), state.block_hashes_snapshot())
    }
}

impl Database for BreejaDb {
    type Error = Infallible;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Preserve absence: merely reading an unknown account must not create it.
        Ok(self.accounts.get(&address).map(|account| AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            // Legacy Breeja leaves the empty-code hash at zero. Derive the EVM
            // value from bytes without rewriting existing chain commitments.
            code_hash: keccak256(&account.code),
            code: None,
            ..Default::default()
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self
            .code_by_hash
            .get(&code_hash)
            .cloned()
            .unwrap_or_default())
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        Ok(self
            .accounts
            .get(&address)
            .and_then(|account| account.storage.get(&index).copied())
            .unwrap_or_default())
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.block_hashes.get(&number).copied().unwrap_or_default())
    }
}

impl DatabaseCommit for BreejaDb {
    fn commit(&mut self, changes: AddressMap<RevmAccount>) {
        for (address, account) in changes {
            if !account.is_touched() {
                continue;
            }
            if account.is_selfdestructed() {
                self.accounts.remove(&address);
                continue;
            }

            let entry = self.accounts.entry(address).or_default();
            // A new contract cannot inherit storage left at a reused address.
            if account.is_created() {
                entry.storage.clear();
            }
            entry.balance = account.info.balance;
            entry.nonce = account.info.nonce;
            entry.code_hash = account.info.code_hash;

            if let Some(code) = account.info.code {
                entry.set_code(code.original_bytes());
                if !code.is_empty() {
                    self.code_by_hash.insert(entry.code_hash, code);
                }
            }

            for (slot, value) in account.storage {
                // Absence and zero mean the same thing to the EVM. Keep a
                // canonical sparse map for snapshots and future storage roots.
                if value.present_value == U256::ZERO {
                    entry.storage.remove(&slot);
                } else {
                    entry.storage.insert(slot, value.present_value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Bytes;
    use primitives::node::BreejaGenesis;
    use revm::state::{EvmStorageSlot, TransactionId};

    use super::*;

    const ADDRESS: Address = Address::repeat_byte(0x11);

    fn database(account: Account) -> BreejaDb {
        BreejaDb::new(HashMap::from([(ADDRESS, account)]), HashMap::new())
    }

    fn change(balance: U256, nonce: u64, code: Option<Bytecode>) -> RevmAccount {
        let mut account = RevmAccount::from(AccountInfo {
            balance,
            nonce,
            code_hash: code.as_ref().map(Bytecode::hash_slow).unwrap_or_default(),
            code,
            ..Default::default()
        });
        account.mark_touch();
        account
    }

    fn slot(original: u64, present: u64) -> EvmStorageSlot {
        EvmStorageSlot::new_changed(
            U256::from(original),
            U256::from(present),
            TransactionId::ZERO,
        )
    }

    #[test]
    fn basic_returns_a_funded_accounts_balance_and_nonce() {
        let mut db = database(Account {
            balance: U256::from(123_456),
            nonce: 7,
            ..Default::default()
        });

        let info = db.basic(ADDRESS).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(123_456));
        assert_eq!(info.nonce, 7);
        assert_eq!(info.code_hash, keccak256([]));
    }

    #[test]
    fn basic_returns_none_without_creating_an_unknown_account() {
        let mut db = BreejaDb::default();
        assert!(db.basic(ADDRESS).unwrap().is_none());
        assert!(db.accounts.is_empty());
    }

    #[test]
    fn code_and_storage_are_read_from_the_snapshot() {
        let code = Bytes::from_static(&[0x60, 0x2a, 0x00]);
        let mut account = Account::default();
        account.set_code(code.clone());
        account.storage.insert(U256::ZERO, U256::from(42));
        let mut db = database(account);

        let info = db.basic(ADDRESS).unwrap().unwrap();
        assert!(info.code.is_none(), "REVM loads code by its hash on demand");
        assert_eq!(info.code_hash, keccak256(&code));
        assert_eq!(
            db.code_by_hash(info.code_hash).unwrap().original_bytes(),
            code
        );
        assert_eq!(db.storage(ADDRESS, U256::ZERO).unwrap(), U256::from(42));
        assert_eq!(db.storage(ADDRESS, U256::from(1)).unwrap(), U256::ZERO);
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::ZERO);
        assert!(db.code_by_hash(B256::ZERO).unwrap().is_empty());
    }

    #[test]
    fn block_hash_comes_from_the_chain_snapshot() {
        let state = BlockState::new_with_gensis(BreejaGenesis::default(), 8);
        let mut db = BreejaDb::from_state(&state);
        assert_eq!(db.block_hash(0).unwrap(), state.head().hash);
        assert_eq!(db.block_hash(1).unwrap(), B256::ZERO);
    }

    #[test]
    fn commit_persists_balance_nonce_code_and_storage() {
        let mut db = BreejaDb::default();
        let code = Bytes::from_static(&[0x60, 0x2a, 0x00]);
        let mut account = change(U256::from(500), 3, Some(Bytecode::new_legacy(code.clone())));
        account.mark_created();
        account.storage.insert(U256::ZERO, slot(0, 42));

        db.commit(AddressMap::from_iter([(ADDRESS, account)]));

        let stored = &db.accounts[&ADDRESS];
        assert_eq!(stored.balance, U256::from(500));
        assert_eq!(stored.nonce, 3);
        assert_eq!(stored.code, code);
        assert_eq!(stored.code_hash, keccak256(&code));
        assert_eq!(stored.storage[&U256::ZERO], U256::from(42));
        assert_eq!(
            db.code_by_hash(keccak256(&code)).unwrap().original_bytes(),
            code
        );
    }

    #[test]
    fn commit_ignores_untouched_accounts() {
        let initial = Account::new(U256::from(100));
        let mut db = database(initial.clone());
        let mut account = change(U256::from(999), 10, None);
        account.unmark_touch();

        db.commit(AddressMap::from_iter([(ADDRESS, account)]));
        assert_eq!(db.accounts[&ADDRESS], initial);
    }

    #[test]
    fn commit_removes_selfdestructed_accounts_and_their_storage() {
        let mut initial = Account::new(U256::from(100));
        initial.storage.insert(U256::ZERO, U256::from(42));
        let mut db = database(initial);
        let mut account = change(U256::ZERO, 0, None);
        account.mark_selfdestruct();

        db.commit(AddressMap::from_iter([(ADDRESS, account)]));
        assert!(db.basic(ADDRESS).unwrap().is_none());
        assert_eq!(db.storage(ADDRESS, U256::ZERO).unwrap(), U256::ZERO);
    }

    #[test]
    fn newly_created_accounts_do_not_inherit_old_storage() {
        let mut initial = Account::default();
        initial.storage.insert(U256::from(7), U256::from(99));
        let mut db = database(initial);
        let mut account = change(U256::ZERO, 1, Some(Bytecode::default()));
        account.mark_created();
        account.storage.insert(U256::ZERO, slot(0, 42));

        db.commit(AddressMap::from_iter([(ADDRESS, account)]));
        assert_eq!(db.storage(ADDRESS, U256::from(7)).unwrap(), U256::ZERO);
        assert_eq!(db.storage(ADDRESS, U256::ZERO).unwrap(), U256::from(42));
    }

    #[test]
    fn zero_writes_clear_slots_without_losing_unloaded_code() {
        let code = Bytes::from_static(&[0x00]);
        let mut initial = Account::default();
        initial.set_code(code.clone());
        initial.storage.insert(U256::ZERO, U256::from(42));
        let mut db = database(initial);
        let info = db.basic(ADDRESS).unwrap().unwrap();
        let mut account = RevmAccount::from(info);
        account.mark_touch();
        account.storage.insert(U256::ZERO, slot(42, 0));

        db.commit(AddressMap::from_iter([(ADDRESS, account)]));
        assert!(db.accounts[&ADDRESS].storage.is_empty());
        assert_eq!(db.storage(ADDRESS, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(db.accounts[&ADDRESS].code, code);
        assert_eq!(db.accounts[&ADDRESS].code_hash, keccak256(&code));
    }
}
