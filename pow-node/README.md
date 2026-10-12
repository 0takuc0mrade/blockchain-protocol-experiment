# Breeja — a proof-of-work Ethereum node with no EVM

Breeja is a teaching implementation of an Ethereum execution node from the
proof-of-work era, written in Rust. It mines blocks, validates signed
transactions, maintains account state, and serves the Ethereum JSON-RPC API.

That omission is the point of the exercise. Ethereum is usually taught EVM-first,
as though the virtual machine were the whole system. Take the EVM out and what
remains is still recognisably Ethereum: accounts, nonces, gas, signatures, a
mempool, proof of work, a difficulty target, blocks linked by hash, and it fits
in a few hundred lines you can read in an afternoon.

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
                 │  2. execute transfers      → executor.rs     │
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

Breeja does not implement `eth_gasPrice`, so `cast send` cannot auto-fill its
parameters. Build and sign the transaction offline with `cast mktx`, then submit
the raw bytes.

```bash
RAW=$(cast mktx --legacy \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --nonce 0 --gas-price 1000000000 --gas-limit 21000 --chain 1 \
  --value 1ether 0x70997970C51812dc3A010C7d01b50e0d17dc79C8)

curl -s -X POST http://localhost:8545 -H 'Content-Type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendRawTransaction\",\"params\":[\"$RAW\"],\"id\":1}"
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

The same eleven methods from the assignment, now backed by real state.

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
| `eth_sendRawTransaction` | Validates and queues a signed transfer |
| `eth_call` | `0x` for a plain transfer; **error** if it needs the EVM |
| `eth_estimateGas` | `0x5208` (21000); **error** if it needs the EVM |

Anything else returns `-32601 Method not found`.

## Chain parameters

Deliberately Frontier-era, in `crates/primitives/src/constants.rs`:

| Parameter | Value | Note |
| --- | --- | --- |
| Chain ID | 1 | |
| Gas price | 1 gwei, fixed | No fee market; no EIP-1559 |
| Transfer cost | 21,000 gas | The only cost there is |
| Block gas limit | 3,141,592 | Frontier's "pi million" — 149 transfers per block |
| Block reward | 5 ETH | As on Frontier and Homestead |
| Target block time | 15s (2s with `--dev`) | |


## Course documents

- [ASSIGNMENT.md](./ASSIGNMENT.md) — the JSON-RPC exercise this node grew out of.
- [FINAL-PROJECT.md](./FINAL-PROJECT.md) — the final project: give Breeja an EVM,
  using [REVM](https://github.com/bluealloy/revm).
