//! Breeja: a proof-of-work Ethereum node without an EVM.
//!
//! Starting the node does three things: build the genesis state, start the miner
//! on its own thread, and start the JSON-RPC server. From then on the miner
//! extends the chain and the server handls any request sent in

use std::time::Duration;

use alloy::primitives::{Address, U256};
use node::{Chain, ChainConfig, miner};
use primitives::{
    constants::{
        DEV_ADDRESS, DEV_ADDRESS_1, DEV_ADDRESS_2, DEV_ADDRESS_3, DEV_BALANCE, RPC_ADDRESS,
    },
    node::BreejaGenesis,
};
use rpc::{RPCContext, start_server};

/// Command-line options, parsed by hand to keep the dependency list short.
struct Options {
    /// Faster blocks and lower starting difficulty, for demos and tests.
    dev: bool,
    /// Address the JSON-RPC server binds to.
    rpc_address: String,
    /// Address that collects block rewards and fees.
    miner_address: Address,
    /// Override the starting difficulty, in leading zero bits.
    difficulty: Option<u8>,
    /// Override the target block time, in seconds.
    target_block_time: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dev: false,
            rpc_address: RPC_ADDRESS.to_owned(),
            // By default the node mines to the first development account, so the
            // rewards land somewhere whose key is known.
            miner_address: DEV_ADDRESS,
            difficulty: None,
            target_block_time: None,
        }
    }
}

const USAGE: &str = "\
breeja - a proof-of-work Ethereum node without an EVM

USAGE:
    breeja [OPTIONS]

OPTIONS:
    --dev                        Low starting difficulty and 2s target blocks
    --rpc-address <ADDR>         JSON-RPC bind address (default 127.0.0.1:8545)
    --miner <ADDRESS>            Address that receives block rewards and fees
    --difficulty <BITS>          Starting difficulty, in leading zero bits
    --target-block-time <SECS>   Block time the miner retargets towards
    -h, --help                   Print this help
";

fn parse_options() -> Result<Options, anyhow::Error> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<String, anyhow::Error> {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };

        match arg.as_str() {
            "--dev" => options.dev = true,
            "--rpc-address" => options.rpc_address = value("--rpc-address")?,
            "--miner" => options.miner_address = value("--miner")?.parse()?,
            "--difficulty" => options.difficulty = Some(value("--difficulty")?.parse()?),
            "--target-block-time" => {
                options.target_block_time = Some(value("--target-block-time")?.parse()?)
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    Ok(options)
}

/// The accounts a fresh Breeja chain starts with.
///
/// These are the standard Anvil/Hardhat development accounts. Their private keys
/// are published in the README, which is the point: a student can sign a real
/// transaction against a brand new chain without generating a key first.
fn genesis() -> BreejaGenesis {
    BreejaGenesis {
        accounts: vec![
            (DEV_ADDRESS, DEV_BALANCE),
            (DEV_ADDRESS_1, DEV_BALANCE),
            (DEV_ADDRESS_2, DEV_BALANCE),
            (DEV_ADDRESS_3, DEV_BALANCE),
        ],
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let options = parse_options()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let mut config = if options.dev {
        ChainConfig::dev(options.miner_address)
    } else {
        ChainConfig::new(options.miner_address)
    };
    if let Some(difficulty) = options.difficulty {
        config.difficulty = difficulty;
    }
    if let Some(secs) = options.target_block_time {
        config.target_block_time = Duration::from_secs(secs);
    }

    let genesis = genesis();
    let chain = Chain::new(genesis.clone(), config);

    {
        let state = chain.read();
        tracing::info!(
            genesis_hash = %state.head().hash,
            accounts = genesis.accounts.len(),
            "genesis block sealed"
        );
        for (address, balance) in &genesis.accounts {
            tracing::info!(%address, balance_eth = %to_eth(*balance), "pre-funded account");
        }
    }

    // The miner runs on its own OS thread; mining is a tight CPU loop that has no
    // business on the async runtime's workers.
    let miner_thread = miner::spawn(chain.clone());

    let (server, _address) =
        start_server(RPCContext::new(chain.clone()), &options.rpc_address).await?;

    // Ctrl-C stops the miner first, then the server, so no block is half-committed.
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    chain.shutdown();
    let _ = server.stop();
    let _ = miner_thread.join();
    server.stopped().await;

    Ok(())
}

/// Renders a wei balance in ETH, for log lines a human reads.
fn to_eth(wei: U256) -> String {
    let one_eth = U256::from(10u64).pow(U256::from(18));
    let whole = wei / one_eth;
    let fraction = wei % one_eth;
    if fraction.is_zero() {
        whole.to_string()
    } else {
        format!("{whole}.{:018}", fraction.to::<u128>())
            .trim_end_matches('0')
            .to_owned()
    }
}
