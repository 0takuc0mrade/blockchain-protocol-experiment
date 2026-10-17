# Breeja — a proof-of-work Ethereum teaching node with REVM

Breeja is a teaching implementation of an Ethereum execution node from the
proof-of-work era, written in Rust. It mines blocks, validates signed
transactions, executes contracts with REVM, maintains account state, and serves
the Ethereum JSON-RPC API.

Breeja began with transfers and no virtual machine. The final project integrates
REVM into that existing node, preserving its proof of work and account/signature
plumbing. It deliberately combines Shanghai execution with teaching consensus
rules, including a 5 ETH block reward; this is not a historical Ethereum network.

```text
                   eth_sendRawTransaction
   wallet / cast ──────────────────────────┐
                                           ▼
                                    ┌─────────────┐
                                    │  JSON-RPC   │  crates/rpc
                                    │  (jsonrpsee)│
                                    └──────┬──────┘
                                           │ validate + queue
                                           ▼
                                    ┌─────────────┐
                                    │   mempool   │  crates/node/pool.rs
                                    └──────┬──────┘
                                           │
                                           ▼
                 ┌──────────────────────────────────────────────┐
                 │  miner loop            crates/node/miner.rs  │
                 │                                              │
                 │  1. snapshot state and mempool               │
                 │  2. execute with REVM      → executor.rs     │
                 │  3. hash until the nonce works → pow.rs      │
                 │  4. commit the sealed block                  │
                 │  5. retarget difficulty                      │
                 └──────────────────────┬───────────────────────┘
                                        ▼
                                 ┌─────────────┐
                                 │ chain state │  crates/primitives/node.rs
                                 └─────────────┘
```

## Running it

```bash
cargo run --release -- --dev
```

`--dev` starts at a low difficulty and targets 2-second blocks. Release mode
matters: a debug build hashes roughly twenty times slower.

The node prints each block as it is sealed, including how many hashes the proof of
work actually cost:

```text
INFO sealed block number=8 hash=0x000000ab64b3a5… txs=1 difficulty=23
     nonce=1446633 attempts=1446634 hash_rate=1509579 H/s took=0.96s
```

Options:

| Flag | Meaning |
| --- | --- |
| `--dev` | Low starting difficulty, 2-second target blocks |
| `--rpc-address <ADDR>` | JSON-RPC bind address (default `127.0.0.1:8545`) |
| `--miner <ADDRESS>` | Who receives block rewards and fees |
| `--difficulty <BITS>` | Starting difficulty, in leading zero bits |
| `--target-block-time <SECS>` | Block time the miner retargets towards |

Set `RUST_LOG=debug` for more detail.

## Pre-funded accounts

Genesis funds four accounts with 10,000 ETH each. These are the standard
Anvil/Hardhat development accounts — **their private keys are public knowledge and
must never hold real funds.**

| Address | Private key |
| --- | --- |
| `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` | `0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80` |
| `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` | `0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d` |
| `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` | `0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a` |
| `0x90F79bf6EB2c4f870365E785982E1f101E93b906` | `0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6` |

## Sending a transaction

Breeja exposes `eth_gasPrice` and `eth_estimateGas`, so `cast send` can fill the
price, gas limit, and nonce. With the node running:

```bash
cast send --legacy --rpc-url http://127.0.0.1:8545 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --value 1ether 0x70997970C51812dc3A010C7d01b50e0d17dc79C8
```

`--legacy` is required. Breeja predates EIP-1559 and rejects typed transactions,
because there is no base fee for one to bid against.

Then poll for the receipt:

```bash
curl -s -X POST http://localhost:8545 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_getTransactionReceipt","params":["<TX_HASH>"],"id":1}'
```

A `null` receipt means "not mined yet" — which is exactly how every Ethereum
client waits for confirmation.

## The JSON-RPC API

The original eleven methods plus the four inspection/tooling methods from M5.

| Method | Behaviour |
| --- | --- |
| `eth_chainId` | `0x1` |
| `eth_blockNumber` | Height of the chain head |
| `eth_getBlockByNumber` | By number or tag (`latest`, `earliest`, `pending`, `safe`, `finalized`) |
| `eth_getBlockByHash` | By hash; `null` if unknown |
| `eth_getTransactionByHash` | Mined transactions; `null` if unknown |
| `eth_getTransactionReceipt` | Receipts for mined transactions; `null` while pending |
| `eth_getBalance` | Balance at any historical block, or `pending` |
| `eth_getTransactionCount` | Nonce; `pending` includes queued transactions |
| `eth_sendRawTransaction` | Validates and queues a signed legacy transfer, deployment, or call |
| `eth_call` | Executes against the selected state and returns bytes without committing changes |
| `eth_estimateGas` | Searches copied state for a sufficient gas limit; `0x5208` for an ordinary transfer |
| `eth_gasPrice` | Fixed legacy gas price, 1 gwei |
| `eth_getCode` | Runtime bytecode at the selected block, or `0x` |
| `eth_getStorageAt` | A slot at the selected block, always 32 bytes, zero if missing |
| `eth_getLogs` | Mined logs in an inclusive block range, filtered by address(es) and positional topics |

Anything else returns `-32601 Method not found`.

## Contracts and simulation

M0–M6 are implemented. A legacy transaction with no `to` deploys its initcode;
contract calls carry calldata. Receipts report `contractAddress`, logs and actual
gas. Reverts and out-of-gas executions are mined with `status: "0x0"`: the nonce
advances and gas is charged, while the attempted contract changes roll back.

`eth_call` accepts hypothetical unfunded or contract callers and does not require
a valid sender nonce. Its default gas price is zero. `eth_estimateGas` defaults
to 1 gwei and limits the search by a supplied sender's available funds, the block
limit and any supplied gas cap. Reverts return JSON-RPC error code 3 with raw
revert data. `data` and `input` are aliases and must match if both are provided.

Historical tags select historical state and block fields. `pending` simulation
uses the committed head, with queued spending reserved for estimation; it does
not execute a pending-state overlay. Gas estimates are sufficient for the probed
snapshot, and can change if state or transaction parameters change. Binary search
is a heuristic for contracts whose behavior depends on remaining gas.

The real-miner and real-RPC integration tests exercise these paths:

```bash
cargo test -p node --test contracts
cargo test -p rpc --test json_rpc
```

`eth_getLogs` defaults both range bounds to `latest`. Addresses and alternatives
within a topic position are ORed; different positions are ANDed, and `null`
matches any topic at that position. Empty alternative arrays are wildcards.
`blockHash` selects one block and cannot be combined with range bounds. Future
or reversed ranges are errors. Results retain block/transaction/log order.
These semantics follow the [Ethereum JSON-RPC specification](https://ethereum.org/en/developers/docs/apis/json-rpc/#eth_getlogs).

Blocks accumulate actual gas usage. Each transaction's declared limit must fit
the remaining budget before execution; transactions that cannot fit stay queued,
and smaller later candidates can be included. Admission checks Shanghai intrinsic
gas, including calldata and initcode. Receipt and block `logsBloom` values contain
the real address/topic bits; reverted logs contribute nothing.

State roots commit to nonce, balance, runtime code, and sorted non-zero storage
slots. The storage commitment hashes concatenated 32-byte slot/value pairs;
the account commitment retains four-field RLP. These are deterministic teaching
commitments, without Ethereum trie proofs. The original PoW header structure is
preserved; block bloom is derived from receipts for RPC. Block responses also
provide the empty ommer hash/list and the parent-hash `mixHash` convention so
standard clients can parse them.

## Solidity demo

With the node running, install Foundry (`forge` and `cast`) and `jq`, then run:

```bash
bash scripts/evm-demo.sh
# For a different local port:
RPC_URL=http://127.0.0.1:18545 bash scripts/evm-demo.sh
```

The script compiles [Counter.sol](./examples/evm-demo/src/Counter.sol) with Solidity
0.8.24, optimization at 200 runs, and **Shanghai** as configured in
[foundry.toml](./examples/evm-demo/foundry.toml). It checks the compiled fixtures,
deploys the contract, sets and reads 42, checks slot zero and its event, and proves
a simulated write leaves storage unchanged. It uses the public development key
above and lets `cast send` fill gas price, gas limit and nonce. The script expects
the development account to be funded. Forge downloads the pinned compiler if it
is not installed.

The compiled creation/runtime fixtures are checked in so the same Solidity
contract is exercised by the ordinary offline Rust test suite. To regenerate
them after intentionally changing the source or compiler settings:

```bash
forge build --root examples/evm-demo
jq -r '.bytecode.object' examples/evm-demo/out/Counter.sol/Counter.json > examples/evm-demo/Counter.init.hex
jq -r '.deployedBytecode.object' examples/evm-demo/out/Counter.sol/Counter.json > examples/evm-demo/Counter.runtime.hex
```

That RPC test deploys and calls the Solidity contract with exactly estimated
gas, checks event topics/data, mines its reverting function, and checks that
`eth_call` preserves state. Run all project checks with:

```bash
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo build --release --locked --offline
```

## Reading the implementation

Use [FINAL-PROJECT.md](./FINAL-PROJECT.md) as the requirements and
[EVM-NOTES.md](./EVM-NOTES.md) for the decisions and verification evidence.
Read `primitives/src/node.rs` for state/snapshots/roots, `node/src/evm.rs` for the
database adapter, `node/src/executor.rs` for execution/accounting, `node/src/pool.rs`
and `miner.rs` for admission and mining, then `rpc/src/handlers.rs` for RPC behavior.
These paths are under `crates/`. `primitives/src/params.rs` and `response.rs` define
filter matching and wire formats. The corresponding unit tests plus
`node/tests/contracts.rs`, `node/tests/mining.rs`, and `rpc/tests/json_rpc.rs`
show the behaviors required by the brief.

State is in memory: restarting loses contracts and blocks. Full historical
snapshots, whole-pool cloning, recomputing storage commitments, and linear log
scans suit small teaching chains. Transaction/receipt roots retain the starter's
hash-list scheme (the receipt list commits only to transaction hashes), and
block sizes remain estimates. There is no Ethereum consensus/header compatibility,
disk persistence, state proof API, reorg handling, or pending execution overlay.

## Chain parameters

Teaching consensus parameters in `crates/primitives/src/constants.rs`, with
Shanghai execution rules:

| Parameter | Value | Note |
| --- | --- | --- |
| Chain ID | 1 | |
| Gas price | 1 gwei, fixed | No fee market; no EIP-1559 |
| Transfer cost | 21,000 gas | Ordinary ETH transfer; contract gas varies |
| Block gas limit | 3,141,592 | Frontier's "pi million" — 149 transfers per block |
| Block reward | 5 ETH | As on Frontier and Homestead |
| Target block time | 15s (2s with `--dev`) | |


## Course documents

- [ASSIGNMENT.md](./ASSIGNMENT.md) — the JSON-RPC exercise this node grew out of.
- [FINAL-PROJECT.md](./FINAL-PROJECT.md) — the final project: give Breeja an EVM,
  using [REVM](https://github.com/bluealloy/revm).
