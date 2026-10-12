# Assignment: Complete the JSON-RPC Server

## Goal

Complete the JSON-RPC server so that it correctly handles the required Ethereum JSON-RPC methods.

For this assignment, **you do not need to implement real blockchain functionality yet**.

When the server receives a valid JSON-RPC request, it should:

1. Parse the request.
2. Identify the requested RPC method.
3. Route the request to the appropriate handler.
4. Return a valid JSON-RPC response containing the **dummy response data** provided by the starter code.

The main goal is to understand how an Ethereum-style JSON-RPC server receives requests and maps them to RPC methods.

## Required Crate

The JSON-RPC server **must be implemented using the [`jsonrpsee`](https://crates.io/crates/jsonrpsee) Rust crate**.

Do not implement your own JSON-RPC server, request parser, or method-routing system.

You should use `jsonrpsee` to:

* Create and run the JSON-RPC HTTP server.
* Register the required RPC methods.
* Handle incoming RPC requests.
* Return JSON-RPC responses and errors.

The purpose of this requirement is to become familiar with how a production-quality Rust JSON-RPC server is structured using an existing library.

## RPC Methods to Implement

You are responsible for implementing the following methods:

| RPC Method                  | Purpose                                                            |
| --------------------------- | ------------------------------------------------------------------ |
| `eth_chainId`               | Returns the chain ID of the blockchain.                            |
| `eth_blockNumber`           | Returns the current block number.                                  |
| `eth_getBlockByNumber`      | Returns a block using its block number.                            |
| `eth_getBlockByHash`        | Returns a block using its block hash.                              |
| `eth_getTransactionByHash`  | Returns a transaction using its transaction hash.                  |
| `eth_getTransactionReceipt` | Returns the receipt for a transaction.                             |
| `eth_getBalance`            | Returns the balance of an account.                                 |
| `eth_getTransactionCount`   | Returns the transaction count (nonce) of an account.               |
| `eth_sendRawTransaction`    | Accepts a signed raw transaction and returns its transaction hash. |
| `eth_call`                  | Executes a read-only EVM call.                                     |
| `eth_estimateGas`           | Estimates the gas required for a transaction.                      |

## Requirements

Your implementation must:

* Be written in **Rust**.
* Use the **`jsonrpsee` crate** for the JSON-RPC server.
* Accept JSON-RPC requests over HTTP.
* Register all of the required RPC methods with `jsonrpsee`.
* Correctly parse the incoming request parameters using `jsonrpsee`.
* Dispatch each supported method to the appropriate handler.
* Return a valid JSON-RPC response.
* Preserve the request's `id` in the response.
* Return the dummy response associated with each method.
* Return an appropriate JSON-RPC error when an unsupported method is requested.

### Important

You are **only implementing the JSON-RPC interface**.

The RPC methods do not need to perform real blockchain operations yet. The handlers should return the **dummy values provided by the starter code**.

For example, a request such as:

```json
{
  "jsonrpc": "2.0",
  "method": "eth_chainId",
  "params": [],
  "id": 1
}
```

should produce a response in the form:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": "0x1"
}
```

The exact dummy values to return will be provided in the starter code.

## What You Are Not Required to Implement

At this stage, you are **not** required to implement:

* A real blockchain database
* Transaction execution
* Consensus
* Block production
* Transaction signing
* A real EVM
* Real account state
* A mempool
* Networking between blockchain nodes
* Real Ethereum state management

We are focusing only on the **JSON-RPC layer**. We would be handling the rest in class.

## Learning Objective

By completing this assignment, you should understand how an Ethereum-style JSON-RPC interface is built in Rust and how external applications communicate with a blockchain node.

The basic architecture is:

```text
Ethereum Client / Wallet / dApp
             │
             │ JSON-RPC Request
             ▼
      ┌─────────────────┐
      │   jsonrpsee     │
      │    RPC Server   │
      ├─────────────────┤
      │ Method Routing  │
      │ RPC Handlers    │
      └─────────────────┘
             │
             │ JSON-RPC Response
             ▼
           Client
```

Each Ethereum RPC method is an API entry point into the node.

For example:

```text
eth_blockNumber
      │
      ▼
RPC Handler
      │
      ▼
Dummy Block Number
      │
      ▼
JSON-RPC Response
```

## Testing

You should be able to test your implementation using tools such as `curl`, Postman, or any JSON-RPC client.

Example:

```bash
curl -X POST http://localhost:8545 \
  -H "Content-Type: application/json" \
  --data '{
    "jsonrpc": "2.0",
    "method": "eth_blockNumber",
    "params": [],
    "id": 1
  }'
```

The server should return a valid JSON-RPC response for the requested method.

## Submission

Your implementation should:

* Compile successfully.
* Start the JSON-RPC server successfully.
* Use `jsonrpsee` for the server implementation.
* Register all required RPC methods.
* Respond correctly to all required RPC methods.
* Return valid JSON-RPC responses.
* Handle unsupported RPC methods with an appropriate error response.
* Be testable using an HTTP JSON-RPC client such as `curl`.

The objective is not to build a full Ethereum node yet. The objective is to build the **RPC interface that a future Ethereum node can expose**.
