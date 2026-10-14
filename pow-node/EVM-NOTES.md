# Breeja EVM project notes

Working draft, started 14 October 2026. The requirements are in
[FINAL-PROJECT.md](FINAL-PROJECT.md); milestone completion must be checked against
that file. These notes combine the student's orientation answers with the code
review and verification evidence. Planned behavior below is not implemented yet.

## Schedule and working agreement

Submission is Wednesday, **21 October 2026**, GMT+1 (Africa/Lagos); the submission
hour has not been supplied. There are two hours available daily. Aim to finish
verification and the write-up on 20 October, leaving submission day as a buffer.

| Session | Scope from the brief | Evidence required before moving on |
| --- | --- | --- |
| 14 October | Part 0 and M0: orientation, branch, baseline | Four orientation answers; 48 passing tests; REVM builds; release mining output |
| 15 October | M1: code, storage, database adapter | Existing tests pass; database reads and commits tested, including unknown addresses |
| 16 October | M2: transfer parity through REVM | Exactly 21,000 gas; correct fees and nonces; mining and supply conservation pass |
| 17 October | M3: deployment, calls, logs, failed execution | Contract code committed; storage and logs persist; revert mined and charged |
| 18 October | M4: simulation and estimation | `eth_call` returns data without changing state; estimated gas actually succeeds |
| 19 October | M5: contract inspection RPC | Historical code/storage; log range/address filters; topic filters and `eth_gasPrice` targeted |
| 20 October | M6, demo, final write-up and checks | Variable block gas; storage-sensitive roots; correct bloom; Solidity demo; release build and clean clippy |
| 21 October | Submission buffer | Review final evidence and submit the PR against the fork |

Each session budgets roughly 15 minutes for explanation, 65 for implementation
with decision updates, 30 for verification, and 10 for notes and a milestone
commit. Revisit the relevant brief section before each milestone. Explain what
changes, why it is needed, and how the test proves it. Record failures and limits
honestly. Preserve existing tests; any changed expectation needs a comment
explaining why the previous expectation was wrong. Required work precedes stretch
goals. Dates are a working plan, not a claim that the milestones are complete.

## What already exists

`bin/breeja` starts genesis, the miner thread, and the JSON-RPC server.
`crates/primitives` defines accounts, blocks, transactions, receipts, and chain
state. `crates/node` validates transactions and mines blocks. `crates/rpc` exposes
eleven methods using jsonrpsee.

A submitted transfer travels through this path:

```text
signed transaction -> RPC -> pool validation -> mempool
    -> miner takes a state snapshot -> apply_transfer -> block reward
    -> proof of work -> commit block, transactions, receipts and history
```

The pool recovers the sender from the signature and checks chain ID, pending
nonce, available balance and gas. The executor debits value plus the fee,
credits the recipient and miner, and increments the nonce. Every accepted
transaction currently consumes 21,000 gas. Mining searches for a nonce whose
header hash has enough leading zero bits, links the result to the parent, and
adjusts difficulty.

The miner releases the shared `RwLock` after taking its snapshot. Execution and
proof of work happen outside the lock; committing takes a short write lock. This
structure must remain when contract execution becomes more expensive.

Accounts currently contain nonce, balance, `storage_root` and `code_hash`, but
the two hashes are placeholders. They have no code bytes or storage map.
`PendingTransaction` has neither calldata nor an optional destination, so contract
creation and calls require changes to transaction plumbing as well as execution.
Receipts have no stored contract address or logs; their RPC forms manufacture
`null`, empty logs and a zero bloom. `eth_call` and `eth_estimateGas` refuse
requests needing execution. State and history are in memory; restart persistence
and networking are outside the required scope.

## Part 0: orientation answers

### 1. Which pool checks change?

The identified checks are correct: the `TxKind::Create` rejection, the rejection
of non-empty `envelope.input()`, and `gas < TRANSFER_GAS` in `pool.rs`.

Remove the two blanket contract rejections once execution supports contracts.
Preserve the creation/call distinction and calldata in the pending transaction.
Replace the transfer-only gas check with a transaction-specific intrinsic-gas
calculation. **Variable execution gas does not remove the minimum gas requirement.**
The base is still 21,000, calldata adds 4 gas per zero byte and 16 per non-zero
byte, and creation adds 32,000. Shanghai also meters initcode words and limits
initcode size; use rules matching the chosen REVM spec. Reject a gas limit below
the calculated minimum. Existing signature, nonce and funding checks still apply.

This corrects the initial proposal to remove the third check: the floor changes
from a constant to a calculation.

### 2. Should execution failure still mean skipping the transaction?

Only a transaction that is invalid for the candidate state should be skipped,
such as one with a wrong nonce or insufficient up-front funds. A valid transaction
can start execution and then revert or halt. It is still included, consumes its
sender's nonce, charges gas, and receives `status: 0x0`.

REVM separates transaction-validity errors from `ExecutionResult::Success`,
`Revert`, and `Halt`. A revert rolls back the attempted contract changes while
preserving transaction-level gas charging and nonce advancement. Out of gas is
a halt and uses the transaction's gas limit. The executor must retain failed
execution receipts rather than treating every failure as invalid admission.

### 3. Why reserve the gas limit but charge only gas used?

`gas_limit * gas_price` is the maximum possible gas liability. Admission must
ensure the sender can cover it plus the transferred value, including liabilities
of transactions already queued. Today the executor charges only `TRANSFER_GAS`
because every accepted transaction is a plain transfer.

With REVM, gas used becomes variable. The sender must still cover the maximum
up front but pays for the actual charged gas; unused gas allowance is released.
The gas limit is a ceiling, not the final bill. REVM performs this accounting.

### 4. What do historical snapshots cost once storage exists?

`BlockState` clones the complete account map after each block. Once accounts
contain code and large storage maps, copying unchanged state becomes expensive.
With deep storage copies, memory grows roughly with the sum of all historical
state sizes, and an unchanged chain state of size S retained for B blocks costs
O(B * S). Snapshot creation also costs time inside the commit lock.

Shared immutable code bytes or a code-by-hash table can avoid duplicating bytecode;
they do not solve repeated storage-map copies. Keep the teaching snapshot model
for this project, document its limit, and test that historical storage remains
independent of later writes. A trie or versioned storage is a separate project.

## Integration decisions and acceptance checks

Use REVM 43, the default route in the brief. The existing lockfile resolves
43.0.3; inspect that installed source when checking APIs. M1 will introduce
`crates/node/src/evm.rs` around an owned snapshot, rather than implementing REVM's
database interface directly on the locked live state. `Database` answers reads
as execution requests them; `DatabaseCommit` persists the returned changes.
Finalize the code/storage representation during M1 and explain the trade-off.

Use Shanghai execution with Breeja's teaching proof of work and 5 ETH reward.
This deliberate mixture is not a historical Ethereum network configuration.
Compile the Solidity demo explicitly for Shanghai so its target matches the VM.
Set base fee to zero to preserve the no-burn supply model. Set beneficiary to
the miner: REVM pays transaction fees and advances sender nonces, so remove the
old manual fee credit and nonce increment when replacing `apply_transfer`.
Continue paying the block reward ourselves. Use `tx_gas_used()`.

Build execution's block environment before running transactions: number,
timestamp, gas limit, difficulty, miner and parent hash must agree with the header
later mined. Use the parent hash as `prevrandao`; the candidate's own hash cannot
exist before execution and mining finish. Supply historical block hashes to the
database for `BLOCKHASH`.

M4 must discard simulation changes and document validation choices. Gas
estimation must probe copied state and return an estimate tested by sending a
transaction with that exact limit. Binary search is a heuristic for contracts
whose behavior depends on remaining gas.

M6 must select transactions by gas, update storage/code commitments and construct
blooms from actual logs. Receipt `to` is null for creation, and logs need block
and transaction metadata for RPC filtering. Preserve Ethereum's distinction
between minimal hex quantities and fixed-width data: storage results are 32 bytes.

All 14 entries in the brief's testing requirements are tracked by their milestone
above, including existing supply conservation. The M6 acceptance checks additionally
require storage-sensitive roots and bloom verification. Final deliverables are
the `feat/evm` branch, passing old and new tests, release build, warning-free clippy,
a Solidity deployment/call demo, this 2–4 page write-up revised to reflect what
actually shipped, and a PR against the fork.

## M0 evidence and current status

- Starting commit: `bbbdf34` (`sync rename`). Branch created: `feat/evm`.
- Pre-existing uncommitted changes: REVM dependency in `crates/node/Cargo.toml`
  and the corresponding `Cargo.lock` updates. These were preserved.
- Rust compiler: 1.95.0; Cargo: 1.95.0. REVM 43.0.3 is available locally and
  compiles with the workspace.
- `cargo test --workspace --locked --offline`: **48 passed**, none ignored
  (13 node unit tests, 8 mining tests, 14 primitives tests, 13 RPC tests).
  The initial sandbox run could not bind RPC sockets; rerunning with local
  socket access passed all tests without changing code or assertions.
- `cargo build --release --locked --offline`: passed (initial release compilation
  took 2m 10s).
- `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`:
  passed with no warnings.
- Release mining smoke test: passed; 20 empty blocks sealed, difficulty adjusted,
  the RPC server started with 11 methods, and the miner shut down cleanly.
  An ephemeral localhost port avoids occupying a development node's usual 8545.
  The environment sets `RUST_LOG=warn`, so `info` was explicitly enabled to
  capture evidence. The command uses a five-second interrupt; `timeout` returns
  124 by design, even when the node handles the interrupt and exits cleanly.

```bash
RUST_LOG=info timeout --signal=INT --kill-after=5s 5s \
  cargo run --release --locked --offline -- --dev --rpc-address 127.0.0.1:0
```

Selected output from the run (timestamps as printed, in UTC):

```text
2026-10-14T13:05:05.916295Z  INFO genesis block sealed genesis_hash=0xcd766ee455099bf23b095ee57164331abc356e93bb7757cd33d50ac109bc1434 accounts=4
2026-10-14T13:05:05.916939Z  INFO JSON-RPC server listening address=127.0.0.1:36345 methods=11
2026-10-14T13:05:05.916962Z  INFO sealed block number=1 hash=0x1e48396bc43b97ea8c3045e4c806c93e90ecdb4bfa0f90278d87a0cead638b85 txs=0 difficulty=3 nonce=2 attempts=3 hash_rate=358938 H/s took=0.00s
2026-10-14T13:05:05.917006Z  INFO sealed block number=2 hash=0x023ce2b8c284ea47957ab28021dfb1ca0a2926d00baf9af676ce6cb3c3bc2e1f txs=0 difficulty=4 nonce=7 attempts=8 hash_rate=400661 H/s took=0.00s
2026-10-14T13:05:05.917090Z  INFO sealed block number=3 hash=0x02022fb3ddc7cf88395e143003e679ce70a6821af5f3cdacb319ea274bbe224d txs=0 difficulty=5 nonce=25 attempts=26 hash_rate=415117 H/s took=0.00s
2026-10-14T13:05:09.862082Z  INFO shutting down
2026-10-14T13:05:09.894983Z  INFO miner stopped
```

Part 0 and M0 acceptance checks are complete; this document and the existing REVM
dependency changes form the M0 checkpoint.
M1–M6 are pending; no contract execution capabilities have been implemented yet.
