//! Contracts exercised through signed transactions and the real proof-of-work miner.

use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, Bytes, U256};
use node::{
    Chain, ChainConfig, miner,
    testing::{
        DEV_PRIVATE_KEY, TEST_CONTRACT_INITCODE, TEST_CONTRACT_RUNTIME, initcode_for_runtime,
        one_eth, sign_contract_call, sign_deployment,
    },
};
use primitives::{
    constants::{BLOCK_REWARD, DEV_ADDRESS, GAS_PRICE},
    node::{BreejaGenesis, TransactionReceipt},
};

struct Harness {
    chain: Chain,
    miner: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn new() -> Self {
        let chain = Chain::new(
            BreejaGenesis {
                accounts: vec![(DEV_ADDRESS, one_eth() * U256::from(100))],
            },
            ChainConfig {
                difficulty: 4,
                target_block_time: Duration::from_millis(50),
                miner: Address::repeat_byte(0x99),
            },
        );
        Self { chain, miner: None }
    }

    fn mine(&mut self, raw: Bytes) -> TransactionReceipt {
        let hash = self.chain.submit_raw_transaction(&raw).unwrap();
        if self.miner.is_none() {
            self.miner = Some(miner::spawn(self.chain.clone()));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(receipt) = self.chain.read().get_receipt(&hash).cloned() {
                return receipt;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for contract transaction {hash}");
    }

    fn deploy(&mut self, initcode: Bytes) -> (Address, TransactionReceipt) {
        let nonce = self.chain.read().get_pending_nonce(&DEV_ADDRESS);
        let receipt =
            self.mine(sign_deployment(DEV_PRIVATE_KEY, initcode, nonce, 150_000).unwrap());
        assert_eq!(receipt.status, 1);
        (
            receipt.contract_address.expect("deployment address"),
            receipt,
        )
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.chain.shutdown();
        if let Some(handle) = self.miner.take() {
            handle.join().unwrap();
        }
    }
}

#[test]
fn a_deployed_contract_has_an_address_and_committed_runtime_code() {
    let mut harness = Harness::new();
    let (address, receipt) = harness.deploy(TEST_CONTRACT_INITCODE.parse().unwrap());
    let state = harness.chain.read();

    assert_eq!(receipt.to, None);
    assert!(receipt.gas_used > 53_000 && receipt.gas_used < 60_000);
    assert_eq!(
        state.get_code(&address),
        TEST_CONTRACT_RUNTIME.parse::<Bytes>().unwrap()
    );
    assert_eq!(state.get_storage(&address, U256::ZERO), U256::ZERO);
    assert_eq!(state.get_account(&address).nonce, 1);
    let tx = state.get_transaction(&receipt.transaction_hash).unwrap();
    assert_eq!(tx.to, None);
    assert_eq!(tx.input, TEST_CONTRACT_INITCODE.parse::<Bytes>().unwrap());
    let block = state.get_block(receipt.block_number).unwrap();
    node::pow::verify(&block.header(), &block.hash, block.difficulty).unwrap();
}

#[test]
fn calling_a_contract_persists_storage_and_a_mined_log() {
    let mut harness = Harness::new();
    let (address, _) = harness.deploy(TEST_CONTRACT_INITCODE.parse().unwrap());
    let input: Bytes = "0x1234".parse().unwrap();
    let receipt = harness.mine(
        sign_contract_call(
            DEV_PRIVATE_KEY,
            address,
            U256::ZERO,
            input.clone(),
            1,
            100_000,
        )
        .unwrap(),
    );
    let state = harness.chain.read();

    assert_eq!(receipt.status, 1);
    assert_eq!(receipt.contract_address, None);
    assert_eq!(receipt.to, Some(address));
    assert!(receipt.gas_used > 43_000 && receipt.gas_used < 45_000);
    assert_eq!(state.get_storage(&address, U256::ZERO), U256::from(42));
    assert_eq!(
        state
            .get_transaction(&receipt.transaction_hash)
            .unwrap()
            .input,
        input
    );
    assert_eq!(receipt.logs.len(), 1);
    let log = &receipt.logs[0];
    assert_eq!(log.address, address);
    assert!(log.topics.is_empty());
    assert!(log.data.is_empty());
    assert_eq!(log.block_hash, receipt.block_hash);
    assert_ne!(log.block_hash, B256::ZERO);
    assert_eq!(log.block_number, receipt.block_number);
    assert_eq!(log.transaction_hash, receipt.transaction_hash);
    assert_eq!(log.transaction_index, receipt.transaction_index);
    assert_eq!(log.log_index, 0);
}

#[test]
fn a_revert_is_mined_charged_and_rolls_back_value_storage_and_logs() {
    let mut harness = Harness::new();
    // Write storage and emit a log, then revert with four zero bytes of data.
    let runtime: Bytes = "0x602a60005560006000a060046000fd".parse().unwrap();
    let (address, _) = harness.deploy(initcode_for_runtime(&runtime));
    let before = harness.chain.read().get_account(&DEV_ADDRESS).balance;
    let receipt = harness.mine(
        sign_contract_call(
            DEV_PRIVATE_KEY,
            address,
            one_eth(),
            Bytes::new(),
            1,
            100_000,
        )
        .unwrap(),
    );
    let state = harness.chain.read();

    assert_eq!(receipt.status, 0);
    assert!(receipt.gas_used > 21_000 && receipt.gas_used < 100_000);
    assert_eq!(state.get_account(&DEV_ADDRESS).nonce, 2);
    assert_eq!(
        state.get_account(&DEV_ADDRESS).balance,
        before - U256::from(receipt.gas_used) * U256::from(GAS_PRICE)
    );
    assert_eq!(state.get_account(&address).balance, U256::ZERO);
    assert_eq!(state.get_storage(&address, U256::ZERO), U256::ZERO);
    assert!(receipt.logs.is_empty());
    assert!(state.get_transaction(&receipt.transaction_hash).is_some());
    assert!(
        state
            .get_block(receipt.block_number)
            .unwrap()
            .transactions
            .contains(&receipt.transaction_hash)
    );
    let total = state
        .accounts_snapshot()
        .values()
        .fold(U256::ZERO, |sum, account| sum + account.balance);
    assert_eq!(
        total,
        one_eth() * U256::from(100) + BLOCK_REWARD * U256::from(state.get_block_number())
    );
}

#[test]
fn out_of_gas_is_mined_and_charges_the_full_limit() {
    let mut harness = Harness::new();
    let runtime: Bytes = "0x5b600056".parse().unwrap(); // Jump to itself forever.
    let (address, _) = harness.deploy(initcode_for_runtime(&runtime));
    let before = harness.chain.read().get_account(&DEV_ADDRESS).balance;
    let receipt = harness.mine(
        sign_contract_call(
            DEV_PRIVATE_KEY,
            address,
            U256::ZERO,
            Bytes::new(),
            1,
            30_000,
        )
        .unwrap(),
    );
    let state = harness.chain.read();

    assert_eq!(receipt.status, 0);
    assert_eq!(receipt.gas_used, 30_000);
    assert_eq!(state.get_account(&DEV_ADDRESS).nonce, 2);
    assert_eq!(
        state.get_account(&DEV_ADDRESS).balance,
        before - U256::from(30_000) * U256::from(GAS_PRICE)
    );
    assert!(receipt.logs.is_empty());
}

#[test]
fn logs_preserve_topics_and_data() {
    let mut harness = Harness::new();
    // MSTORE 42; LOG1 topic 0xaa and the 32-byte memory word.
    let runtime: Bytes = "0x602a60005260aa60206000a100".parse().unwrap();
    let (address, _) = harness.deploy(initcode_for_runtime(&runtime));
    let receipt = harness.mine(
        sign_contract_call(
            DEV_PRIVATE_KEY,
            address,
            U256::ZERO,
            Bytes::new(),
            1,
            100_000,
        )
        .unwrap(),
    );

    assert_eq!(receipt.status, 1);
    assert_eq!(receipt.logs.len(), 1);
    assert_eq!(receipt.logs[0].topics, vec![B256::from(U256::from(0xaa))]);
    assert_eq!(
        receipt.logs[0].data.as_ref(),
        U256::from(42).to_be_bytes::<32>()
    );
}
