use std::collections::HashMap;

use alloy::primitives::{Address, B256, U256};
use primitives::{
    constants::{BLOCK_REWARD, TRANSFER_GAS},
    node::{Account, PendingTransaction, Transaction, TransactionReceipt, hash_list_root},
};

/// What executing a batch of transactions produced.
pub struct Execution {
    /// Account state after the batch, ready to become the new head state.
    pub accounts: HashMap<Address, Account>,
    pub transactions: Vec<Transaction>,
    pub receipts: Vec<TransactionReceipt>,
    pub gas_used: u64,
    pub transaction_root: B256,
    pub receipts_root: B256,
    pub state_root: B256,
}

/// Applies one ETH transfer to the account state.
///
/// Returns the gas used, which for a transfer is always the intrinsic cost: there
/// is no code to execute, so there is nothing else to charge for.
///
/// The ETH does not balance to zero across this function by design — the sender
/// pays `value + fee`, the recipient receives `value`, and the miner receives
/// `fee`. Nothing is created or destroyed.
pub fn apply_transfer(
    accounts: &mut HashMap<Address, Account>,
    tx: &PendingTransaction,
    miner: Address,
) -> Result<u64, anyhow::Error> {
    let fee = U256::from(TRANSFER_GAS) * U256::from(tx.gas_price);
    let total = tx
        .value
        .checked_add(fee)
        .ok_or_else(|| anyhow::anyhow!("transaction cost overflows a U256"))?;

    let sender = accounts.entry(tx.from).or_default();
    if sender.nonce != tx.nonce {
        anyhow::bail!(
            "nonce mismatch for {}: account is at {}, transaction has {}",
            tx.from,
            sender.nonce,
            tx.nonce
        );
    }
    sender.debit(total)?;
    sender.increase_nonce();

    accounts.entry(tx.to).or_default().credit(tx.value)?;
    accounts.entry(miner).or_default().credit(fee)?;

    Ok(TRANSFER_GAS)
}

/// Pays the miner for sealing a block.
///
/// This is the only way new ETH enters the supply, and it is what makes mining
/// worth the electricity. Breeja pays Frontier's 5 ETH; Ethereum reduced this
/// twice before dropping it entirely at the merge.
pub fn pay_block_reward(
    accounts: &mut HashMap<Address, Account>,
    miner: Address,
) -> Result<(), anyhow::Error> {
    accounts.entry(miner).or_default().credit(BLOCK_REWARD)?;
    Ok(())
}

/// Executes a batch of pending transactions, producing the state and receipts the
/// next block will commit to.
///
/// A transaction that fails here is *skipped*, not included. Real Ethereum
/// includes failing transactions and charges for them, because an EVM can revert
/// halfway through and the gas is still spent. Breeja validates everything before
/// it reaches the mempool, so the only failures left are races — a nonce that
/// another transaction already used — and those are better dropped than mined.
pub fn execute_block(
    mut accounts: HashMap<Address, Account>,
    batch: &[PendingTransaction],
    block_number: u64,
    miner: Address,
) -> Execution {
    let mut transactions = Vec::new();
    let mut receipts = Vec::new();
    let mut gas_used = 0u64;

    for tx in batch {
        match apply_transfer(&mut accounts, tx, miner) {
            Ok(tx_gas) => {
                let index = transactions.len() as u64;
                gas_used += tx_gas;

                transactions.push(Transaction {
                    hash: tx.hash,
                    // The block hash is unknown until the block is sealed, so it
                    // is stamped in afterwards.
                    block_hash: B256::ZERO,
                    block_number,
                    transaction_index: index,
                    chain_id: tx.chain_id,
                    from: tx.from,
                    to: tx.to,
                    value: tx.value,
                    nonce: tx.nonce,
                    gas: tx.gas,
                    gas_price: tx.gas_price,
                    input: Default::default(),
                    v: tx.v,
                    r: tx.r,
                    s: tx.s,
                });

                receipts.push(TransactionReceipt {
                    transaction_hash: tx.hash,
                    transaction_index: index,
                    block_hash: B256::ZERO,
                    block_number,
                    from: tx.from,
                    to: tx.to,
                    gas_used: tx_gas,
                    cummulative_gas_used: gas_used,
                    effective_gas_price: tx.gas_price,
                    status: 1,
                });
            }
            Err(error) => {
                tracing::warn!(tx = %tx.hash, %error, "dropping transaction that no longer applies");
            }
        }
    }

    // The miner's reward is paid after the transactions, so a miner cannot spend
    // it inside the very block that grants it.
    if let Err(error) = pay_block_reward(&mut accounts, miner) {
        tracing::error!(%error, "failed to pay block reward");
    }

    let tx_hashes: Vec<B256> = transactions.iter().map(|tx| tx.hash).collect();
    let receipt_hashes: Vec<B256> = receipts.iter().map(|r| r.transaction_hash).collect();

    Execution {
        transaction_root: hash_list_root(&tx_hashes),
        receipts_root: hash_list_root(&receipt_hashes),
        state_root: primitives::node::state_root_of(&accounts),
        accounts,
        transactions,
        receipts,
        gas_used,
    }
}

#[cfg(test)]
mod tests {
    use primitives::constants::{DEV_ADDRESS, DEV_ADDRESS_1, GAS_PRICE};

    use super::*;

    fn one_eth() -> U256 {
        U256::from(10u64).pow(U256::from(18))
    }

    fn funded(balance: U256) -> HashMap<Address, Account> {
        let mut accounts = HashMap::new();
        accounts.insert(DEV_ADDRESS, Account::new(balance));
        accounts
    }

    fn transfer(value: U256, nonce: u64) -> PendingTransaction {
        PendingTransaction {
            hash: B256::repeat_byte(0xab),
            from: DEV_ADDRESS,
            to: DEV_ADDRESS_1,
            value,
            nonce,
            gas: TRANSFER_GAS,
            gas_price: GAS_PRICE,
            chain_id: 1,
            v: 0,
            r: U256::ZERO,
            s: U256::ZERO,
        }
    }

    #[test]
    fn a_transfer_moves_value_charges_a_fee_and_bumps_the_nonce() {
        let miner = Address::repeat_byte(0x99);
        let mut accounts = funded(one_eth() * U256::from(10u64));
        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);

        let gas = apply_transfer(&mut accounts, &transfer(one_eth(), 0), miner).unwrap();
        assert_eq!(gas, TRANSFER_GAS);

        let sender = &accounts[&DEV_ADDRESS];
        assert_eq!(
            sender.balance,
            one_eth() * U256::from(10u64) - one_eth() - fee
        );
        assert_eq!(sender.nonce, 1);
        assert_eq!(accounts[&DEV_ADDRESS_1].balance, one_eth());
        assert_eq!(accounts[&miner].balance, fee);
    }

    #[test]
    fn no_eth_is_created_or_destroyed_by_a_transfer() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let mut accounts = funded(start);

        apply_transfer(&mut accounts, &transfer(one_eth(), 0), miner).unwrap();

        let total: U256 = accounts
            .values()
            .map(|a| a.balance)
            .fold(U256::ZERO, |a, b| a + b);
        assert_eq!(total, start);
    }

    #[test]
    fn a_transfer_the_sender_cannot_afford_is_rejected() {
        let miner = Address::repeat_byte(0x99);
        // Enough for the value but not the fee on top.
        let mut accounts = funded(one_eth());
        assert!(apply_transfer(&mut accounts, &transfer(one_eth(), 0), miner).is_err());
    }

    #[test]
    fn a_transfer_with_the_wrong_nonce_is_rejected() {
        let miner = Address::repeat_byte(0x99);
        let mut accounts = funded(one_eth() * U256::from(10u64));
        assert!(apply_transfer(&mut accounts, &transfer(one_eth(), 7), miner).is_err());
    }

    #[test]
    fn executing_a_block_pays_the_reward_and_records_receipts() {
        let miner = Address::repeat_byte(0x99);
        let accounts = funded(one_eth() * U256::from(10u64));
        let batch = vec![transfer(one_eth(), 0)];

        let execution = execute_block(accounts, &batch, 1, miner);

        assert_eq!(execution.transactions.len(), 1);
        assert_eq!(execution.receipts.len(), 1);
        assert_eq!(execution.receipts[0].status, 1);
        assert_eq!(execution.gas_used, TRANSFER_GAS);
        assert_eq!(execution.receipts[0].cummulative_gas_used, TRANSFER_GAS);

        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD + fee);

        // Non-empty roots, because this block committed to a transaction.
        assert_ne!(execution.transaction_root, B256::ZERO);
        assert_ne!(execution.state_root, B256::ZERO);
    }

    #[test]
    fn an_empty_block_still_pays_the_miner_but_commits_to_no_transactions() {
        let miner = Address::repeat_byte(0x99);
        let execution = execute_block(funded(one_eth()), &[], 1, miner);

        assert!(execution.transactions.is_empty());
        assert_eq!(execution.gas_used, 0);
        assert_eq!(execution.transaction_root, B256::ZERO);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD);
    }

    #[test]
    fn a_transaction_that_no_longer_applies_is_skipped_not_mined() {
        let miner = Address::repeat_byte(0x99);
        let accounts = funded(one_eth() * U256::from(10u64));
        // Two transactions claiming the same nonce: only the first can apply.
        let batch = vec![transfer(one_eth(), 0), transfer(one_eth(), 0)];

        let execution = execute_block(accounts, &batch, 1, miner);
        assert_eq!(execution.transactions.len(), 1);
    }
}
