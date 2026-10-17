# Breeja EVM project notes

I started on 14 October 2026 and followed [FINAL-PROJECT.md](FINAL-PROJECT.md)
through M6. I integrated REVM into the existing node and preserved its mining,
signature recovery and snapshot/execute/commit structure.

## My orientation answers

1. **Pool checks.** I found the `TxKind::Create` rejection, the rejection of
   non-empty calldata, and `gas < TRANSFER_GAS`. I initially thought variable gas
   meant removing the third check. I corrected that: I remove the blanket contract
   rejections and replace the flat floor with intrinsic gas for each transaction.
   Shanghai charges 21,000 base gas, 4/16 per zero/non-zero calldata byte,
   32,000 extra for creation, and two gas per initcode word. I enforce its
   initcode-size limit. Signature, nonce and funding checks remain necessary.
2. **Execution failure.** I distinguish an invalid transaction from a valid
   execution that reverts or halts. I skip invalid transactions. I mine execution
   failures with status zero, advance the sender's nonce and charge gas.
   REVM rolls back attempted contract changes while retaining transaction-level
   accounting. Out of gas consumes the full limit; a revert can leave gas unused.
3. **Reservation versus charge.** I reserve `gas_limit * gas_price` plus value
   because it is the maximum liability, including queued spending. I ultimately
   charge actual gas and release unused allowance. Affording the final transfer
   bill does not necessarily mean affording a larger declared limit up front.
4. **History cost.** I keep full account snapshots after each block. Deep copies
   of storage maps make memory proportional to the sum of historical state sizes:
   retaining unchanged state of size S for B blocks costs roughly O(B * S).
   Snapshot creation also takes time in the commit lock. Sharing immutable code
   bytes helps but does not solve repeated storage-map copying.

## M0: my baseline

I started from `bbbdf34` on `feat/evm`. REVM was already an uncommitted dependency;
I preserved it and its lockfile changes. My toolchain is Rust/Cargo 1.95.0 and
my lockfile resolves REVM 43.0.3. I checked its installed source for APIs.

I recorded **48 passing tests**, a successful release build and warning-free
Clippy. My first sandbox run could not bind RPC sockets; allowing localhost
access passed without changing assertions. My release node mined 20 empty
blocks, adjusted difficulty, served eleven methods and shut down cleanly.

I captured the baseline with:

    RUST_LOG=info timeout --signal=INT --kill-after=5s 5s \
      cargo run --release --locked --offline -- --dev --rpc-address 127.0.0.1:0

I used an ephemeral port and enabled info logging because my environment sets
`RUST_LOG=warn`. The interrupt deliberately returns 124. I retained full messages
in the M0 checkpoint; these are selected fields:

    INFO sealed block number=1 txs=0 difficulty=3 nonce=2 attempts=3
    INFO sealed block number=2 txs=0 difficulty=4 nonce=7 attempts=8
    INFO sealed block number=3 txs=0 difficulty=5 nonce=25 attempts=26

## M1: my state and database adapter

I added raw `Bytes` code and `HashMap<U256, U256>` storage to `Account`, keeping
primitives independent of REVM. I retained the four-field account RLP and
excluded raw code/storage; their hash fields carry the commitments.
I added historical accessors and tested snapshot isolation.

I put `BreejaDb` over an owned snapshot in `crates/node/src/evm.rs`.
I need `Database` because execution discovers addresses and slots dynamically;
`DatabaseCommit` applies REVM's returned changes. I return `None` for an unknown
account without creating it and zero for an absent slot. I derive EVM code hashes
from actual bytes and cache analyzed bytecode by hash within each snapshot.

I commit touched accounts, remove selfdestructed accounts, clear storage for newly
created accounts and remove zero slots. I persist present values rather than
original journal values and preserve code when REVM has not loaded it.
I update the working snapshot; the miner installs it in live state after sealing.

## M2: my execution and accounting

I first proved transfer parity: exactly 21,000 gas, the expected balances, one
nonce increment and one fee payment. I removed manual transfer accounting.
REVM pays the beneficiary; I separately pay the 5 ETH block reward after
transactions. I use `tx_gas_used()` and drop the EVM before committing its output.

I chose Shanghai, zero base fee, the miner as beneficiary and the parent hash
as `prevrandao`. A non-zero base fee would burn ETH and break the original
supply rule. I deliberately combine Shanghai execution with teaching PoW and
Frontier-style rewards; I do not claim historical Ethereum consensus.

I fix the candidate's EVM-visible fields before execution and supply preceding
ancestor hashes for `BLOCKHASH`. I cannot use the current block's hash as
randomness: execution produces its state root before mining can find that hash.
I build the database, execute and mine outside the shared lock, then stamp
block hashes and commit briefly. My original supply-conservation test still passes.

## M3: my contracts and receipts

I changed destinations to `Option<Address>`; absence means creation.
I carry signed initcode/calldata into REVM and mined transactions. I retain
created addresses and logs with address, topics, data and block/transaction/log
positions, stamping block hashes after mining. I render creation destinations as
JSON `null` and correct the wire spelling of `cumulativeGasUsed`.

I removed the no-contract admission rules and use REVM's Shanghai intrinsic-gas
calculation. I reject oversized initcode, gas limits above the block limit and
overflowing maximum costs. I test deployment, storage, LOG0/LOG1, reverts and
out-of-gas through a real miner. I check that a revert rolls back value,
storage and logs while preserving fees and the nonce increment.

## M4: my calls and estimates

I execute unsigned requests on copied databases and discard changes.
I relax nonce, balance and contract-sender checks only for simulation;
mining retains strict validation. I guard numeric widths, cost overflow and
conflicting `data`/`input` aliases.

I default `eth_call` gas price to zero. I return successful output, JSON-RPC
code 3 with raw bytes for a revert, and an execution error for a halt.
I capture historical accounts, block fields and the preceding 256 hashes
coherently, release the lock, and execute on RPC blocking workers.

I default estimation to 1 gwei and cap gas by the block limit, request cap and
a supplied sender's available funds. I probe the upper bound, then binary-search
from intrinsic gas, copying original state for every probe. I return a tested
sufficient limit without extra margin. I send deployments and calls with exactly
that estimate, including a storage-refund case where using charged gas would fail.

I treat binary search as a heuristic for gas-dependent contracts, not a proof
of global minimality. I keep `pending` simulation on committed head state;
estimation reserves queued spending. I do not execute a pending-state overlay.

## M5: my inspection RPC

I added historical `eth_getCode` and `eth_getStorageAt`, returning empty code or
a full 32-byte zero word when absent. I scan mined receipts in canonical order
for `eth_getLogs`. I support inclusive ranges, one/many addresses, positional
topic AND, alternatives OR, and `null` wildcards. Empty alternatives are wildcards.
I reject invalid ranges and conflicting block-hash/range filters.

I added fixed-price `eth_gasPrice`. My README `cast send --legacy` command
worked without gas price, limit or nonce: status one, 21,000 gas, block 15,
hash `0x22d76a11db763d4b5cc16135b4d8ee08acdce86bb84fb87f4a9fd353176bba26`.
I changed the unsupported-method test to `debug_traceTransaction`, explaining
in its comment why `eth_gasPrice` is no longer an unsupported example.

## M6: my gas budget, blooms and commitments

I accumulate actual gas use while requiring each declared limit to fit the
remaining budget before execution. I leave oversized candidates queued and scan
later candidates for smaller transactions. I removed the fixed 149-candidate
cutoff and stop once less than transfer intrinsic gas remains.
I test allowance reuse and a 150-candidate queue whose first block contains
a 2,000,000-gas failed creation plus a later 21,000-gas transfer.

I construct the 2048-bit bloom from emitting addresses and topics, using three
11-bit indices from each Keccak hash with Ethereum's byte/bit ordering.
I exclude log data. I compare against Alloy's bloom implementation and test
receipt/block blooms, empty blocks, global log indices and reverted logs.
I derive block blooms from receipts for RPC, preserving the original PoW header.

I hash sorted non-zero storage entries as fixed-width slot/value pairs.
I derive code/storage commitments from actual contents before four-field account
RLP encoding, avoiding stale cached hashes. I update touched storage metadata.
I test code-only and storage-only differences, full-width slots, insertion order
and zero normalization. I retain a deterministic teaching hash, not an Ethereum trie.

## My Solidity demo and final verification

I added `examples/evm-demo/src/Counter.sol` and `scripts/evm-demo.sh`.
I pin Solidity 0.8.24, Shanghai and 200 optimizer runs. I check compiled
creation/runtime fixtures into the repository so Rust tests need no compiler;
the script recompiles and compares both before deployment.

I deploy, set/read 42, verify slot zero and the event, and prove a simulated
write changes nothing. I also test exact estimates and a mined revert in Rust.
My live demo passed at contract `0x5fbdb2315678afecb367f032d93f642f64180aa3`,
block 18, setter gas **44,970**, status one at 1 gwei.
I fixed missing block-response fields discovered by cast's confirmation watcher:
empty ommer hash/list and parent-hash `mixHash`. I test Alloy block deserialization.

I retained existing coverage and commented on changed no-EVM expectations:

| Stage | Passing tests | Checkpoint |
| --- | --- | --- |
| M0 | 48 | `813e188` |
| M1 | 59 | `6a85d48` |
| M2 | 65 | `94474fc` |
| M3 | 75 | `30586ef` |
| M4 | 85 | `7eea765` |
| M5 | 88 | `d7f8dbf` |
| M6 | 95 | M6 commit |

I passed the workspace tests, release build and Clippy with warnings denied:

    cargo test --workspace --locked --offline
    cargo clippy --workspace --all-targets --locked --offline -- -D warnings
    cargo build --release --locked --offline

I leave state in memory, full historical snapshots, whole-pool copying and linear
log scans. I preserve the starter's hash-list transaction/receipt roots; the
receipt list commits only to transaction hashes. I expose derived blooms rather
than adding them to the hashed header, and block sizes remain estimates.
I do not implement Ethereum consensus compatibility, trie proofs, persistence,
reorgs or a pending execution overlay. I still need the submission pull request
against my fork after reviewing this implementation.
