use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U256};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use node::{
    Chain, ChainConfig, miner,
    testing::{
        DEV_PRIVATE_KEY, DEV_PRIVATE_KEY_1, address_of, initcode_for_runtime, one_eth,
        sign_contract_call, sign_deployment, sign_transfer,
    },
};
use primitives::node::BreejaGenesis;
use rpc::{RPCContext, start_server};
use serde_json::Value;

/// A running node and a client pointed at it.
struct Harness {
    client: HttpClient,
    chain: Chain,
    _server: jsonrpsee::server::ServerHandle,
    miner: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// Starts a node with the given funded accounts, mining on a dedicated thread.
    async fn start(funded: &[(Address, U256)], mine: bool) -> Self {
        let config = ChainConfig {
            difficulty: 4,
            target_block_time: Duration::from_millis(50),
            miner: Address::repeat_byte(0x99),
        };
        let chain = Chain::new(
            BreejaGenesis {
                accounts: funded.to_vec(),
            },
            config,
        );

        // Port 0 lets the OS pick a free port, so tests can run in parallel.
        let (server, address) = start_server(RPCContext::new(chain.clone()), "127.0.0.1:0")
            .await
            .expect("server should start");
        let url = format!("http://{address}");
        let client = HttpClientBuilder::default().build(url).unwrap();

        let miner = mine.then(|| miner::spawn(chain.clone()));

        Self {
            client,
            chain,
            _server: server,
            miner,
        }
    }

    /// Polls `predicate` until it holds, or panics.
    async fn wait_for(&self, label: &str, mut predicate: impl FnMut(&Chain) -> bool) {
        for _ in 0..1_000 {
            if predicate(&self.chain) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {label}");
    }

    async fn submit(&self, raw: Bytes) -> Value {
        let hash: String = self
            .client
            .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
            .await
            .unwrap();
        self.wait_for("receipt", |chain| {
            chain.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
        self.client
            .request("eth_getTransactionReceipt", rpc_params![hash])
            .await
            .unwrap()
    }

    async fn deploy_runtime(&self, runtime: &[u8]) -> (Address, Value) {
        let nonce = self
            .chain
            .read()
            .get_pending_nonce(&address_of(DEV_PRIVATE_KEY).unwrap());
        let receipt = self
            .submit(
                sign_deployment(
                    DEV_PRIVATE_KEY,
                    initcode_for_runtime(runtime),
                    nonce,
                    150_000,
                )
                .unwrap(),
            )
            .await;
        assert_eq!(receipt["status"], "0x1");
        (
            receipt["contractAddress"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
            receipt,
        )
    }

    fn stop_mining(&mut self) {
        self.chain.shutdown();
        if let Some(handle) = self.miner.take() {
            handle.join().unwrap();
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop_mining();
    }
}

/// Parses a hex quantity the way a client would.
fn from_quantity(raw: &str) -> u128 {
    u128::from_str_radix(raw.trim_start_matches("0x"), 16).expect("a valid hex quantity")
}

#[tokio::test]
async fn code_and_storage_queries_use_historical_snapshots_and_fixed_width_data() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let mut harness = Harness::start(&[(alice, one_eth())], true).await;
    let runtime: Bytes = "0x602a60005500".parse().unwrap();
    let (address, deployment) = harness.deploy_runtime(&runtime).await;
    let nonce = harness.chain.read().get_pending_nonce(&alice);
    let receipt = harness
        .submit(
            sign_contract_call(
                DEV_PRIVATE_KEY,
                address,
                U256::ZERO,
                Bytes::new(),
                nonce,
                100_000,
            )
            .unwrap(),
        )
        .await;
    harness.stop_mining();

    for (who, tag, expected) in [
        (address, "latest", runtime.to_string()),
        (address, "earliest", "0x".to_owned()),
        (alice, "latest", "0x".to_owned()),
        (Address::ZERO, "latest", "0x".to_owned()),
    ] {
        let code: String = harness
            .client
            .request("eth_getCode", rpc_params![who, tag])
            .await
            .unwrap();
        assert_eq!(code, expected);
    }
    for (who, slot, tag, expected) in [
        (address, U256::ZERO, "latest", 42),
        (
            address,
            U256::ZERO,
            deployment["blockNumber"].as_str().unwrap(),
            0,
        ),
        (address, U256::MAX, "latest", 0),
        (Address::ZERO, U256::ZERO, "latest", 0),
    ] {
        let value: String = harness
            .client
            .request("eth_getStorageAt", rpc_params![who, slot, tag])
            .await
            .unwrap();
        assert_eq!(value, format!("0x{expected:064x}"));
        assert_eq!(value.len(), 66);
    }
    let value: String = harness
        .client
        .request(
            "eth_getStorageAt",
            rpc_params![address, "0x0", &receipt["blockNumber"]],
        )
        .await
        .unwrap();
    assert_eq!(value, format!("0x{:064x}", 42));
    let error = harness
        .client
        .request::<Value, _>("eth_getStorageAt", rpc_params![address, "-1", "latest"])
        .await
        .unwrap_err();
    assert!(
        matches!(error, jsonrpsee::core::client::Error::Call(ref error) if error.code() == -32602)
    );
    assert!(
        harness
            .client
            .request::<Value, _>("eth_getCode", rpc_params![address, "0xffffffffffffffff"])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn logs_filter_range_addresses_and_positional_topic_alternatives() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let mut harness = Harness::start(&[(alice, one_eth())], true).await;
    // LOG2: first topic aa, second topic bb, data is the word 42.
    let runtime: Bytes = "0x602a60005260bb60aa60206000a200".parse().unwrap();
    let (first, _) = harness.deploy_runtime(&runtime).await;
    let (second, _) = harness.deploy_runtime(&runtime).await;
    let mut receipts = Vec::new();
    for address in [first, second, first] {
        let nonce = harness.chain.read().get_pending_nonce(&alice);
        receipts.push(
            harness
                .submit(
                    sign_contract_call(
                        DEV_PRIVATE_KEY,
                        address,
                        U256::ZERO,
                        Bytes::new(),
                        nonce,
                        100_000,
                    )
                    .unwrap(),
                )
                .await,
        );
    }
    harness.stop_mining();
    let aa = format!("0x{:064x}", 0xaa);
    let bb = format!("0x{:064x}", 0xbb);
    let cc = format!("0x{:064x}", 0xcc);
    let base = serde_json::json!({"fromBlock": "earliest", "toBlock": "latest"});
    let expected: Vec<Value> = receipts.iter().map(|r| r["logs"][0].clone()).collect();
    for (changes, indices) in [
        (serde_json::json!({}), vec![0, 1, 2]),
        (serde_json::json!({"address": first}), vec![0, 2]),
        (
            serde_json::json!({"address": [first, second]}),
            vec![0, 1, 2],
        ),
        (serde_json::json!({"address": Address::ZERO}), vec![]),
        (serde_json::json!({"topics": [aa, bb]}), vec![0, 1, 2]),
        (
            serde_json::json!({"topics": [null, [cc, bb]]}),
            vec![0, 1, 2],
        ),
        (
            serde_json::json!({"topics": [[cc, aa], null]}),
            vec![0, 1, 2],
        ),
        (serde_json::json!({"topics": [bb, aa]}), vec![]),
        (serde_json::json!({"topics": [aa, cc]}), vec![]),
        (serde_json::json!({"topics": [null, null, null]}), vec![]),
        (
            serde_json::json!({"fromBlock": receipts[1]["blockNumber"], "toBlock": receipts[1]["blockNumber"]}),
            vec![1],
        ),
    ] {
        let mut filter = base.clone();
        filter
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        let logs: Vec<Value> = harness
            .client
            .request("eth_getLogs", rpc_params![&filter])
            .await
            .unwrap();
        assert_eq!(
            logs,
            indices
                .iter()
                .map(|&i| expected[i].clone())
                .collect::<Vec<_>>(),
            "filter: {filter}"
        );
    }
    let logs: Vec<Value> = harness
        .client
        .request(
            "eth_getLogs",
            rpc_params![
                serde_json::json!({"blockHash": receipts[1]["blockHash"], "address": second})
            ],
        )
        .await
        .unwrap();
    assert_eq!(logs, vec![expected[1].clone()]);
    for filter in [
        serde_json::json!({"fromBlock": "latest", "toBlock": "earliest"}),
        serde_json::json!({"toBlock": "0xffffffffffffffff"}),
        serde_json::json!({"blockHash": receipts[0]["blockHash"], "fromBlock": "earliest"}),
        serde_json::json!({"topics": [null, null, null, null, null]}),
        serde_json::json!({"topics": ["0xaa"]}),
        serde_json::json!({"address": "0xaa"}),
    ] {
        let error = harness
            .client
            .request::<Value, _>("eth_getLogs", rpc_params![filter])
            .await
            .unwrap_err();
        assert!(
            matches!(error, jsonrpsee::core::client::Error::Call(ref error) if error.code() == -32602),
            "{error}"
        );
    }
}

#[tokio::test]
async fn gas_price_reports_the_chains_fixed_legacy_price() {
    let harness = Harness::start(&[], false).await;
    let price: String = harness
        .client
        .request("eth_gasPrice", rpc_params![])
        .await
        .unwrap();
    assert_eq!(from_quantity(&price), primitives::constants::GAS_PRICE);
}

#[tokio::test]
async fn eth_chain_id_returns_the_configured_chain_id() {
    let harness = Harness::start(&[], false).await;
    let id: String = harness
        .client
        .request("eth_chainId", rpc_params![])
        .await
        .unwrap();
    assert_eq!(id, "0x1");
}

#[tokio::test]
async fn eth_block_number_starts_at_genesis_and_climbs_as_blocks_are_mined() {
    let harness = Harness::start(&[], true).await;

    harness
        .wait_for("blocks to be mined", |c| c.read().get_block_number() >= 3)
        .await;

    let number: String = harness
        .client
        .request("eth_blockNumber", rpc_params![])
        .await
        .unwrap();
    assert!(
        number.starts_with("0x"),
        "must be a hex quantity, got {number}"
    );
    assert!(from_quantity(&number) >= 3);
}

#[tokio::test]
async fn eth_get_block_by_number_accepts_tags_and_numbers() {
    let harness = Harness::start(&[], true).await;
    harness
        .wait_for("a block", |c| c.read().get_block_number() >= 2)
        .await;

    // By tag, with the fullTransactions flag.
    let latest: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["latest", false])
        .await
        .unwrap();
    assert!(latest["hash"].as_str().unwrap().starts_with("0x"));

    // By tag, with the flag omitted entirely — also valid JSON-RPC.
    let genesis: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["earliest"])
        .await
        .unwrap();
    assert_eq!(genesis["number"], "0x0");
    assert_eq!(
        genesis["parentHash"],
        format!("0x{}", "0".repeat(64)),
        "genesis has no parent"
    );

    // By number.
    let first: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x1", false])
        .await
        .unwrap();
    assert_eq!(first["number"], "0x1");

    // Every field a client expects is present and hex-encoded.
    for field in [
        "number",
        "hash",
        "parentHash",
        "stateRoot",
        "transactionsRoot",
        "receiptsRoot",
        "difficulty",
        "totalDifficulty",
        "timestamp",
        "gasLimit",
        "gasUsed",
        "miner",
        "extraData",
        "nonce",
        "size",
    ] {
        let value = first[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing"));
        assert!(value.starts_with("0x"), "{field} is not hex: {value}");
    }
    // The proof-of-work nonce is a fixed-width 8-byte field, not a quantity.
    assert_eq!(first["nonce"].as_str().unwrap().len(), 18);
}

#[tokio::test]
async fn eth_get_block_by_number_rejects_a_block_that_does_not_exist() {
    let harness = Harness::start(&[], false).await;
    let result: Result<Value, _> = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0xffffff", false])
        .await;
    assert!(
        result.is_err(),
        "a future block should be an error, not null"
    );
}

#[tokio::test]
async fn eth_get_block_by_hash_finds_the_same_block_as_by_number() {
    let harness = Harness::start(&[], true).await;
    harness
        .wait_for("a block", |c| c.read().get_block_number() >= 1)
        .await;

    let by_number: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x1", false])
        .await
        .unwrap();
    let hash = by_number["hash"].as_str().unwrap().to_owned();

    let by_hash: Value = harness
        .client
        .request("eth_getBlockByHash", rpc_params![hash, false])
        .await
        .unwrap();
    assert_eq!(by_hash, by_number);

    // An unknown hash is null, not an error.
    let missing: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![format!("0x{}", "11".repeat(32)), false],
        )
        .await
        .unwrap();
    assert!(missing.is_null());
}

#[tokio::test]
async fn eth_get_balance_reads_current_pending_and_historical_state() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    // Latest, and the one-parameter form.
    let balance: String = harness
        .client
        .request("eth_getBalance", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(balance, "0x56bc75e2d63100000");
    let without_tag: String = harness
        .client
        .request("eth_getBalance", rpc_params![alice])
        .await
        .unwrap();
    assert_eq!(without_tag, balance);

    // An address nobody funded is zero, not an error.
    let empty: String = harness
        .client
        .request(
            "eth_getBalance",
            rpc_params![Address::repeat_byte(0x77), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(empty, "0x0");

    // Send 5 ETH and wait for it to land.
    let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth() * U256::from(5u64), 0).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("the transfer to be mined", |c| {
            c.read().get_account(&bob).balance > U256::ZERO
        })
        .await;

    let bob_now: String = harness
        .client
        .request("eth_getBalance", rpc_params![bob, "latest"])
        .await
        .unwrap();
    assert_eq!(from_quantity(&bob_now), 5 * 10u128.pow(18));

    // At genesis Bob had nothing, and the chain still remembers that.
    let bob_at_genesis: String = harness
        .client
        .request("eth_getBalance", rpc_params![bob, "0x0"])
        .await
        .unwrap();
    assert_eq!(bob_at_genesis, "0x0");

    let _ = tx_hash;
}

#[tokio::test]
async fn eth_get_transaction_count_tracks_mined_and_pending_nonces() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    // No miner: the transactions stay pending, which is what this test is about.
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], false).await;

    let mined: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(mined, "0x0");

    for nonce in 0..2 {
        let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth(), nonce).unwrap();
        let _: String = harness
            .client
            .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
            .await
            .unwrap();
    }

    // "latest" still sees no mined transactions...
    let mined: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(mined, "0x0");

    // ...but "pending" includes the queued ones, which is the nonce a wallet must
    // use for its next transaction.
    let pending: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "pending"])
        .await
        .unwrap();
    assert_eq!(pending, "0x2");
}

#[tokio::test]
async fn a_transfer_produces_a_transaction_and_a_receipt() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth(), 0).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    assert_eq!(tx_hash.len(), 66, "a transaction hash is 32 bytes of hex");

    // Before it is mined, there is a transaction but no receipt. That difference
    // is exactly how a client knows to keep waiting.
    harness
        .wait_for("the transfer to be mined", |c| {
            c.read().get_receipt(&tx_hash.parse().unwrap()).is_some()
        })
        .await;

    let tx: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    assert_eq!(tx["from"], format!("{alice:#x}"));
    assert_eq!(tx["to"], format!("{bob:#x}"));
    assert_eq!(tx["value"], "0xde0b6b3a7640000");
    assert_eq!(tx["nonce"], "0x0");
    assert_eq!(tx["input"], "0x");
    assert_eq!(tx["type"], "0x0", "legacy transaction");
    assert_eq!(tx["chainId"], "0x1");

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    assert_eq!(receipt["status"], "0x1");
    assert_eq!(receipt["gasUsed"], "0x5208");
    assert_eq!(receipt["from"], format!("{alice:#x}"));
    assert_eq!(receipt["to"], format!("{bob:#x}"));
    assert!(
        receipt["contractAddress"].is_null(),
        "a plain transfer does not create a contract"
    );
    assert_eq!(receipt["logs"].as_array().unwrap().len(), 0);
    assert_eq!(receipt["blockHash"], tx["blockHash"]);

    // The block it landed in lists it, and can inline it.
    let block: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![receipt["blockHash"].as_str().unwrap(), true],
        )
        .await
        .unwrap();
    let inlined = block["transactions"].as_array().unwrap();
    assert_eq!(inlined.len(), 1);
    assert_eq!(inlined[0]["hash"], tx_hash);

    // With the flag off, the same block lists bare hashes instead.
    let hashes_only: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![receipt["blockHash"].as_str().unwrap(), false],
        )
        .await
        .unwrap();
    assert_eq!(hashes_only["transactions"][0], tx_hash);
}

#[tokio::test]
async fn an_unknown_transaction_or_receipt_is_null() {
    let harness = Harness::start(&[], false).await;
    let unknown = format!("0x{}", "11".repeat(32));

    let tx: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![unknown.clone()])
        .await
        .unwrap();
    assert!(tx.is_null());

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![unknown])
        .await
        .unwrap();
    assert!(receipt.is_null());
}

#[tokio::test]
async fn eth_send_raw_transaction_rejects_what_the_node_cannot_execute() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], false).await;

    // Not hex at all.
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params!["not hex"])
        .await;
    assert!(result.is_err());

    // Hex, but not a transaction.
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params!["0xdeadbeef"])
        .await;
    assert!(result.is_err());

    // A real transaction for more ETH than the sender has.
    let raw = sign_transfer(
        DEV_PRIVATE_KEY,
        Address::repeat_byte(0x22),
        one_eth() * U256::from(1_000u64),
        0,
    )
    .unwrap();
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await;
    let error = result.expect_err("an unaffordable transfer must be rejected");
    assert!(
        error.to_string().contains("insufficient funds"),
        "the error should say why: {error}"
    );
}

#[tokio::test]
async fn eth_call_executes_transfers_and_calldata_to_empty_accounts() {
    let harness = Harness::start(&[], false).await;

    // Calling an empty account with no calldata returns no data.
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({"to": Address::repeat_byte(0x22), "value": "0x1"}),
                "latest"
            ],
        )
        .await
        .unwrap();
    assert_eq!(result, "0x");

    // The one-parameter form is valid too.
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": Address::repeat_byte(0x22)})],
        )
        .await
        .unwrap();
    assert_eq!(result, "0x");

    // The old no-EVM rejection was correct before integration. An EOA call with
    // calldata now executes successfully and returns empty output.
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({"to": Address::repeat_byte(0x22), "data": "0x70a08231"}),
                "latest"
            ],
        )
        .await
        .unwrap();
    assert_eq!(result, "0x");
}

#[tokio::test]
async fn eth_estimate_gas_returns_the_intrinsic_cost_of_a_transfer() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(10u64))], false).await;

    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice,
                "to": Address::repeat_byte(0x22),
                "value": "0xde0b6b3a7640000"
            })],
        )
        .await
        .unwrap();
    assert_eq!(estimate, "0x5208", "21000 gas");

    // The old creation rejection reflected the missing interpreter. REVM can
    // now estimate valid initcode, even when it deploys empty runtime code.
    let result: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({"from": alice, "data": "0x6080604052"})],
        )
        .await
        .unwrap();
    assert!(from_quantity(&result) > 53_000);
}

#[tokio::test]
async fn an_unsupported_method_gets_the_standard_method_not_found_error() {
    let harness = Harness::start(&[], false).await;

    // M5 implements eth_gasPrice; tracing remains unsupported and must still
    // produce the standard method-not-found error.
    let result: Result<Value, _> = harness
        .client
        .request("debug_traceTransaction", rpc_params![])
        .await;
    let error = result.expect_err("an unregistered method must be an error");
    assert!(
        error.to_string().contains("Method not found") || error.to_string().contains("-32601"),
        "got: {error}"
    );
}

#[tokio::test]
async fn deployment_and_contract_logs_are_reported_over_rpc() {
    use node::testing::{
        TEST_CONTRACT_INITCODE, TEST_CONTRACT_RUNTIME, sign_contract_call, sign_deployment,
    };

    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100))], true).await;
    let raw = sign_deployment(
        DEV_PRIVATE_KEY,
        TEST_CONTRACT_INITCODE.parse().unwrap(),
        0,
        150_000,
    )
    .unwrap();
    let deployment_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("deployment receipt", |chain| {
            chain
                .read()
                .get_receipt(&deployment_hash.parse().unwrap())
                .is_some()
        })
        .await;
    let deployment: Value = harness
        .client
        .request(
            "eth_getTransactionReceipt",
            rpc_params![deployment_hash.clone()],
        )
        .await
        .unwrap();
    assert_eq!(deployment["status"], "0x1");
    assert!(deployment["to"].is_null());
    let address: Address = deployment["contractAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        harness.chain.read().get_code(&address),
        TEST_CONTRACT_RUNTIME
            .parse::<alloy::primitives::Bytes>()
            .unwrap()
    );
    let transaction: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![deployment_hash])
        .await
        .unwrap();
    assert!(transaction["to"].is_null());
    assert_eq!(transaction["input"], TEST_CONTRACT_INITCODE);

    let raw = sign_contract_call(
        DEV_PRIVATE_KEY,
        address,
        U256::ZERO,
        "0x1234".parse().unwrap(),
        1,
        100_000,
    )
    .unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("contract call receipt", |chain| {
            chain.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![hash.clone()])
        .await
        .unwrap();
    assert_eq!(receipt["status"], "0x1");
    assert_eq!(receipt["to"], format!("{address:#x}"));
    assert!(receipt["contractAddress"].is_null());
    assert!(
        from_quantity(receipt["cumulativeGasUsed"].as_str().unwrap())
            >= from_quantity(receipt["gasUsed"].as_str().unwrap())
    );
    assert_eq!(receipt["logs"].as_array().unwrap().len(), 1);
    let log = &receipt["logs"][0];
    assert_eq!(log["address"], format!("{address:#x}"));
    assert_eq!(log["blockHash"], receipt["blockHash"]);
    assert_eq!(log["blockNumber"], receipt["blockNumber"]);
    assert_eq!(log["transactionHash"], hash);
    assert_eq!(log["transactionIndex"], receipt["transactionIndex"]);
    assert_eq!(log["logIndex"], "0x0");
    assert_eq!(log["data"], "0x");
    assert_eq!(log["topics"], serde_json::json!([]));
    assert_eq!(log["removed"], false);
}

#[tokio::test]
async fn eth_call_returns_output_without_changing_any_chain_state() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let mut harness = Harness::start(&[(alice, one_eth() * U256::from(100))], true).await;
    let runtime: Bytes = "0x602a600055602a60005260206000f3".parse().unwrap();
    let (address, _) = harness.deploy_runtime(&runtime).await;
    harness.stop_mining();
    let before = harness.chain.read().accounts_snapshot();
    let height = harness.chain.read().get_block_number();
    let expected = format!("0x{:064x}", 42);

    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({
                "from": Address::repeat_byte(0x77), "to": address,
                "value": "0xde0b6b3a7640000", "nonce": "0x63"
            })],
        )
        .await
        .unwrap();
    assert_eq!(result, expected);
    // A contract can be a hypothetical caller even though it cannot sign a tx.
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({
                "from": address, "to": address, "nonce": "0x63"
            })],
        )
        .await
        .unwrap();
    assert_eq!(result, expected);
    let state = harness.chain.read();
    assert_eq!(state.accounts_snapshot(), before);
    assert_eq!(state.get_block_number(), height);
    assert_eq!(state.mempool_len(), 0);
    assert_eq!(state.get_storage(&address, U256::ZERO), U256::ZERO);
}

#[tokio::test]
async fn gas_estimates_are_sufficient_for_deployment_and_contract_calls() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100))], true).await;
    let runtime: Bytes = "0x602a600055602a60005260206000f3".parse().unwrap();
    let initcode = initcode_for_runtime(&runtime);
    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice, "input": initcode
            })],
        )
        .await
        .unwrap();
    let gas = u64::try_from(from_quantity(&estimate)).unwrap();
    assert!(gas > 53_000 && gas < 100_000);
    let receipt = harness
        .submit(sign_deployment(DEV_PRIVATE_KEY, initcode, 0, gas).unwrap())
        .await;
    assert_eq!(receipt["status"], "0x1");
    let address: Address = receipt["contractAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(harness.chain.read().get_code(&address), runtime);

    let starved: Result<String, _> = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice, "to": address, "gas": "0x5208"
            })],
        )
        .await;
    assert!(starved.is_err());
    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice, "to": address
            })],
        )
        .await
        .unwrap();
    let gas = u64::try_from(from_quantity(&estimate)).unwrap();
    assert!(gas > 43_000 && gas < 50_000);
    assert_eq!(
        harness.chain.read().get_storage(&address, U256::ZERO),
        U256::ZERO
    );
    assert_eq!(harness.chain.read().get_account(&alice).nonce, 1);
    let receipt = harness
        .submit(
            sign_contract_call(DEV_PRIVATE_KEY, address, U256::ZERO, Bytes::new(), 1, gas).unwrap(),
        )
        .await;
    assert_eq!(receipt["status"], "0x1");
    assert!(from_quantity(receipt["gasUsed"].as_str().unwrap()) <= u128::from(gas));
    assert_eq!(
        harness.chain.read().get_storage(&address, U256::ZERO),
        U256::from(42)
    );
}

#[tokio::test]
async fn estimation_searches_for_a_sufficient_limit_instead_of_using_refunded_gas() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100))], true).await;
    // Set a slot and clear it again: the refund lowers the charged gas, but the
    // transaction still needs enough gas to reach the second SSTORE.
    let runtime: Bytes = "0x6001600055600060005500".parse().unwrap();
    let (address, _) = harness.deploy_runtime(&runtime).await;
    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice, "to": address
            })],
        )
        .await
        .unwrap();
    let gas = u64::try_from(from_quantity(&estimate)).unwrap();
    let receipt = harness
        .submit(
            sign_contract_call(DEV_PRIVATE_KEY, address, U256::ZERO, Bytes::new(), 1, gas).unwrap(),
        )
        .await;
    assert_eq!(receipt["status"], "0x1");
    assert!(u128::from(gas) > from_quantity(receipt["gasUsed"].as_str().unwrap()));
    assert_eq!(
        harness.chain.read().get_storage(&address, U256::ZERO),
        U256::ZERO
    );
}

#[tokio::test]
async fn eth_call_uses_the_requested_historical_code_and_storage() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100))], true).await;
    // Empty calldata reads slot 0; non-empty calldata writes its first word.
    let runtime: Bytes = "0x3615600c57600035600055005b60005460005260206000f3"
        .parse()
        .unwrap();
    let (address, deployment) = harness.deploy_runtime(&runtime).await;
    let input = Bytes::from(U256::from(7).to_be_bytes::<32>().to_vec());
    let receipt = harness
        .submit(
            sign_contract_call(DEV_PRIVATE_KEY, address, U256::ZERO, input, 1, 100_000).unwrap(),
        )
        .await;
    assert_eq!(receipt["status"], "0x1");
    let latest: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": address}), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(latest, format!("0x{:064x}", 7));
    let historical: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({"to": address}),
                deployment["blockNumber"].as_str().unwrap()
            ],
        )
        .await
        .unwrap();
    assert_eq!(historical, format!("0x{:064x}", 0));
    let genesis: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": address}), "earliest"],
        )
        .await
        .unwrap();
    assert_eq!(genesis, "0x");
}

#[tokio::test]
async fn simulation_uses_the_selected_block_environment_and_ancestor_hashes() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100))], true).await;
    // Return NUMBER, TIMESTAMP, PREVRANDAO and BLOCKHASH(calldata[0]).
    let runtime: Bytes = "0x4360005242602052446040526000354060605260806000f3"
        .parse()
        .unwrap();
    let (address, receipt) = harness.deploy_runtime(&runtime).await;
    let tag = receipt["blockNumber"].as_str().unwrap();
    let block: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params![tag, false])
        .await
        .unwrap();
    let number = from_quantity(tag);
    let input = Bytes::from(U256::from(number - 1).to_be_bytes::<32>().to_vec());
    let output: Bytes = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": address, "data": input}), tag],
        )
        .await
        .unwrap();
    let parent: B256 = block["parentHash"].as_str().unwrap().parse().unwrap();
    let mut expected = Vec::new();
    expected.extend_from_slice(&U256::from(number).to_be_bytes::<32>());
    expected.extend_from_slice(
        &U256::from(from_quantity(block["timestamp"].as_str().unwrap())).to_be_bytes::<32>(),
    );
    expected.extend_from_slice(parent.as_slice());
    expected.extend_from_slice(parent.as_slice());
    assert_eq!(output.as_ref(), expected);

    let input = Bytes::from(U256::from(number).to_be_bytes::<32>().to_vec());
    let output: Bytes = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": address, "data": input}), tag],
        )
        .await
        .unwrap();
    assert_eq!(
        &output[96..],
        B256::ZERO.as_slice(),
        "BLOCKHASH cannot read the current block"
    );
}

#[tokio::test]
async fn simulations_report_revert_data_and_halts_as_errors() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100))], true).await;
    let runtime: Bytes = "0x63deadbeef6000526004601cfd".parse().unwrap();
    let (address, _) = harness.deploy_runtime(&runtime).await;
    for method in ["eth_call", "eth_estimateGas"] {
        let result: Result<String, _> = harness
            .client
            .request(
                method,
                rpc_params![serde_json::json!({"from": alice, "to": address})],
            )
            .await;
        match result.unwrap_err() {
            jsonrpsee::core::client::Error::Call(error) => {
                assert_eq!(error.code(), 3);
                assert_eq!(error.message(), "execution reverted");
                assert_eq!(error.data().unwrap().get(), "\"0xdeadbeef\"");
            }
            error => panic!("unexpected transport error: {error}"),
        }
    }
    let runtime: Bytes = "0x5b600056".parse().unwrap();
    let (address, _) = harness.deploy_runtime(&runtime).await;
    let result: Result<String, _> = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": address, "gas": "0x7530"})],
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("execution halted"));
}

#[tokio::test]
async fn simulation_rejects_invalid_fields_and_unknown_blocks() {
    let harness = Harness::start(&[], false).await;
    let address = Address::repeat_byte(0x22);
    let too_wide = format!("{:#x}", U256::MAX);
    for request in [
        serde_json::json!({"to": address, "gas": too_wide}),
        serde_json::json!({"to": address, "gasPrice": too_wide}),
        serde_json::json!({"to": address, "nonce": too_wide}),
        serde_json::json!({"to": address, "data": "0x00", "input": "0x01"}),
    ] {
        for method in ["eth_call", "eth_estimateGas"] {
            let result: Result<String, _> = harness
                .client
                .request(method, rpc_params![request.clone()])
                .await;
            match result.unwrap_err() {
                jsonrpsee::core::client::Error::Call(error) => assert_eq!(error.code(), -32602),
                error => panic!("unexpected transport error: {error}"),
            }
        }
    }
    for method in ["eth_call", "eth_estimateGas"] {
        let result: Result<String, _> = harness
            .client
            .request(
                method,
                rpc_params![serde_json::json!({"to": address}), "0xffffff"],
            )
            .await;
        assert!(result.is_err());
    }
    let overflowing: Result<String, _> = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({"to": address, "value": too_wide})],
        )
        .await;
    assert!(overflowing.is_err());
}

#[tokio::test]
async fn estimates_respect_affordable_gas_and_pending_reservations() {
    use primitives::constants::GAS_PRICE;

    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let balance = U256::from(50_000) * U256::from(GAS_PRICE);
    let harness = Harness::start(&[(alice, balance)], false).await;
    let recipient = Address::repeat_byte(0x22);
    let request = serde_json::json!({"from": alice, "to": recipient});
    let gas: String = harness
        .client
        .request("eth_estimateGas", rpc_params![request.clone()])
        .await
        .unwrap();
    assert_eq!(
        gas, "0x5208",
        "a sender need not afford the entire block gas limit"
    );

    for nonce in 0..2 {
        let raw = sign_transfer(DEV_PRIVATE_KEY, recipient, U256::ZERO, nonce).unwrap();
        let _: String = harness
            .client
            .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
            .await
            .unwrap();
    }
    let pending: Result<String, _> = harness
        .client
        .request("eth_estimateGas", rpc_params![request.clone(), "pending"])
        .await;
    assert!(
        pending.is_err(),
        "pending reservations leave fewer than 21,000 gas affordable"
    );
    let latest: String = harness
        .client
        .request("eth_estimateGas", rpc_params![request, "latest"])
        .await
        .unwrap();
    assert_eq!(latest, "0x5208");
    let no_fee_balance: Result<String, _> = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice, "to": recipient, "value": format!("{balance:#x}")
            })],
        )
        .await;
    assert!(no_fee_balance.is_err());
    let anonymous: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "to": recipient, "value": "0xde0b6b3a7640000"
            })],
        )
        .await
        .unwrap();
    assert_eq!(anonymous, "0x5208");
}
