use std::collections::HashMap;

use alloy::primitives::{Address, B256, TxKind, U256};
use primitives::{
    constants::{BLOCK_REWARD, CHAIN_ID},
    node::{
        Account, Block, Log, PendingTransaction, Transaction, TransactionReceipt, hash_list_root,
    },
};
use revm::{
    Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::result::ResultAndState,
};

pub use revm::context_interface::result::ExecutionResult;

use crate::evm::{BreejaDb, EVM_SPEC};

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

/// Executes a transfer, contract call, or deployment and commits its snapshot changes.
///
/// REVM owns gas charging, beneficiary fees, and the sender's nonce. A validity
/// error returns `Err` without committing changes; execution success or failure
/// returns an outcome whose transaction-level changes are committed.
pub fn execute_transaction(
    db: &mut BreejaDb,
    tx: &PendingTransaction,
    block: &Block,
) -> Result<ExecutionResult, anyhow::Error> {
    let cfg = CfgEnv::new_with_spec(EVM_SPEC).with_chain_id(CHAIN_ID);
    let outcome = run_transaction(db, tx, block, cfg)?;
    db.commit(outcome.state);
    Ok(outcome.result)
}

/// Executes an unsigned RPC request without persisting any returned changes.
pub fn simulate_transaction(
    db: &BreejaDb,
    tx: &PendingTransaction,
    block: &Block,
) -> Result<ExecutionResult, anyhow::Error> {
    // Hypothetical callers may be unfunded or contracts, and RPC nonce checks
    // must not prevent inspection. Mining uses the strict configuration above.
    let mut cfg = CfgEnv::new_with_spec(EVM_SPEC).with_chain_id(CHAIN_ID);
    cfg.disable_nonce_check = true;
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    tx.value
        .checked_add(U256::from(tx.gas) * U256::from(tx.gas_price))
        .ok_or_else(|| anyhow::anyhow!("transaction cost overflows a U256"))?;
    let mut snapshot = db.clone();
    Ok(run_transaction(&mut snapshot, tx, block, cfg)?.result)
}

fn run_transaction(
    db: &mut BreejaDb,
    tx: &PendingTransaction,
    block: &Block,
    cfg: CfgEnv,
) -> Result<ResultAndState, anyhow::Error> {
    let block_env = BlockEnv {
        number: U256::from(block.number),
        beneficiary: block.miner,
        timestamp: U256::from(block.timestamp),
        gas_limit: block.gas_limit,
        // Breeja has no EIP-1559 burn: every charged wei goes to the miner.
        basefee: 0,
        difficulty: U256::from(block.difficulty),
        // The candidate's own hash does not exist until after execution and PoW.
        prevrandao: Some(block.parent_hash),
        ..Default::default()
    };
    let tx_env = TxEnv {
        tx_type: 0,
        caller: tx.from,
        gas_limit: tx.gas,
        gas_price: tx.gas_price,
        kind: tx.to.map(TxKind::Call).unwrap_or(TxKind::Create),
        value: tx.value,
        data: tx.input.clone(),
        nonce: tx.nonce,
        chain_id: Some(tx.chain_id),
        ..Default::default()
    };

    let mut evm = Context::mainnet()
        .with_db(&mut *db)
        .with_block(block_env)
        .with_cfg(cfg)
        .build_mainnet();
    let outcome = evm
        .transact(tx_env)
        .map_err(|error| anyhow::anyhow!("EVM transaction rejected: {error}"))?;

    // Release REVM's mutable database borrow before applying its returned state.
    drop(evm);
    Ok(outcome)
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
/// Invalid transactions are skipped. A valid REVM execution outcome is included,
/// with actual gas usage and success status. The owned database keeps execution
/// outside the shared chain lock.
pub fn execute_block(mut db: BreejaDb, batch: &[PendingTransaction], block: &Block) -> Execution {
    let mut transactions = Vec::new();
    let mut receipts = Vec::new();
    let mut gas_used = 0u64;
    let mut log_index = 0u64;

    for tx in batch {
        // Reserve the transaction's declared limit before trying it. It stays
        // queued if it cannot fit; actual charged gas determines the next budget.
        if tx.gas > block.gas_limit.saturating_sub(gas_used) {
            continue;
        }
        match execute_transaction(&mut db, tx, block) {
            Ok(result) => {
                let tx_gas = result.tx_gas_used();
                let index = transactions.len() as u64;
                gas_used += tx_gas;

                transactions.push(Transaction {
                    hash: tx.hash,
                    // The block hash is unknown until the block is sealed, so it
                    // is stamped in afterwards.
                    block_hash: B256::ZERO,
                    block_number: block.number,
                    transaction_index: index,
                    chain_id: tx.chain_id,
                    from: tx.from,
                    to: tx.to,
                    value: tx.value,
                    nonce: tx.nonce,
                    gas: tx.gas,
                    gas_price: tx.gas_price,
                    input: tx.input.clone(),
                    v: tx.v,
                    r: tx.r,
                    s: tx.s,
                });

                receipts.push(TransactionReceipt {
                    transaction_hash: tx.hash,
                    transaction_index: index,
                    block_hash: B256::ZERO,
                    block_number: block.number,
                    from: tx.from,
                    to: tx.to,
                    contract_address: result.created_address(),
                    logs: result
                        .logs()
                        .iter()
                        .map(|log| {
                            let entry = Log {
                                address: log.address,
                                topics: log.data.topics().to_vec(),
                                data: log.data.data.clone(),
                                block_hash: B256::ZERO,
                                block_number: block.number,
                                transaction_hash: tx.hash,
                                transaction_index: index,
                                log_index,
                            };
                            log_index += 1;
                            entry
                        })
                        .collect(),
                    gas_used: tx_gas,
                    cummulative_gas_used: gas_used,
                    effective_gas_price: tx.gas_price,
                    status: u8::from(result.is_success()),
                });
            }
            Err(error) => {
                tracing::warn!(tx = %tx.hash, %error, "dropping transaction that no longer applies");
            }
        }
    }

    // The miner's reward is paid after the transactions, so a miner cannot spend
    // it inside the very block that grants it.
    if let Err(error) = pay_block_reward(&mut db.accounts, block.miner) {
        tracing::error!(%error, "failed to pay block reward");
    }

    let tx_hashes: Vec<B256> = transactions.iter().map(|tx| tx.hash).collect();
    let receipt_hashes: Vec<B256> = receipts.iter().map(|r| r.transaction_hash).collect();

    Execution {
        transaction_root: hash_list_root(&tx_hashes),
        receipts_root: hash_list_root(&receipt_hashes),
        state_root: primitives::node::state_root_of(&db.accounts),
        accounts: db.accounts,
        transactions,
        receipts,
        gas_used,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Bytes;
    use primitives::{
        constants::{BLOCK_GAS_LIMIT, DEV_ADDRESS, DEV_ADDRESS_1, GAS_PRICE, TRANSFER_GAS},
        node::Block,
    };

    use crate::evm::BreejaDb;

    use super::*;

    fn one_eth() -> U256 {
        U256::from(10u64).pow(U256::from(18))
    }

    // The old tests passed a raw account map to apply_transfer. REVM needs an
    // owned database and real block context; the balance/gas expectations stay
    // unchanged because this milestone proves transfer parity.
    fn funded(balance: U256) -> BreejaDb {
        let mut accounts = HashMap::new();
        accounts.insert(DEV_ADDRESS, Account::new(balance));
        BreejaDb::new(accounts, HashMap::new())
    }

    fn transfer(value: U256, nonce: u64) -> PendingTransaction {
        PendingTransaction {
            hash: B256::repeat_byte(0xab),
            from: DEV_ADDRESS,
            // Creation now uses None; an existing transfer keeps its destination.
            to: Some(DEV_ADDRESS_1),
            input: Bytes::new(),
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

    fn candidate(miner: Address) -> Block {
        Block {
            number: 1,
            parent_hash: B256::repeat_byte(0x12),
            timestamp: 1_700_000_000,
            difficulty: 8,
            gas_limit: BLOCK_GAS_LIMIT,
            miner,
            ..Default::default()
        }
    }

    #[test]
    fn a_transfer_through_revm_uses_exactly_21000_gas_and_preserves_accounting() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let mut db = funded(start);
        let fee = U256::from(21_000u64) * U256::from(GAS_PRICE);

        let result =
            execute_transaction(&mut db, &transfer(one_eth(), 0), &candidate(miner)).unwrap();

        assert!(result.is_success());
        assert_eq!(result.tx_gas_used(), 21_000);
        assert_eq!(db.accounts[&DEV_ADDRESS].balance, start - one_eth() - fee);
        assert_eq!(db.accounts[&DEV_ADDRESS].nonce, 1);
        assert_eq!(db.accounts[&DEV_ADDRESS_1].balance, one_eth());
        assert_eq!(db.accounts[&miner].balance, fee);
        let total = db
            .accounts
            .values()
            .fold(U256::ZERO, |sum, account| sum + account.balance);
        assert_eq!(total, start);
    }

    #[test]
    fn a_transfer_moves_value_charges_a_fee_and_bumps_the_nonce() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));
        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);

        let result =
            execute_transaction(&mut db, &transfer(one_eth(), 0), &candidate(miner)).unwrap();
        assert_eq!(result.tx_gas_used(), TRANSFER_GAS);

        let sender = &db.accounts[&DEV_ADDRESS];
        assert_eq!(
            sender.balance,
            one_eth() * U256::from(10u64) - one_eth() - fee
        );
        assert_eq!(sender.nonce, 1);
        assert_eq!(db.accounts[&DEV_ADDRESS_1].balance, one_eth());
        assert_eq!(db.accounts[&miner].balance, fee);
    }

    #[test]
    fn no_eth_is_created_or_destroyed_by_a_transfer() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let mut db = funded(start);

        execute_transaction(&mut db, &transfer(one_eth(), 0), &candidate(miner)).unwrap();

        let total: U256 = db
            .accounts
            .values()
            .map(|a| a.balance)
            .fold(U256::ZERO, |a, b| a + b);
        assert_eq!(total, start);
    }

    #[test]
    fn a_transfer_the_sender_cannot_afford_is_rejected() {
        let miner = Address::repeat_byte(0x99);
        // Enough for the value but not the fee on top.
        let mut db = funded(one_eth());
        assert!(execute_transaction(&mut db, &transfer(one_eth(), 0), &candidate(miner)).is_err());
    }

    #[test]
    fn a_transfer_with_the_wrong_nonce_is_rejected() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));
        assert!(execute_transaction(&mut db, &transfer(one_eth(), 7), &candidate(miner)).is_err());
    }

    #[test]
    fn executing_a_block_pays_the_reward_and_records_receipts() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        let batch = vec![transfer(one_eth(), 0)];

        let execution = execute_block(db, &batch, &candidate(miner));

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
        let execution = execute_block(funded(one_eth()), &[], &candidate(miner));

        assert!(execution.transactions.is_empty());
        assert_eq!(execution.gas_used, 0);
        assert_eq!(execution.transaction_root, B256::ZERO);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD);
    }

    #[test]
    fn a_transaction_that_no_longer_applies_is_skipped_not_mined() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        // Two transactions claiming the same nonce: only the first can apply.
        let batch = vec![transfer(one_eth(), 0), transfer(one_eth(), 0)];

        let execution = execute_block(db, &batch, &candidate(miner));
        assert_eq!(execution.transactions.len(), 1);
    }

    #[test]
    fn unused_gas_is_not_charged_to_the_sender() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let mut db = funded(start);
        let mut tx = transfer(one_eth(), 0);
        tx.gas = 100_000;

        let result = execute_transaction(&mut db, &tx, &candidate(miner)).unwrap();

        let fee = U256::from(21_000u64) * U256::from(GAS_PRICE);
        assert!(result.is_success());
        assert_eq!(result.tx_gas_used(), 21_000);
        assert_eq!(db.accounts[&DEV_ADDRESS].balance, start - one_eth() - fee);
        assert_eq!(db.accounts[&miner].balance, fee);
    }

    #[test]
    fn the_sender_must_cover_the_full_gas_reservation_before_execution() {
        let miner = Address::repeat_byte(0x99);
        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);
        let mut db = funded(one_eth() + fee);
        let before = db.accounts.clone();
        let mut tx = transfer(one_eth(), 0);
        tx.gas = 100_000;

        // The old transfer helper only checked value + 21,000 gas. Ethereum
        // validity also requires affordability of the full declared gas limit.
        assert!(execute_transaction(&mut db, &tx, &candidate(miner)).is_err());
        assert_eq!(db.accounts, before);
    }

    #[test]
    fn a_transaction_over_the_block_gas_limit_is_rejected_without_changes() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));
        let before = db.accounts.clone();
        let mut tx = transfer(one_eth(), 0);
        tx.gas = BLOCK_GAS_LIMIT + 1;

        assert!(execute_transaction(&mut db, &tx, &candidate(miner)).is_err());
        assert_eq!(db.accounts, before);
    }

    #[test]
    fn a_transaction_for_another_chain_is_rejected_without_changes() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));
        let before = db.accounts.clone();
        let mut tx = transfer(one_eth(), 0);
        tx.chain_id = CHAIN_ID + 1;

        assert!(execute_transaction(&mut db, &tx, &candidate(miner)).is_err());
        assert_eq!(db.accounts, before);
    }

    #[test]
    fn a_sender_who_is_also_the_miner_keeps_the_fee() {
        let start = one_eth() * U256::from(10u64);
        let mut db = funded(start);

        let result =
            execute_transaction(&mut db, &transfer(one_eth(), 0), &candidate(DEV_ADDRESS)).unwrap();

        assert!(result.is_success());
        assert_eq!(result.tx_gas_used(), 21_000);
        assert_eq!(db.accounts[&DEV_ADDRESS].balance, start - one_eth());
        assert_eq!(db.accounts[&DEV_ADDRESS].nonce, 1);
        assert_eq!(db.accounts[&DEV_ADDRESS_1].balance, one_eth());
    }

    #[test]
    fn a_contract_call_that_cannot_fit_is_left_unexecuted() {
        let miner = Address::repeat_byte(0x99);
        let address = Address::repeat_byte(0x55);
        let mut db = funded(one_eth() * U256::from(10));
        let mut contract = Account::default();
        contract.set_code("0x5b600056".parse().unwrap()); // Infinite loop: burn all gas.
        db.accounts.insert(address, contract);
        // Rebuild the code cache after installing the test fixture.
        let db = BreejaDb::new(db.accounts, HashMap::new());
        let mut first = transfer(U256::ZERO, 0);
        first.to = Some(address);
        first.gas = 2_000_000;
        let mut second = first.clone();
        second.nonce = 1;
        second.hash = B256::repeat_byte(0xcd);

        let execution = execute_block(db, &[first, second], &candidate(miner));

        assert_eq!(execution.gas_used, 2_000_000);
        assert!(execution.gas_used <= BLOCK_GAS_LIMIT);
        assert_eq!(execution.transactions.len(), 1);
        assert_eq!(execution.receipts[0].status, 0);
        assert_eq!(execution.accounts[&DEV_ADDRESS].nonce, 1);
    }

    #[test]
    fn simulation_allows_hypothetical_callers_without_committing_any_state() {
        let miner = Address::repeat_byte(0x99);
        let address = Address::repeat_byte(0x55);
        let mut db = funded(U256::ZERO);
        let mut contract = Account::default();
        contract.set_code("0x602a600055602a60005260206000f3".parse().unwrap());
        db.accounts.insert(address, contract);
        let mut db = BreejaDb::new(db.accounts, HashMap::new());
        let before = db.accounts.clone();
        let mut tx = transfer(one_eth(), 99);
        tx.to = Some(address);
        tx.gas = 100_000;

        let result = simulate_transaction(&db, &tx, &candidate(miner)).unwrap();
        assert!(result.is_success());
        assert_eq!(
            result.output().unwrap().as_ref(),
            U256::from(42).to_be_bytes::<32>()
        );
        assert_eq!(db.accounts, before);
        assert!(execute_transaction(&mut db, &tx, &candidate(miner)).is_err());
        assert_eq!(db.accounts, before);
    }
}
