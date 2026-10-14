//! Block production.
//!
//! The miner runs one loop forever: assemble a candidate block from the mempool,
//! execute it, hunt for a nonce that satisfies the difficulty, then commit it as
//! the new chain head. Every block is mined, including empty ones — an idle
//! proof-of-work chain still burns electricity to keep extending itself, and the
//! block reward is the incentive to keep doing so.
//!
//! The important structural detail is *when the lock is held*. Mining can take
//! many seconds, and holding the state lock through it would freeze every RPC
//! request. So the miner snapshots the state, releases the lock, does its hashing,
//! and only re-acquires the lock for the instant it takes to commit.

use std::{
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::primitives::Bytes;
use primitives::{
    constants::{BLOCK_GAS_LIMIT, EXTRA_DATA},
    node::{Block, retarget_difficulty},
};

use crate::{chain::Chain, evm::BreejaDb, executor::execute_block, pow};

/// Starts the miner on its own OS thread.
///
/// Mining is a tight CPU loop, so it does not belong on an async runtime's worker
/// threads where it would starve the RPC server.
pub fn spawn(chain: Chain) -> JoinHandle<()> {
    thread::Builder::new()
        .name("breeja-miner".to_owned())
        .spawn(move || run(chain))
        .expect("failed to spawn the miner thread")
}

/// The mining loop.
fn run(chain: Chain) {
    let config = chain.config().clone();
    let target_ms = config.target_block_time.as_millis() as u64;
    let mut difficulty = config.difficulty;

    tracing::info!(
        miner = %config.miner,
        difficulty,
        target_block_time_secs = config.target_block_time.as_secs(),
        "miner started"
    );

    while !chain.is_shutting_down() {
        // snapshot the chain, holding the read lock only briefly
        let generation = chain.mempool_generation();
        let (accounts, block_hashes, parent, batch) = {
            let state = chain.read();
            (
                state.accounts_snapshot(),
                state.block_hashes_snapshot(),
                state.head().clone(),
                state.peek_mempool_batch(),
            )
        };

        let number = parent.number + 1;

        // Fix the EVM-visible fields before execution so the mined header uses
        // the same number, timestamp, miner, gas limit, and parent hash.
        let mut block = Block {
            number,
            parent_hash: parent.hash,
            difficulty,
            total_difficulty: parent.total_difficulty + difficulty as u64,
            // A block's timestamp must advance, even if the system clock does not.
            timestamp: unix_now().max(parent.timestamp + 1),
            gas_limit: BLOCK_GAS_LIMIT,
            miner: config.miner,
            extra_data: Bytes::from_static(EXTRA_DATA.as_bytes()),
            ..Default::default()
        };

        // Build the REVM database and execute with no chain lock held.
        let db = BreejaDb::new(accounts, block_hashes);
        let execution = execute_block(db, &batch, &block);
        block.state_root = execution.state_root;
        block.transaction_root = execution.transaction_root;
        block.receipts_root = execution.receipts_root;
        block.gas_used = execution.gas_used;
        block.transactions = execution.transactions.iter().map(|tx| tx.hash).collect();
        let mut header = block.header();

        // the proof of work, also with no lock held
        //
        // The search is abandoned if the node is shutting down, or if a new
        // transaction arrives: there is no point spending another ten seconds on a
        // block that is already out of date when rebuilding it costs nothing. Real
        // miners do the same, for the same reason.
        let abort = || chain.is_shutting_down() || chain.mempool_generation() != generation;
        let Some(seal) = pow::mine(&mut header, difficulty, &abort) else {
            if chain.is_shutting_down() {
                break;
            }
            tracing::debug!(
                number,
                "new transaction arrived; rebuilding block {number} to include it"
            );
            continue;
        };
        block.nonce = seal.nonce;
        block.hash = seal.hash;

        // A block's own hash cannot be known until it is sealed, so the
        // transactions and receipts learn which block they are in only now.
        let mut transactions = execution.transactions;
        let mut receipts = execution.receipts;
        let block_tx_count = transactions.len();
        for tx in &mut transactions {
            tx.block_hash = block.hash;
        }
        for receipt in &mut receipts {
            receipt.block_hash = block.hash;
        }

        // commit, holding the write lock for as little as possible
        {
            let mut state = chain.write();
            if state.head().hash != parent.hash {
                tracing::warn!(
                    "chain head moved while mining block {number}; discarding the candidate"
                );
                continue;
            }
            block.transactions = transactions.iter().map(|tx| tx.hash).collect();
            state.commit_block(execution.accounts, block, transactions, receipts);
        }

        tracing::info!(
            number,
            hash = %seal.hash,
            txs = block_tx_count,
            difficulty,
            nonce = seal.nonce,
            attempts = seal.attempts,
            hash_rate = format_args!("{:.0} H/s", seal.hash_rate()),
            took = format_args!("{:.2}s", seal.elapsed_secs),
            "sealed block"
        );

        // retarget difficulty from what the last block actually cost
        let elapsed_ms = (seal.elapsed_secs * 1_000.0) as u64;
        let next = retarget_difficulty(difficulty, elapsed_ms, target_ms);
        if next != difficulty {
            tracing::info!(from = difficulty, to = next, "retargeting difficulty");
            difficulty = next;
        }
    }

    tracing::info!("miner stopped");
}

/// Current wall-clock time as a Unix timestamp in seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
