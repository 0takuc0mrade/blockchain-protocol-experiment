use std::{
    sync::{
        Arc, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy::primitives::{Address, B256, Bytes};
use primitives::{
    constants::{
        DEV_DIFFICULTY_LEADING_ZEROS, DEV_TARGET_BLOCK_TIME_SECS, DIFFICULTY_LEADING_ZEROS,
        TARGET_BLOCK_TIME_SECS,
    },
    node::{BlockState, BreejaGenesis},
};

use crate::pool;

/// The knobs that define how this node mines.
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// Difficulty the first mined block is attempted at, in leading zero bits.
    /// From here the miner retargets towards `target_block_time`.
    pub difficulty: u8,
    /// How long the node wants each block to take.
    pub target_block_time: Duration,
    /// Address that receives block rewards and transaction fees.
    pub miner: Address,
}

impl ChainConfig {
    /// Mainnet-ish defaults: 15 second blocks.
    pub fn new(miner: Address) -> Self {
        Self {
            difficulty: DIFFICULTY_LEADING_ZEROS,
            target_block_time: Duration::from_secs(TARGET_BLOCK_TIME_SECS),
            miner,
        }
    }

    /// Development defaults: 2 second blocks, so a demo produces visible progress.
    pub fn dev(miner: Address) -> Self {
        Self {
            difficulty: DEV_DIFFICULTY_LEADING_ZEROS,
            target_block_time: Duration::from_secs(DEV_TARGET_BLOCK_TIME_SECS),
            miner,
        }
    }
}

/// A cloneable handle to the node's state.
#[derive(Clone)]
pub struct Chain {
    state: Arc<RwLock<BlockState>>,
    config: Arc<ChainConfig>,
    shutdown: Arc<AtomicBool>,
    /// Bumped every time a transaction is accepted. The miner watches this so it
    /// can abandon a block it has already started and rebuild it with the new
    /// transaction included, rather than making that transaction wait for the
    /// current search to finish.
    mempool_generation: Arc<AtomicU64>,
}

impl Chain {
    /// Builds a chain from a genesis definition, sealing the genesis block.
    pub fn new(genesis: BreejaGenesis, config: ChainConfig) -> Self {
        let state = BlockState::new_with_gensis(genesis, config.difficulty);
        Self {
            state: Arc::new(RwLock::new(state)),
            config: Arc::new(config),
            shutdown: Arc::new(AtomicBool::new(false)),
            mempool_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn config(&self) -> &ChainConfig {
        &self.config
    }

    /// Read access to the chain state.
    ///
    /// A poisoned lock means a thread panicked mid-write, which would leave the
    /// chain in an unknown state, so there is nothing sensible to do but crash.
    pub fn read(&self) -> RwLockReadGuard<'_, BlockState> {
        self.state.read().expect("chain state lock was poisoned")
    }

    /// Write access to the chain state.
    pub fn write(&self) -> RwLockWriteGuard<'_, BlockState> {
        self.state.write().expect("chain state lock was poisoned")
    }

    /// Validates a raw signed transaction and queues it for mining.
    pub fn submit_raw_transaction(&self, raw: &Bytes) -> Result<B256, anyhow::Error> {
        let hash = pool::admit(&mut self.write(), raw)?;
        self.mempool_generation.fetch_add(1, Ordering::Release);
        tracing::info!(tx = %hash, "accepted transaction into the mempool");
        Ok(hash)
    }

    /// A counter that changes whenever the mempool gains a transaction.
    pub fn mempool_generation(&self) -> u64 {
        self.mempool_generation.load(Ordering::Acquire)
    }

    /// Asks the miner to stop at its next opportunity.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}
