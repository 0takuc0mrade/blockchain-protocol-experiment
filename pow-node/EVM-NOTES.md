# Breeja EVM project notes

I started this project on 14 October 2026 and am following
[FINAL-PROJECT.md](FINAL-PROJECT.md). This is my working technical write-up through
M5. I distinguish completed checks from the M6 work still outstanding.

## What I started with

I traced a signed transaction through RPC, signature recovery and pool admission,
a miner-owned account snapshot, `apply_transfer`, the block reward, proof of work,
and a short locked commit. Accounts held nonce and balance plus placeholder
`code_hash` and `storage_root` fields. The pool accepted only legacy transfers;
RPC calls needing execution were refused. I kept that existing mining and
signature infrastructure when adding REVM.

## My orientation answers

1. **Pool checks.** I found the `TxKind::Create` rejection, the rejection of
   non-empty calldata, and `gas < TRANSFER_GAS`. I initially thought variable gas
   meant removing the third check. I corrected that: I remove the blanket contract
   rejections and replace the flat floor with intrinsic gas for the actual
   transaction. Shanghai charges 21,000 base gas, 4/16 per zero/non-zero calldata
   byte, 32,000 extra for creation, and initcode-word metering. I also enforce its
   initcode size limit. Signature, nonce and funding validation remain necessary.
2. **Execution failure.** I distinguish invalid transactions from valid executions
   that revert or halt. Invalid transactions are skipped. A reverted or halted
   transaction is mined, advances its sender's nonce, pays gas, and has receipt
   status zero. I retain transaction-level accounting while REVM rolls back
   attempted contract changes. Out of gas consumes the full limit; a revert can
   leave unused gas.
3. **Reservation versus charge.** I reserve `gas_limit * gas_price` plus value
   because that is the maximum liability, including amounts already queued.
   The sender ultimately pays charged gas, with unused allowance released.
   A sender who can afford the final 21,000-gas transfer bill may still be unable
   to cover a larger declared limit up front.
4. **History cost.** I keep full account snapshots after each block. With deep
   storage-map copies, memory grows with the sum of historical state sizes:
   an unchanged state of size S retained for B blocks costs roughly O(B * S).
   Snapshot creation also takes time in the commit lock. Shared immutable code
   bytes reduce copying; they do not solve repeated storage-map copies.
   I test historical isolation and document this teaching implementation's limit.

## M0: baseline

I started from `bbbdf34` and created `feat/evm`. REVM was already added as an
uncommitted dependency; I preserved it and its lockfile changes. My installed
toolchain is Rust/Cargo 1.95.0, and the lockfile resolves REVM 43.0.3. I consult
that installed source for APIs rather than assuming older REVM examples apply.

My baseline had **48 passing tests**. The first sandbox run could not bind RPC
sockets; rerunning with localhost access passed without changing assertions.
Release build and Clippy passed. My release node mined 20 empty blocks, adjusted
difficulty, served 11 RPC methods and shut down cleanly.

I captured the baseline with:

    RUST_LOG=info timeout --signal=INT --kill-after=5s 5s \
      cargo run --release --locked --offline -- --dev --rpc-address 127.0.0.1:0

I used an ephemeral port and explicitly enabled info logging because my
environment sets `RUST_LOG=warn`. The interrupt deliberately makes `timeout`
return 124. These are selected fields from three captured block messages; the
full lines are retained in the M0 checkpoint:

    INFO sealed block number=1 txs=0 difficulty=3 nonce=2 attempts=3
    INFO sealed block number=2 txs=0 difficulty=4 nonce=7 attempts=8
    INFO sealed block number=3 txs=0 difficulty=5 nonce=25 attempts=26

## M1: state and REVM's database

I added `code: Bytes` and `storage: HashMap<U256, U256>` to `Account`.
I chose raw bytes to keep primitives independent of REVM. Snapshot clones share
immutable byte allocations and independently copy storage maps. I added current
and historical code/storage accessors. I retain the four-field account RLP,
excluding raw code/storage with `#[rlp(skip)]`; their commitments belong in
the hash fields. `set_code` updates bytes and their hash together.

I put `BreejaDb` in `crates/node/src/evm.rs`, over an owned snapshot. An EVM
cannot know every address or slot it will touch before execution; `Database`
answers reads as REVM requests them. `DatabaseCommit` applies the returned
changes. I return `None` for an unknown account without creating it, and zero
for an absent slot. I derive the EVM code hash from actual bytes, including the
canonical empty-code hash, while leaving untouched legacy account metadata intact.

I cache analyzed bytecode by hash once per snapshot. I do not implement a global
deduplicated code store. I commit touched accounts only, remove selfdestructed
accounts, clear old storage for new accounts, and remove zero-valued slots.
I persist each slot's current value rather than its original journal value.
I preserve existing code when REVM has not loaded it. These commits update the
working snapshot; the miner controls installation in live state.

## M2: execution and accounting

I wrote the parity test before implementing `execute_transaction`. It proves
exactly **21,000 gas**, the expected transfer balances, one nonce increment and
one fee payment. I removed the old manual debit/credit/nonce logic. REVM pays the
beneficiary; Breeja separately pays the 5 ETH block reward after transactions.

I configured Shanghai execution, `basefee = 0`, the miner as beneficiary, and the
parent hash as `prevrandao`. A non-zero base fee would burn ETH and invalidate
the existing supply-conservation rule. This Shanghai execution plus teaching
proof of work and Frontier-style rewards is a deliberate hybrid, not a historical
Ethereum network. I will target Shanghai explicitly when compiling the Solidity demo.

I build the candidate before execution so its number, timestamp, difficulty,
gas limit and miner agree with the header later mined. I supply ancestor hashes
for `BLOCKHASH`. The candidate's own hash cannot exist yet: execution supplies
the state root, which proof of work hashes next. I build the database, execute and
mine outside the shared lock, then stamp block hashes and commit briefly.

I use `tx_gas_used()`, drop the EVM before committing returned state, and map
success/failure to receipt status. Existing executor tests now supply database
and block context; their economic assertions and the mining supply-conservation
test remain intact. My live README `cast mktx` transfer was mined in block 12
with status one, 21,000 gas and a 21,000,000,000,000 wei fee. I checked supply
against genesis plus rewards at a single fixed height.

## M3: contracts and receipts

I changed destinations to `Option<Address>`: absence means creation. I carry
signed initcode/calldata through the pool into REVM and mined transactions.
Receipts retain created addresses and logs, including address, topics, data and
block/transaction/log positions. I stamp log block hashes after mining and render
creation destinations as JSON `null`. I corrected the wire spelling of
`cumulativeGasUsed`.

I reuse REVM's Shanghai intrinsic-gas calculation for admission instead of
duplicating its constants. I reject oversized initcode, gas limits above a block's
capacity and overflowing value-plus-fee addition. I brought this part of M6
forward because contract admission needs it immediately. I also reserve the
declared transaction gas against the remaining block budget before execution;
actual charged gas reduces that budget afterward. Calls that cannot fit stay queued.

I tested the brief's raw deployment and storage/log contract using signed
transactions and the real miner. I also tested topics and data, a revert after
an ETH payment/storage write/log, and an infinite loop that exhausts gas.
The failed transactions are included and charged, while the attempted effects
roll back. The supply check still holds. RPC tests verify created addresses,
nullable destinations and mined log metadata.

## M4: simulation and gas estimation

I share the same transaction and block mapping between mining and simulation.
For simulation I clone the database, execute, and discard the returned state
without calling `commit`. I enable REVM's optional balance and EIP-3607 controls
but disable those checks only in the simulation configuration; mining keeps
strict defaults. I also disable nonce validation for hypothetical RPC callers.
This lets an unfunded address or a contract ask what a call would do.

I default `eth_call` gas price to zero, respect a supplied price, and cap gas by
the block limit. Successful calls return output bytes. Reverts return JSON-RPC
code 3 with raw revert bytes; halts return an execution error. I validate numeric
widths and reject conflicting `data`/`input` aliases. I guard cost overflow
even when balance validation is disabled.

I capture accounts, the selected header and its ancestor hashes under one read
lock, then release it before execution. Historical tags use historical code,
storage and block fields. I corrected the hash window to the preceding 256
blocks, excluding the current block. I run simulation/estimation on JSON-RPC
blocking workers to keep CPU work off the async server's workers.

For estimation I default price to 1 gwei. When a sender is supplied, I check
value affordability and cap the upper bound by funds left for gas, so a sender
need not afford an entire block's gas. I honor the request gas cap and use
intrinsic gas as the lower boundary. I first execute at the upper bound, then
binary-search using a fresh copy of the original state for every probe.
I return a successfully executed limit without an added margin.

I tested sending both a deployment and a call with exactly their estimated gas.
I added a refund case: charged receipt gas is lower than the limit needed to
complete both storage writes. Returning `gas_used` directly would fail that test.
Binary search remains a heuristic: `GAS`-dependent contracts need not be
monotonic, and I do not claim a globally minimal limit. The estimate applies to
the probed state and matching transaction parameters.

I deliberately keep `pending` simulation on committed head state; estimation
reserves queued spending when that tag is requested. I do not execute a pending
overlay. Without a supplied sender, I provide a hypothetical estimate without
claiming that any particular account can afford the eventual transaction.

## M5: inspection RPC

I added historical `eth_getCode` and `eth_getStorageAt`, returning empty code
and a full 32-byte zero word for missing data. I scan mined receipts in canonical
block/transaction order for `eth_getLogs`. I support inclusive ranges, one or
many addresses, positional topic AND, alternatives OR, and `null` wildcards.
I also accept a block hash instead of a range and reject conflicting or invalid
filters. I run the scan on a blocking worker.

I added the fixed 1 gwei `eth_gasPrice`. My README `cast send --legacy` command
worked without supplying gas price, gas limit or nonce: status one, 21,000 gas,
block 15, hash `0x22d76a11db763d4b5cc16135b4d8ee08acdce86bb84fb87f4a9fd353176bba26`.
I now test unsupported-method behavior with `debug_traceTransaction`, because
the old test's `eth_gasPrice` example has become supported.

## Verification and remaining work

I retained the original test coverage and explain changed no-EVM expectations
in code comments. My checkpoint evidence is:

| Stage | Workspace tests | Checkpoint |
| --- | --- | --- |
| M0 | 48 passed | `813e188` |
| M1 | 59 passed | `6a85d48` |
| M2 | 65 passed | `94474fc` |
| M3 | 75 passed | `30586ef` |
| M4 | 85 passed | `7eea765` |
| M5 | 88 passed | Current milestone |

My suite includes all mining/supply tests, five contract integration tests,
and 25 real-server RPC tests. I verify whole-account-map equality after
`eth_call`, historical storage, block fields/hashes, revert data, affordable gas,
pending reservations, and exact-estimate submission. I run:

    cargo test --workspace --locked --offline
    cargo clippy --workspace --all-targets --locked --offline -- -D warnings
    cargo build --release --locked --offline

All three checks passed. I also checked the updated release CLI help; no tests
were ignored and Clippy reported no warnings.

M6 remains: final block-packing verification, storage-root
commitments and real log blooms. Storage-only changes still do not affect the
state root, and receipt blooms are still placeholders. I still need the Solidity
deployment/call demo and final submission review. State remains in memory and
full snapshots remain costly; persistence and a trie are outside my required scope.
