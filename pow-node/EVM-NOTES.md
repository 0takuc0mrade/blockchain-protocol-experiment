# Breeja EVM project notes

I started this working draft on 14 October 2026 to record my understanding of
Breeja, my integration decisions, and the evidence behind them. I am following
[FINAL-PROJECT.md](FINAL-PROJECT.md) and checking each milestone against its
requirements. My implementation plans below are still pending unless I explicitly
record a completed check.

## My starting point

I traced the existing node from its entry point through transaction validation,
execution, mining, and RPC responses. `bin/breeja` starts genesis, the miner thread,
and the JSON-RPC server. `crates/primitives` defines accounts, blocks, transactions, receipts, and chain
state. `crates/node` validates transactions and mines blocks. `crates/rpc` exposes
eleven methods using jsonrpsee.

I understand the current transfer path as:

```text
signed transaction -> RPC -> pool validation -> mempool
    -> miner takes a state snapshot -> apply_transfer -> block reward
    -> proof of work -> commit block, transactions, receipts and history
```

In the pool, I found that the sender is recovered from the signature, with checks
for chain ID, pending nonce, available balance and gas. The executor debits value plus the fee,
credits the recipient and miner, and increments the nonce. Every accepted
transaction currently consumes 21,000 gas. Mining searches for a nonce whose
header hash has enough leading zero bits, links the result to the parent, and
adjusts difficulty.

I need to preserve the miner's locking structure. It releases the shared `RwLock`
after taking its snapshot. Execution and proof of work happen outside the lock;
committing takes a short write lock. Holding the lock through contract execution
would block RPC requests for the duration of that work.

In the starting `Account`, I found nonce, balance, `storage_root` and `code_hash`,
but the two hashes were placeholders. Accounts had no code bytes or storage map.
`PendingTransaction` has neither calldata nor an optional destination, so contract
creation and calls require changes to transaction plumbing as well as execution.
Receipts have no stored contract address or logs; their RPC forms manufacture
`null`, empty logs and a zero bloom. `eth_call` and `eth_estimateGas` refuse
requests needing execution. I also found that state and history are in memory;
I am keeping restart persistence and networking outside this project's required scope.

## Part 0: orientation answers

### 1. Which pool checks do I need to change?

I identified three checks: the `TxKind::Create` rejection, the rejection
of non-empty `envelope.input()`, and `gas < TRANSFER_GAS` in `pool.rs`.

I will remove the two blanket contract rejections once execution supports contracts.
I will preserve the creation/call distinction and calldata in the pending transaction,
and replace the transfer-only gas check with a transaction-specific intrinsic-gas
calculation. **Variable execution gas does not remove the minimum gas requirement.**
The base is still 21,000, calldata adds 4 gas per zero byte and 16 per non-zero
byte, and creation adds 32,000. Shanghai also meters initcode words and limits
initcode size, so I need rules matching the chosen REVM spec. I will reject a gas
limit below the calculated minimum and retain signature, nonce and funding checks.

I initially thought I could remove the third check because gas becomes variable.
I now understand that the floor changes from a constant to a calculation.

### 2. Should execution failure still mean skipping the transaction?

I need to distinguish an invalid transaction from a valid transaction whose
execution fails. Only a transaction that is invalid for the candidate state should
be skipped, such as one with a wrong nonce or insufficient up-front funds. A valid transaction
can start execution and then revert or halt. It is still included, consumes its
sender's nonce, charges gas, and receives `status: 0x0`.

REVM separates transaction-validity errors from `ExecutionResult::Success`,
`Revert`, and `Halt`. A revert rolls back the attempted contract changes while
preserving transaction-level gas charging and nonce advancement. Out of gas is
a halt and uses the transaction's gas limit. I will retain failed execution
receipts rather than treating every failure as invalid admission.

### 3. Why reserve the gas limit but charge only gas used?

My understanding is that `gas_limit * gas_price` is the maximum possible gas
liability. Admission must ensure the sender can cover it plus the transferred value, including liabilities
of transactions already queued. Today the executor charges only `TRANSFER_GAS`
because every accepted transaction is a plain transfer.

When I integrate REVM, gas used becomes variable. The sender must still cover the
maximum up front but pays for the actual charged gas; unused gas allowance is released.
The gas limit is a ceiling, not the final bill. REVM performs this accounting.

### 4. What do historical snapshots cost once storage exists?

I found that `BlockState` clones the complete account map after each block. Once
accounts contain code and large storage maps, copying unchanged state becomes expensive.
With deep storage copies, memory grows roughly with the sum of all historical
state sizes, and an unchanged chain state of size S retained for B blocks costs
O(B * S). Snapshot creation also costs time inside the commit lock.

Shared immutable code bytes or a code-by-hash table can avoid duplicating bytecode;
they do not solve repeated storage-map copies. I plan to keep the teaching snapshot
model for this project, document its limit, and test that historical storage
remains independent of later writes. I am leaving a trie or versioned storage
for a separate project.

## Integration decisions and acceptance checks

I am taking the brief's default route, REVM 43. The existing lockfile resolves
43.0.3, so I check APIs against that installed source. In M1, I introduced
`crates/node/src/evm.rs` around an owned snapshot, keeping REVM's database interface
away from locked live state. `Database` answers reads as execution requests them;
`DatabaseCommit` persists the returned changes in the working snapshot. My M1
decisions below explain the code/storage representation.

I configured Shanghai execution with Breeja's teaching proof of work and 5 ETH reward.
I understand that this deliberate mixture is not a historical Ethereum network
configuration. I will compile the Solidity demo explicitly for Shanghai so its
target matches the VM. I set base fee to zero to preserve the no-burn supply model,
and beneficiary to the miner. REVM pays transaction fees and advances sender
nonces, so I removed the old manual fee credit and nonce increment when replacing
`apply_transfer`. I kept the node's block reward payment and use `tx_gas_used()`.

I build execution's block environment before running transactions: number,
timestamp, gas limit, difficulty, miner and parent hash must agree with the header
later mined. I use the parent hash as `prevrandao`; the candidate's own hash cannot
exist before execution and mining finish. I also supply historical block hashes
to the database for `BLOCKHASH`.

In M4, I will discard simulation changes and document my validation choices.
For gas estimation, I will probe copied state and test the estimate by sending a
transaction with that exact limit. I understand that binary search is a heuristic
for contracts whose behavior depends on remaining gas.

In M6, I will select transactions by gas, update storage/code commitments and
construct blooms from actual logs. I need receipt `to` to be null for creation,
and logs to include block and transaction metadata for RPC filtering. I will
preserve Ethereum's distinction between minimal hex quantities and fixed-width
data: storage results are 32 bytes.

I will verify all 14 entries in the brief's testing requirements, including
existing supply conservation, and the additional M6 checks for storage-sensitive
roots and blooms. My final deliverables are the `feat/evm` branch, passing old and
new tests, a release build, warning-free clippy, a Solidity deployment/call demo,
this 2–4 page write-up revised to reflect what I actually shipped, and a PR
against my fork.

## M0 evidence and current status

My baseline verification established the following:

- I started from `bbbdf34` (`sync rename`), with `feat/evm` as my project branch.
- I retained the REVM dependency already added in `crates/node/Cargo.toml`
  and the corresponding `Cargo.lock` updates.
- My toolchain is Rust 1.95.0 and Cargo 1.95.0. REVM 43.0.3 is available locally
  and compiles with the workspace.
- My `cargo test --workspace --locked --offline` baseline is **48 passed**, none ignored
  (13 node unit tests, 8 mining tests, 14 primitives tests, 13 RPC tests).
  My initial sandbox run could not bind RPC sockets; the rerun with local
  socket access passed all tests without changes to code or assertions.
- My `cargo build --release --locked --offline` check passed (initial release compilation
  took 2m 10s).
- My `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`
  check passed with no warnings.
- My release mining smoke test passed: 20 empty blocks sealed, difficulty adjusted,
  the RPC server started with 11 methods, and the miner shut down cleanly.
  I used an ephemeral localhost port to avoid occupying a development node's usual 8545.
  My environment sets `RUST_LOG=warn`, so the run explicitly enabled `info` to
  capture evidence. My command uses a five-second interrupt; `timeout` returns
  124 by design, even when the node handles the interrupt and exits cleanly.

```bash
RUST_LOG=info timeout --signal=INT --kill-after=5s 5s \
  cargo run --release --locked --offline -- --dev --rpc-address 127.0.0.1:0
```

I recorded the following output from the run (timestamps as printed, in UTC):

```text
2026-10-14T13:05:05.916295Z  INFO genesis block sealed genesis_hash=0xcd766ee455099bf23b095ee57164331abc356e93bb7757cd33d50ac109bc1434 accounts=4
2026-10-14T13:05:05.916939Z  INFO JSON-RPC server listening address=127.0.0.1:36345 methods=11
2026-10-14T13:05:05.916962Z  INFO sealed block number=1 hash=0x1e48396bc43b97ea8c3045e4c806c93e90ecdb4bfa0f90278d87a0cead638b85 txs=0 difficulty=3 nonce=2 attempts=3 hash_rate=358938 H/s took=0.00s
2026-10-14T13:05:05.917006Z  INFO sealed block number=2 hash=0x023ce2b8c284ea47957ab28021dfb1ca0a2926d00baf9af676ce6cb3c3bc2e1f txs=0 difficulty=4 nonce=7 attempts=8 hash_rate=400661 H/s took=0.00s
2026-10-14T13:05:05.917090Z  INFO sealed block number=3 hash=0x02022fb3ddc7cf88395e143003e679ce70a6821af5f3cdacb319ea274bbe224d txs=0 difficulty=5 nonce=25 attempts=26 hash_rate=415117 H/s took=0.00s
2026-10-14T13:05:09.862082Z  INFO shutting down
2026-10-14T13:05:09.894983Z  INFO miner stopped
```

I have completed the Part 0 and M0 acceptance checks. My M0 checkpoint is
`813e188` (`chore(evm): record M0 baseline and orientation`), containing these
notes and the existing REVM dependency changes.

## M1: code, storage and the database adapter

I extended `Account` with runtime `code: Bytes` and `storage: HashMap<U256, U256>`.
I chose raw bytes to keep the primitives crate independent of REVM. Snapshot
clones share immutable byte allocations but copy storage maps, so later writes
cannot change historical values. I added current and historical code/storage
accessors and a snapshot of the last 256 block hashes for `BLOCKHASH`.

I kept the existing four-field account RLP: nonce, balance, storage root and code
hash. I excluded the raw payloads with `#[rlp(skip)]`, because their commitments
belong in the hash fields. `set_code` installs bytes and their hash together.
Storage-root calculation is still pending M6; at this stage, a storage-only
change does not yet affect the state root. I have not weakened or changed any
existing test expectations.

I implemented `BreejaDb` in `crates/node/src/evm.rs`. Its code-by-hash cache analyzes
identical bytecode once per snapshot; this is not a global deduplicated code store.
`basic` returns balance, nonce and the hash derived from the actual code, leaving
REVM to load code on demand through `code_by_hash`. Deriving that hash also gives
REVM the correct empty-code hash while preserving Breeja's existing placeholder
hashes in accounts that have not changed. I return `None` for an unknown address
without inserting it, and zero for a missing storage slot.

I commit only touched accounts, remove accounts flagged as selfdestructed, clear
old storage for newly created accounts, and remove slots written to zero. I preserve
existing code when REVM has not loaded it. REVM's account record also contains
execution flags and original/current storage values; I persist the current values
in Breeja's simpler account type. These changes affect only the owned working
snapshot; the miner still controls when a sealed block reaches live state.

My nine adapter tests cover funded and unknown accounts, code/storage reads,
block hashes, complete commits, untouched accounts, deletion, new-account storage
and zero writes. My two additional primitives tests cover missing values and
historical snapshot isolation. My M1 checks passed:

- `cargo test --workspace --locked --offline`: **59 passed**, including all 48
  original tests and 11 new tests; none ignored.
- `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`:
  no warnings.
- `cargo build --release --locked --offline`: passed.
- Formatting checks for the changed Rust files and `git diff --check`: passed.

I have completed all M1 acceptance checks in `FINAL-PROJECT.md`.

My M1 checkpoint is `6a85d48` (`feat(evm): add M1 code and storage snapshot database`).

## M2: transfers through REVM

I wrote the transfer parity test before implementing `execute_transaction` in
`executor.rs`. It now proves that a plain transfer uses exactly **21,000 gas**,
moves the expected value, pays the beneficiary exactly once, and advances the
sender's nonce exactly once. Total ETH is conserved across the transaction.

I replaced `apply_transfer` with REVM execution over `BreejaDb`. I map the pending
transaction's sender, destination, value, nonce, gas limit, gas price and chain ID
into a legacy `TxEnv`. I map the candidate block into `BlockEnv`, including its
miner as beneficiary and parent hash as `prevrandao`. The pool still admits only
transfers, so calldata remains empty for this milestone.

I take actual gas from `tx_gas_used()` and receipt status from `is_success()`.
Validity errors return without committing snapshot changes. For an execution
outcome, I drop the EVM to release its database borrow, then commit the returned
state. I removed manual transfer debits, recipient/fee credits and nonce bumps;
REVM performs them. I retained the separate block reward after the transactions.

I changed the miner to create its candidate block before execution so EVM-visible
fields match the header later mined. Account and recent-block-hash snapshots are
taken under the read lock; database construction, execution and proof of work
happen after releasing it. Execution fills the candidate's roots and gas usage,
then proof of work supplies its nonce and hash. The existing seal verification,
difficulty adjustment and brief write-lock commit remain intact.

I adapted the seven existing executor tests to the database/block signature,
with a code comment explaining the setup change. I kept their economic expectations
and the mining supply-conservation assertion unchanged. My six new tests cover
parity, unused gas, up-front affordability, per-transaction block gas validation,
chain ID validation and a sender who is also the miner. One important distinction:
enough ETH for the eventual 21,000-gas bill is insufficient if the sender cannot
cover the transaction's larger declared gas limit up front.

My M2 verification passed:

- `cargo test --workspace --locked --offline`: **65 passed**, including all 8
  mining integration tests and all 13 real-server RPC tests; none ignored.
- `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`:
  no warnings.
- `cargo build --release --locked --offline`: passed.
- A fresh `cargo run --release --locked --offline -- --dev` node, with an ephemeral
  RPC port and a separate miner address, accepted the exact `cast mktx` transaction
  arguments from the README. The node shut down cleanly afterward.

I recorded this live transfer evidence:

| Check | Result |
| --- | --- |
| Transaction hash | `0x22d76a11db763d4b5cc16135b4d8ee08acdce86bb84fb87f4a9fd353176bba26` |
| Inclusion | Block 12, status `0x1` |
| Gas / fee | 21,000 gas / 21,000,000,000,000 wei |
| Sender nonce | 1 |
| Sender balance | 9,998.999979 ETH |
| Recipient balance | 10,001 ETH |
| Supply at block 14 | Exactly 40,000 ETH genesis + 14 × 5 ETH rewards |

I completed M2 at checkpoint `94474fc`. The remaining work at that point was
contracts, RPC simulation and inspection, and the final storage-root/bloom checks.

## M3: contract deployment, calls and failed execution

I changed transaction destinations to `Option<Address>`: `None` means creation,
while `Some(address)` means a call or transfer. I carry initcode/calldata from
signature decoding through the mempool into REVM and the mined transaction.
Receipts now retain the created address and logs, including address, topics,
data and block/transaction/log positions. I stamp each log's block hash after
mining and render creation destinations as JSON `null`. I also corrected the
receipt's JSON spelling to `cumulativeGasUsed`.

I removed the blanket creation/calldata rejections. For admission I reuse REVM's
Shanghai intrinsic-gas rules, including calldata bytes, creation overhead and
initcode-word metering, and reject oversized initcode and gas limits above a
block's capacity. This part of the M6 gas work is needed as soon as contracts
are admitted. I also check cost addition for overflow.

I added a conservative block guard: a transaction must have its declared gas
limit available in the remaining block budget before execution. Its actual
charged gas reduces the budget afterward. A transaction that does not fit stays
pending. The final M6 block-packing verification remains to be completed.

I verified that a revert is still mined and charged, advances the nonce, and
rolls back ETH value, storage writes and emitted logs. A separate infinite-loop
contract proves that out of gas is mined with status zero and consumes the full
declared limit. I extended signing helpers for deployments and contract calls.

My M3 suite passed **75 tests**, including the brief's raw-bytecode deployment,
storage/log call, revert, unchanged supply-conservation test, and real-server RPC
checks for deployment receipts and log metadata. Topic/data preservation and gas
admission also have regression checks. Release build and warning-free Clippy
passed. RPC simulation remains M4; inspection methods
remain M5. Storage roots and log blooms remain unfinished M6 work.
