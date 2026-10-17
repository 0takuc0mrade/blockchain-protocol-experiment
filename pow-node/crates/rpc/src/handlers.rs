use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorObjectOwned, Params, error::ErrorCode},
};
use node::{
    evm::{BreejaDb, intrinsic_gas},
    executor::{ExecutionResult, simulate_transaction},
};
use primitives::{
    constants::{CHAIN_ID, GAS_PRICE},
    node::{Block, BlockState, PendingTransaction},
    params::{BlockTag, CallRequest, LogFilter},
    response::{
        BlockResponse, LogResponse, TransactionReceiptResponse, TransactionResponse, data_bytes,
        data_hash, quantity,
    },
};
use serde_json::Value;

use crate::RPCContext;

/// JSON-RPC error code Ethereum clients use for "your request was understood but
/// is not valid", as opposed to a malformed one.
const INVALID_INPUT: i32 = -32000;

/// Turns an internal error into a JSON-RPC error object the client can read.
fn invalid_input(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(INVALID_INPUT, error.to_string(), None::<()>)
}

fn invalid_params(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        ErrorCode::InvalidParams.code(),
        error.to_string(),
        None::<()>,
    )
}

/// Parses a 32-byte hash parameter.
fn parse_hash(raw: &str) -> Result<B256, ErrorObjectOwned> {
    raw.parse::<B256>()
        .map_err(|e| invalid_params(format!("invalid 32-byte hash {raw:?}: {e}")))
}

/// Resolves a block tag against the chain head, erroring on a block that does not
/// exist rather than silently substituting the latest one.
fn resolve_tag(state: &BlockState, tag: &BlockTag) -> Result<u64, ErrorObjectOwned> {
    tag.resolve(state.get_block_number()).map_err(invalid_input)
}

/// The chain's id, used by wallets for EIP-155 replay protection.
pub fn chain_id_handler() -> String {
    quantity(CHAIN_ID)
}

/// The number of the chain head. A freshly started node sits at `0x0`, the genesis
/// block, and climbs from there as the miner seals blocks.
pub fn block_number_handler(context: &RPCContext) -> String {
    quantity(context.chain.read().get_block_number())
}

pub fn gas_price_handler(context: &RPCContext) -> String {
    quantity(context.chain.read().gas_price())
}

pub fn get_code_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);
    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    Ok(data_bytes(
        &state.get_code_at(&address, number).expect("resolved block"),
    ))
}

pub fn get_storage_at_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let slot: U256 = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);
    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    let value = state
        .get_storage_at(&address, slot, number)
        .expect("resolved block");
    // Storage is DATA, not a minimal-width QUANTITY, including for zero.
    Ok(format!("0x{value:064x}"))
}

pub fn get_logs_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Vec<LogResponse>> {
    let filter: LogFilter = params.one()?;
    if filter
        .topics
        .as_ref()
        .is_some_and(|topics| topics.len() > 4)
    {
        return Err(invalid_params("at most four topic positions are allowed"));
    }
    let state = context.chain.read();
    let (from, to) = if let Some(hash) = filter.block_hash {
        if filter.from_block.is_some() || filter.to_block.is_some() {
            return Err(invalid_params(
                "blockHash cannot be combined with fromBlock or toBlock",
            ));
        }
        let number = state
            .get_block_by_hash(&hash)
            .ok_or_else(|| invalid_input("unknown block hash"))?
            .number;
        (number, number)
    } else {
        let head = state.get_block_number();
        let from = filter
            .from_block
            .unwrap_or(BlockTag::Latest)
            .resolve(head)
            .map_err(invalid_params)?;
        let to = filter
            .to_block
            .unwrap_or(BlockTag::Latest)
            .resolve(head)
            .map_err(invalid_params)?;
        if from > to {
            return Err(invalid_params("fromBlock must not exceed toBlock"));
        }
        (from, to)
    };
    // Traverse canonical block/transaction order, never the receipt HashMap order.
    Ok((from..=to)
        .filter_map(|number| state.get_block(number))
        .flat_map(|block| &block.transactions)
        .filter_map(|hash| state.get_receipt(hash))
        .flat_map(|receipt| &receipt.logs)
        .filter(|log| filter.matches(log))
        .map(LogResponse::from)
        .collect())
}

/// Renders a block, inlining full transaction objects if the caller asked for them.
fn render_block(state: &BlockState, number: u64, full_transactions: bool) -> Value {
    let Some(block) = state.get_block(number) else {
        return Value::Null;
    };

    let transactions: Vec<Value> = if full_transactions {
        block
            .transactions
            .iter()
            .filter_map(|hash| state.get_transaction(hash))
            .map(|tx| {
                serde_json::to_value(TransactionResponse::from(tx))
                    .expect("a transaction response always serializes")
            })
            .collect()
    } else {
        block
            .transactions
            .iter()
            .map(|hash| Value::String(data_hash(hash)))
            .collect()
    };

    serde_json::to_value(BlockResponse::from_block(block, transactions))
        .expect("a block response always serializes")
}

pub fn get_block_by_number_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    // Ethereum's optional trailing parameters have to be read one at a time: a
    // client may send just the block, or the block and the `fullTransactions`
    // flag, and both are valid.
    let mut sequence = params.sequence();
    let tag: BlockTag = sequence.next()?;
    let full_transactions = sequence.optional_next::<bool>()?.unwrap_or(false);

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    Ok(render_block(&state, number, full_transactions))
}

pub fn get_block_by_hash_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    let mut sequence = params.sequence();
    let hash = parse_hash(&sequence.next::<String>()?)?;
    let full_transactions = sequence.optional_next::<bool>()?.unwrap_or(false);

    let state = context.chain.read();
    // An unknown hash is `null`, not an error: the caller asked a legitimate
    // question and the answer is "no such block".
    let Some(number) = state.get_block_by_hash(&hash).map(|block| block.number) else {
        return Ok(Value::Null);
    };
    Ok(render_block(&state, number, full_transactions))
}

pub fn get_transaction_by_hash_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<Value> {
    let hash = parse_hash(&params.one::<String>()?)?;

    let state = context.chain.read();
    match state.get_transaction(&hash) {
        Some(tx) => Ok(serde_json::to_value(TransactionResponse::from(tx))
            .expect("a transaction response always serializes")),
        None => Ok(Value::Null),
    }
}

/// A receipt exists only once a transaction has been mined. A transaction still
/// sitting in the mempool correctly returns `null` here, which is exactly how a
/// client polls for confirmation.
pub fn get_transaction_receipt_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<Value> {
    let hash = parse_hash(&params.one::<String>()?)?;

    let state = context.chain.read();
    match state.get_receipt(&hash) {
        Some(receipt) => Ok(
            serde_json::to_value(TransactionReceiptResponse::from(receipt))
                .expect("a receipt response always serializes"),
        ),
        None => Ok(Value::Null),
    }
}

pub fn get_balance_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let balance = match tag {
        // "pending" means "after everything in the mempool lands", which is what a
        // wallet wants before it builds the next transaction.
        BlockTag::Pending => state.get_pending_balance(&address),
        _ => {
            let number = resolve_tag(&state, &tag)?;
            state
                .get_account_at(&address, number)
                .map(|account| account.balance)
                .unwrap_or(U256::ZERO)
        }
    };

    Ok(quantity(balance))
}

/// The account's nonce: how many transactions it has sent.
pub fn get_transaction_count_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let nonce = match tag {
        BlockTag::Pending => state.get_pending_nonce(&address),
        _ => {
            let number = resolve_tag(&state, &tag)?;
            state
                .get_account_at(&address, number)
                .map(|account| account.nonce)
                .unwrap_or(0)
        }
    };

    Ok(quantity(nonce))
}

/// Accepts a signed transaction, validates it, and queues it for mining.
///
/// The response is the transaction hash, returned the moment the transaction is
/// *accepted* — not when it is mined. The client then polls
/// `eth_getTransactionReceipt` until a receipt appears, which is the standard
/// Ethereum confirmation flow.
pub fn send_raw_transaction_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let raw: String = params.one()?;
    let raw: Bytes = raw
        .parse()
        .map_err(|e| invalid_params(format!("raw transaction is not valid hex: {e}")))?;

    let hash = context
        .chain
        .submit_raw_transaction(&raw)
        .map_err(invalid_input)?;

    Ok(data_hash(&hash))
}

struct Simulation {
    db: BreejaDb,
    block: Block,
    tx: PendingTransaction,
    available: Option<U256>,
}

impl Simulation {
    fn probe(&self, gas_limit: u64) -> RpcResult<ExecutionResult> {
        let mut tx = self.tx.clone();
        tx.gas = gas_limit;
        simulate_transaction(&self.db, &tx, &self.block).map_err(invalid_input)
    }
}

/// Capture one coherent historical state/header view, then release the read lock.
fn simulation_request(
    params: Params<'_>,
    context: &RPCContext,
    default_price: u128,
) -> RpcResult<Simulation> {
    let mut sequence = params.sequence();
    let request: CallRequest = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);
    let input = request.calldata().map_err(invalid_params)?;
    let gas = request
        .gas
        .map(|gas| u64::try_from(gas).map_err(|_| invalid_params("gas exceeds u64")))
        .transpose()?;
    let gas_price = request
        .gas_price
        .map(|price| u128::try_from(price).map_err(|_| invalid_params("gasPrice exceeds u128")))
        .transpose()?
        .unwrap_or(default_price);
    let nonce = request
        .nonce
        .map(|nonce| u64::try_from(nonce).map_err(|_| invalid_params("nonce exceeds u64")))
        .transpose()?;
    let from = request.from.unwrap_or(Address::ZERO);
    let (accounts, block_hashes, block, available) = {
        let state = context.chain.read();
        let number = resolve_tag(&state, &tag)?;
        let accounts = state
            .accounts_snapshot_at(number)
            .ok_or_else(|| invalid_input("block state is unavailable"))?;
        let block = state
            .get_block(number)
            .cloned()
            .ok_or_else(|| invalid_input("block is unavailable"))?;
        let available = request.from.map(|address| {
            if tag == BlockTag::Pending {
                state.get_pending_balance(&address)
            } else {
                accounts
                    .get(&address)
                    .map(|account| account.balance)
                    .unwrap_or_default()
            }
        });
        (
            accounts,
            state.block_hashes_before(number),
            block,
            available,
        )
    };
    let nonce = nonce.unwrap_or_else(|| {
        accounts
            .get(&from)
            .map(|account| account.nonce)
            .unwrap_or_default()
    });
    let tx = PendingTransaction {
        // Unsigned simulations never enter the pool or acquire a transaction hash.
        hash: B256::ZERO,
        from,
        to: request.to,
        input,
        value: request.value.unwrap_or_default(),
        nonce,
        gas: gas.unwrap_or(block.gas_limit).min(block.gas_limit),
        gas_price,
        chain_id: CHAIN_ID,
        v: 0,
        r: U256::ZERO,
        s: U256::ZERO,
    };
    Ok(Simulation {
        db: BreejaDb::new(accounts, block_hashes),
        block,
        tx,
        available,
    })
}

fn ensure_success(result: &ExecutionResult) -> RpcResult<()> {
    match result {
        ExecutionResult::Success { .. } => Ok(()),
        ExecutionResult::Revert { output, .. } => Err(ErrorObjectOwned::owned(
            3,
            "execution reverted",
            Some(data_bytes(output)),
        )),
        ExecutionResult::Halt { reason, .. } => {
            Err(invalid_input(format!("execution halted: {reason:?}")))
        }
    }
}

/// Execute on a throwaway snapshot, returning bytes without committing changes.
pub fn call_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let simulation = simulation_request(params, context, 0)?;
    let result = simulation.probe(simulation.tx.gas)?;
    ensure_success(&result)?;
    Ok(data_bytes(
        result.output().expect("successful execution has output"),
    ))
}

/// Find a sufficient gas limit by probing independent copies of one state snapshot.
pub fn estimate_gas_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let simulation = simulation_request(params, context, GAS_PRICE)?;
    let kind = simulation.tx.to.map(TxKind::Call).unwrap_or(TxKind::Create);
    let intrinsic = intrinsic_gas(kind, &simulation.tx.input).map_err(invalid_input)?;
    let mut upper = simulation.tx.gas;
    if let Some(available) = simulation.available {
        let funds_for_gas = available
            .checked_sub(simulation.tx.value)
            .ok_or_else(|| invalid_input("insufficient funds for transaction value"))?;
        if simulation.tx.gas_price > 0 {
            let allowance = funds_for_gas / U256::from(simulation.tx.gas_price);
            upper = upper.min(u64::try_from(allowance).unwrap_or(u64::MAX));
        }
    }
    if upper < intrinsic {
        return Err(invalid_input(format!(
            "gas allowance {upper} is below intrinsic cost {intrinsic}"
        )));
    }
    ensure_success(&simulation.probe(upper)?)?;
    let mut lower = intrinsic - 1;
    // GAS-dependent contracts need not be monotonic. The returned upper bound
    // is a successfully executed probe, not a proof of the global minimum.
    while upper - lower > 1 {
        let middle = lower + (upper - lower) / 2;
        if simulation.probe(middle)?.is_success() {
            upper = middle;
        } else {
            lower = middle;
        }
    }
    Ok(quantity(upper))
}
