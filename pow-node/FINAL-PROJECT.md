# Final Project: Give Breeja an EVM

## What you are building

Breeja is currently an Ethereum node with everything *except* a virtual machine.
It mines real proof-of-work blocks, verifies real signatures, tracks real account
state, and serves the Ethereum JSON-RPC API — but the only transaction it accepts
is a plain ETH transfer, because it has no way to execute code.

Your final project is to remove that limitation. When you are done, your node must
be able to deploy a Solidity contract, call it, store data in it, emit events from
it, and report all of that over JSON-RPC — on a chain your own miner is extending
with proof of work.

**The default route is [REVM](https://github.com/bluealloy/revm)**, the EVM
implementation used in production by Reth and Foundry. You will not be writing an
interpreter. You will be doing what a real client team does: taking an EVM and
integrating it into a node.

If you would rather write your own EVM from scratch, see
[Appendix B](#appendix-b-writing-your-own-evm) — that route is open, but you are
on your own for the interpreter itself.

### The one-sentence version

You are replacing a 30-line function that moves ETH with a real virtual machine,
and then dealing with everything that breaks as a result.


## Deliverables

| # | Deliverable | Notes |
| --- | --- | --- |
| 1 | Working code on a branch named `feat/evm` | Must build with `cargo build --release` and pass `cargo clippy` with no warnings |
| 2 | All existing tests still passing, or justified changes | Some *must* change. See [Milestone 2](#milestone-2-execute-transfers-through-revm). Changing a test requires a comment explaining why the old expectation was wrong |
| 3 | New tests for every new capability | See [Testing requirements](#testing-requirements) |
| 4 | A deployed contract demo | A shell script or Rust test that deploys a Solidity contract to your node and calls it |
| 5 | `EVM-NOTES.md` | Your own write-up: what you did, what broke, what you decided and why. 2–4 pages |

Submit as a pull request against your fork.

### Timeline

Fill these in from the course schedule:

| Milestone | Target date |
| --- | --- |
| M0–M1 (state model) | |
| M2–M3 (execution, contracts) | |
| M4–M5 (RPC) | |
| M6 + write-up | |


## Learning objectives

By the end you should be able to explain, from code you wrote:

1. **Why the EVM needs a database interface** rather than direct access to state, and
   what `Database` and `DatabaseCommit` each do.
2. **What an account actually is** once contracts exist — and why `storage_root` and
   `code_hash`, which Breeja currently leaves empty, suddenly matter.
3. **Where gas comes from.** Breeja hardcodes 21,000. You will watch the EVM compute
   it, and find out what `eth_estimateGas` has to do when the answer is not a constant.
4. **What a reverted transaction is.** Breeja has no concept of a transaction that
   fails after being included. You will add one, and discover why a receipt has a
   `status` field at all.
5. **Why `eth_call` exists** and how it differs from `eth_sendRawTransaction` by exactly
   one thing: whether you keep the state changes.
6. **Who pays the miner.** This is subtler than it looks, and it is where most of you
   will introduce a bug. See [Pitfall 1](#pitfall-1-revm-already-pays-the-miner).


## Part 0: Orientation

**Do not skip this.** You cannot integrate an EVM into a node you do not understand.

### Read these files, in this order

| File | What to look for |
| --- | --- |
| `crates/primitives/src/node.rs` | `Account` — note `storage_root` and `code_hash` are always empty. That is what you are about to change |
| `crates/node/src/executor.rs` | `apply_transfer` — the entire state transition, ~30 lines. This is what REVM replaces |
| `crates/node/src/miner.rs` | The block production loop. Note *when* the state lock is held |
| `crates/node/src/pool.rs` | The validation rules. Three of them exist only because there is no EVM — find them |
| `crates/rpc/src/handlers.rs` | `call_handler` and `estimate_gas_handler` — both currently return errors for anything interesting |

### Answer these before writing code

Write the answers in your `EVM-NOTES.md`. They are not busywork; each one is a
decision you will have to make later.

1. In `pool.rs`, two checks reject contracts outright, citing the missing EVM. Find
   them. A third check does not mention the EVM at all but silently assumes every
   transaction is a plain transfer — find that one too. What should each become:
   removed, or relaxed?
2. `apply_transfer` returns `Result<u64, Error>`, and a failure means the transaction
   is **skipped** rather than included. Read the comment above `execute_block`
   explaining why. Once contracts exist, is that still the right behaviour? What does
   real Ethereum do?
3. Breeja charges `TRANSFER_GAS` but reserves `gas_limit × gas_price` when admitting
   to the mempool. Why the difference? What happens to the difference once gas is
   variable?
4. `BlockState` keeps a full account snapshot per block, which is how historical
   `eth_getBalance` works. What does that cost once accounts have storage?

### Get REVM compiling before you change anything

```bash
cd toy-node
cargo add revm --package node
cargo build
```


## Part 1: What actually changes

Before the milestones, understand the shape of the problem. An EVM is not a module
you bolt on; it changes what state *is*.

### Accounts grow two dimensions

Today a Breeja account is a nonce and a balance. With an EVM it also has:

- **Code** — immutable bytes, set once at deployment.
- **Storage** — a `HashMap<U256, U256>`, unbounded and mutable.

This is why `code_hash` and `storage_root` are in the `Account` struct already.
Ethereum's account encoding has always had four fields; Breeja just never used two
of them.

### Execution stops being a function and becomes a conversation

`apply_transfer` reads two accounts and writes three. An EVM cannot work that way —
it does not know in advance which accounts a transaction will touch. A contract can
read any address, call any contract, touch any storage slot, and decide what to do
next based on what it finds.

So REVM does not take your state as an argument. It takes a **`Database`**: a trait
with four methods that it calls *during* execution, whenever it needs something.

```text
   your state  ──implements Database──►  REVM asks: "balance of 0xabc?"
                                        REVM asks: "code at 0xdef?"
                                        REVM asks: "storage slot 7 of 0xdef?"
                                        ...
                                        REVM returns: a bundle of changes
   your state  ◄──DatabaseCommit────────  you apply them
```

That inversion is the single most important idea in this project. Everything else
follows from it.

### Transactions can now fail after being included

Breeja has no notion of this. A transaction either validates and gets mined, or is
rejected. With an EVM, a transaction can be perfectly valid — correct nonce, enough
balance, enough gas — and still `REVERT` halfway through because the contract decided
to reject it.

Such a transaction **is still mined**, and the sender **still pays** for the gas it
burned. That is what `status: "0x0"` on a receipt means, and it is why
`ExecutionResult` has three variants rather than being a `Result`.

### Gas stops being a constant

`eth_estimateGas` currently returns `0x5208` unconditionally. Once code can run, the
only way to know what a transaction costs is to execute it. That turns gas estimation
into a search problem — see [Milestone 4](#milestone-4-real-eth_call-and-eth_estimategas).


## Target architecture

```text
                      eth_sendRawTransaction / eth_call
   wallet / cast ───────────────────────────────────────┐
                                                        ▼
                                                 ┌─────────────┐
                                                 │  JSON-RPC   │  crates/rpc
                                                 └──────┬──────┘
                                   validate + queue     │     simulate (eth_call)
                                                        ▼           │
                                                 ┌─────────────┐    │
                                                 │   mempool   │    │
                                                 └──────┬──────┘    │
                                                        ▼           ▼
   ┌────────────────────────────────────────────────────────────────────────┐
   │  miner loop                                                            │
   │    1. snapshot state                                                   │
   │    2. for each tx:  ┌──────────────────────────────────────┐           │
   │                     │  REVM                                │           │
   │                     │    reads  ──► Database  (YOU WRITE)  │  ◄─────── │
   │                     │    writes ──► EvmState               │           │
   │                     └──────────────┬───────────────────────┘           │
   │                        DatabaseCommit (YOU WRITE)                      │
   │    3. pay block reward                                                 │
   │    4. proof of work (unchanged)                                        │
   │    5. commit block                                                     │
   └────────────────────────────────────┬───────────────────────────────────┘
                                        ▼
                                 ┌─────────────┐
                                 │ chain state │  accounts + CODE + STORAGE
                                 └─────────────┘
```

Note what does **not** change: the proof of work, the difficulty retargeting, the
block structure, the signature recovery, the mempool plumbing, the RPC server
skeleton. You are replacing one box.


## Milestones

Each milestone is independently testable. **Do them in order** and commit at each
boundary — if you break something in M4 you want to be able to diff against a
working M3.


### Milestone 0: Branch and baseline

**Goal:** a clean starting point you can always get back to.

```bash
git checkout -b feat/evm
cargo test --workspace   # 48 tests should pass
```

Record the baseline in `EVM-NOTES.md`: how many tests pass, and the output of
`cargo run --release -- --dev` for a few blocks.

**Acceptance:** branch exists, 48 tests pass, REVM is in `crates/node/Cargo.toml`.


### Milestone 1: Teach state about code and storage

**Goal:** accounts can hold code and storage, and REVM can read them.

**Build:**

1. Extend `Account` in `crates/primitives/src/node.rs` with code and storage. You
   choose the representation — but think about the trade-off. Storing `Bytecode`
   directly is simple; storing a `code_hash` plus a separate
   `HashMap<B256, Bytecode>` deduplicates identical contracts, which is what real
   nodes do and what REVM's `code_by_hash` method expects to exist.
2. Add accessors to `BlockState`: get code for an address, get a storage slot.
3. Implement `Database` and `DatabaseCommit`. See
   [Appendix A](#appendix-a-revm-reference) for a complete, verified implementation —
   this is the one piece of code I am giving you outright, because getting the trait
   signatures wrong costs hours and teaches nothing.

**Where to put the `Database` impl.** A new module, `crates/node/src/evm.rs`, is the
natural home. Do **not** implement it on `BlockState` directly — REVM needs `&mut`
access for the whole duration of a transaction, and `BlockState` lives behind an
`RwLock` shared with the RPC server. Wrap a *snapshot* instead. The miner already
works this way (`accounts_snapshot()`); follow that pattern.

**Acceptance:**

- `cargo test --workspace` still passes (48 tests — you have not changed behaviour yet).
- A new unit test: construct your database type with one funded account, call
  `Database::basic` on it, and assert the balance and nonce come back correctly.
- A new unit test: `DatabaseCommit` applied to a hand-built change set updates
  balance, nonce, code, and a storage slot.


### Milestone 2: Execute transfers through REVM

**Goal:** the existing behaviour, produced by the EVM instead of by `apply_transfer`.
No new features. This is a *parity* milestone, and it is the most important one in the
project — if you get this right, everything after it is incremental.

**Build:**

1. In `crates/node/src/executor.rs`, replace the body of `apply_transfer` (or write a
   new `execute_transaction` alongside it) to run the transaction through REVM.
2. Map `PendingTransaction` onto REVM's `TxEnv`, and your `Block` onto `BlockEnv`.
3. Take `ExecutionResult` back and turn it into your `Transaction` and
   `TransactionReceipt`.

**The parity test — write this first:**

> A plain ETH transfer, executed through REVM, costs exactly **21,000 gas** and moves
> exactly the same ETH as `apply_transfer` did.

I have verified this against REVM 43: a transfer with empty calldata reports
`tx_gas_used() == 21000`. If your number differs, something is wrong with your
`TxEnv` or `CfgEnv` — fix it before moving on.

**Expect these existing tests to change**, and understand why before you touch them:

- `executor.rs` unit tests call `apply_transfer` directly with a hand-built accounts
  map. They will need the new signature.
- `the_chain_never_creates_or_destroys_eth_outside_the_block_reward` in
  `crates/node/tests/mining.rs` is your canary. **If this test fails, you have a real
  bug** — almost certainly [Pitfall 1](#pitfall-1-revm-already-pays-the-miner). Do not
  "fix" it by weakening the assertion.

**Acceptance:**

- Transfers still work end to end: `cargo test -p node --test mining` passes.
- The parity test above passes.
- `cargo run --release -- --dev` still mines, and you can still send a transfer with
  `cast mktx` exactly as the README describes.

### Milestone 3: Contracts

**Goal:** deploy a contract and call it.

**Build:**

1. **Relax the mempool rules** in `pool.rs`. Two checks reject contracts explicitly —
   `TxKind::Create` and non-empty calldata. A third, the `gas < TRANSFER_GAS` floor,
   assumes every transaction costs the same. Decide what each becomes. Note that the
   intrinsic gas cost of a transaction with calldata is *not* 21,000 — calldata is
   charged per byte, so a flat floor is now wrong.
2. **Handle `TxKind::Create`**. A transaction with no `to` deploys a contract. REVM
   returns the new address via `ExecutionResult::created_address()`.
3. **Receipts get real.** Three fields that are currently hardcoded must become real:
   - `contract_address` — `Some(addr)` for a deployment, `None` otherwise. It is
     currently always `None`.
   - `logs` — currently always empty. REVM returns them from
     `ExecutionResult::logs()`. You will need a log type in `primitives` with
     `address`, `topics`, and `data`.
   - `status` — currently always `1`. A reverted transaction gets `0`.
4. **Include failed transactions.** This is the behaviour change you reasoned about in
   Part 0, question 2. A reverted transaction is mined, the sender pays, the nonce
   advances, and the receipt says `status: 0x0`. Only transactions that are *invalid*
   (bad nonce, insufficient balance for the up-front gas reservation) are skipped.

**Acceptance:**

- An integration test that deploys a contract from raw bytecode and asserts
  `contract_address` is `Some`, and that the code is committed to state.
- An integration test that calls the deployed contract and asserts a storage slot
  changed and a log was emitted.
- An integration test that a **reverting** transaction is mined with `status: 0x0`,
  that the sender was charged, and that the sender's nonce advanced.

Use this contract for the tests — it is small enough to hand-assemble, and I have
verified it works end to end on REVM 43:

```text
initcode:  0x600b600c600039600b6000f3602a60005560006000a000

runtime (the last 11 bytes):
  602a 6000 55   PUSH1 42, PUSH1 0, SSTORE   -> storage[0] = 42
  6000 6000 a0   PUSH1 0, PUSH1 0, LOG0      -> emits an empty log
  00             STOP
```

Deploying it costs ~55,500 gas; calling it costs ~43,500 and emits 1 log.

### Milestone 4: Real `eth_call` and `eth_estimateGas`

**Goal:** the two methods that currently refuse to answer, answering.

**Build:**

1. **`eth_call`.** Execute the transaction against current state, return the output
   bytes, and **throw the state changes away**. That discard is the only difference
   between `eth_call` and a real transaction. Do not commit. Consider: should
   `eth_call` even require a valid nonce or sufficient balance? Look at what
   `CfgEnv` offers for disabling checks, and justify your choice in your notes.
2. **`eth_estimateGas`.** You cannot know the cost without executing, and executing
   with too little gas fails. The standard approach is a **binary search**:
   - Execute with the block gas limit. If it fails, the transaction cannot succeed at
     any gas limit — return an error.
   - Binary search the range `[intrinsic_cost, block_gas_limit]` for the lowest limit
     at which execution still succeeds.
   - Return that, usually with a small safety margin.

   Each probe is a full execution against a throwaway copy of state. Roughly 20
   iterations covers a 30M gas range.

   Be careful: a contract can behave *differently* depending on how much gas it has
   (via the `GAS` opcode), so the search is a heuristic, not a proof. Note that in
   your write-up.

**Acceptance:**

- `eth_call` against your deployed contract returns the expected bytes.
- `eth_call` does not change state: assert a storage slot is unchanged after calling
  a function that would have written to it.
- `eth_estimateGas` returns `0x5208` for a plain transfer (parity with before) and
  something plausible and *sufficient* for a contract call — assert that a transaction
  sent with exactly the estimated gas actually succeeds.
- The no-EVM errors are gone from both handlers.

### Milestone 5: The RPC methods contracts require

**Goal:** a client can actually inspect contracts on your chain.

The earlier assignment told you to implement exactly eleven methods and add none.
**That constraint is now lifted** — it existed because a node with no EVM has nothing
more to say. Now it does.

**Required:**

| Method | Returns |
| --- | --- |
| `eth_getCode` | The code at an address, `0x` if none. Takes an address and a block tag |
| `eth_getStorageAt` | One 32-byte storage slot. Takes an address, a slot, and a block tag |
| `eth_getLogs` | Logs matching a filter |

`eth_getLogs` is the hard one. A full implementation supports `fromBlock`, `toBlock`,
`address` (one or many), and `topics` with positional `null` wildcards. **Minimum
requirement:** `fromBlock`, `toBlock`, and `address`. Topic filtering earns full
marks. Read the
[spec](https://ethereum.org/en/developers/docs/apis/json-rpc/#eth_getlogs) carefully —
the topics array semantics are genuinely unintuitive.

**Strongly recommended:** `eth_gasPrice`. It is one line, and without it `cast send`
cannot auto-fill its parameters — which means with it, your node works with standard
tooling instead of requiring the `cast mktx` dance in the README. This is the single
highest-value line of code in the project.

**Acceptance:**

- `eth_getCode` returns the deployed runtime bytecode, and `0x` for an account with
  no code.
- `eth_getStorageAt` returns `42` (as a 32-byte hex value) for slot 0 of the test
  contract.
- `eth_getLogs` finds the log emitted in M3, filtered by both block range and address.
- If you added `eth_gasPrice`: a `cast send` command that works without `--gas-price`.
  Put it in your README.

### Milestone 6: Make it hold together

**Goal:** the things that are now wrong because gas is variable.

**Build:**

1. **Block gas limit enforcement.** The miner currently fits
   `BLOCK_GAS_LIMIT / TRANSFER_GAS = 149` transactions per block, assuming every
   transaction costs the same. That assumption is dead. Fill blocks by *accumulating
   gas used* until the next transaction would not fit.
2. **Mempool admission gas.** The `gas < TRANSFER_GAS` check must become a real
   intrinsic-gas calculation: 21,000 base, plus per-byte calldata cost, plus 32,000
   more for a contract creation.
3. **`logsBloom`.** Currently 256 hardcoded zero bytes. Build a real bloom filter from
   the receipt's logs. The algorithm is in the Yellow Paper and is about 15 lines.
4. **State root.** `state_root_of` hashes sorted addresses and account encodings. It
   now needs to commit to code and storage too, or two chains with different contract
   state would produce the same root — which would make the root useless.

**Acceptance:**

- A test that a block does not exceed `BLOCK_GAS_LIMIT`, using transactions with
  varying gas.
- A test that two states differing only in one storage slot produce different state
  roots.
- A test that `logsBloom` is non-zero for a block containing logs, and that it matches
  the logs actually present.

## Pitfalls

These are the bugs I expect most of you to hit. The first two I verified
experimentally against REVM 43 while writing this document, because they are the kind
that fail silently.

### Pitfall 1: REVM already pays the miner

**Breeja's `apply_transfer` credits the fee to the miner by hand. REVM does this
itself.** If you keep both, every fee is paid twice and your chain mints ETH out of
nothing.

Verified: with `BlockEnv.beneficiary` set and `basefee = 0`, after a 21,000-gas
transfer at 1 gwei the beneficiary's balance increased by exactly
21,000 × 1 gwei = 21,000,000,000,000 wei. REVM did that, not me.

**What to do:** remove your manual fee credit. Keep the block reward — REVM knows
nothing about block rewards, that is your node's policy, and you must still pay it
yourself.

The test `the_chain_never_creates_or_destroys_eth_outside_the_block_reward` exists
precisely to catch this. Trust it.

### Pitfall 2: a non-zero basefee burns ETH

Verified: with `basefee = 500_000_000` and `gas_price = 1_000_000_000`, the sender was
charged the full gas price but the beneficiary received only *half* the fee. The other
half was burned — that is EIP-1559 working exactly as designed.

Breeja is deliberately pre-1559: a single fixed `GAS_PRICE`, no fee market, no burn.
**Set `basefee = 0`** to match. If you set it to anything else, your supply-conservation
test will fail and it will be *right* to fail.

If you *want* a fee market, that is a legitimate stretch goal — but then update the
test to account for the burn, and say so in your notes.

### Pitfall 3: REVM increments the nonce

REVM bumps the sender's nonce as part of execution. `apply_transfer` also calls
`increase_nonce()`. Keep both and every transaction consumes two nonces, which breaks
every subsequent transaction from that sender.

### Pitfall 4: `gas_used()` is deprecated

`ExecutionResult::gas_used()` compiles but emits a deprecation warning: it is ambiguous
after EIP-8037's state-gas split. Use **`tx_gas_used()`**. Since a deliverable is a
clean `cargo clippy`, you will have to fix this anyway.

### Pitfall 5: the `SpecId` tension

Breeja is thematically Frontier-era — 5 ETH block rewards, "pi million" gas limit.
But Frontier predates almost every opcode a modern Solidity compiler emits. Compile a
contract with current `solc` and run it under `SpecId::FRONTIER` and it will fail on
something like `PUSH0`.

**Use `SpecId::SHANGHAI`.** It has everything modern Solidity needs and does not
require the blob-gas fields that `CANCUN` expects. If you choose `CANCUN`, you must
populate `blob_excess_gas_and_price` in your `BlockEnv`.

Acknowledge the inconsistency in your write-up. "Frontier consensus rules, Shanghai
execution rules" is not a real chain — but it is a deliberate teaching compromise, and
being able to articulate *why* it is a compromise is worth more than silently picking
one.

### Pitfall 6: the borrow checker and the state lock

REVM borrows your database mutably for as long as the EVM object lives. You cannot
commit while the EVM is still alive. Drop it first:

```rust
let outcome = evm.transact(tx)?;
let state = outcome.state.clone();
drop(evm);            // release the &mut borrow on the database
db.commit(state);
```

And do **not** hold the `RwLock` write guard across execution. The miner's existing
snapshot-execute-commit structure exists for exactly this reason; a contract call can
run for a long time, and blocking every RPC request while it does is a bug. Read the
comments in `miner.rs` before you restructure anything.

### Pitfall 7: `prevrandao`

Under post-merge specs (including Shanghai) the `DIFFICULTY` opcode becomes
`PREVRANDAO` and reads `BlockEnv.prevrandao`. Leaving it `None` can fail for contracts
that use `block.prevrandao`, so give it a real 32-byte value.

Breeja has no field for this. Real Ethereum fills it from the beacon chain's RANDAO,
and pre-merge it was Ethash's `mixHash` — neither of which Breeja's simple keccak
proof of work produces.

**Use the parent block's hash: `prevrandao: Some(parent.hash)`.**

And notice *why* it cannot be the current block's hash, because the reason is worth
internalising: execution happens **before** mining. The block's own hash does not exist
yet when the EVM runs — finding it is what the miner does next, and the hash depends on
the state root that execution produces. Anything the EVM reads about "this block" must
be known before the nonce is found.

(If you would rather add a real `mix_hash` field to `Block`, that is fine — but
remember it is part of the hashed header, so it must go into `BlockHeader` too, and
every block's hash changes.)

## Testing requirements

Breeja ships 48 tests. **Your submission must not reduce that number**, except where
you justify a change in a code comment.

Required new tests, at minimum:

**Unit tests** (`crates/node/src/evm.rs`, `executor.rs`)

1. `Database::basic` returns the right balance and nonce for a funded account.
2. `Database::basic` returns `None` or an empty account for an unknown address — be
   deliberate about which, it affects how REVM treats the account.
3. `DatabaseCommit` persists a balance change, a nonce change, new code, and a storage
   write.
4. A transfer through REVM costs exactly 21,000 gas.

**Integration tests** (`crates/node/tests/`)

5. Deploy a contract; assert `contract_address` is `Some` and code is committed.
6. Call the contract; assert storage changed and a log was emitted.
7. A reverting transaction is mined with `status: 0x0`, the sender is charged, and the
   nonce advances.
8. Supply conservation still holds: total ETH equals genesis plus block rewards. (This
   test already exists. It must still pass.)
9. A block never exceeds `BLOCK_GAS_LIMIT` given transactions of varying cost.

**RPC tests** (`crates/rpc/tests/`)

10. `eth_getCode` returns the runtime bytecode.
11. `eth_getStorageAt` returns the stored value as a 32-byte hex quantity.
12. `eth_call` returns the right output **and** leaves state unchanged.
13. `eth_estimateGas` returns a sufficient estimate — send a transaction with exactly
    that gas and assert it succeeds.
14. `eth_getLogs` finds a log by block range and by address.

Follow the existing style: the harness in `crates/rpc/tests/json_rpc.rs` starts a real
server on an ephemeral port, and `crates/node/src/testing.rs` shows how to sign.
Extend `testing.rs` with helpers for signing deployments and contract calls — you will
need them repeatedly.


## Grading

| Weight | Criterion |
| --- | --- |
| 25% | **Correct REVM integration.** `Database`/`DatabaseCommit` are right; state round-trips through the EVM faithfully |
| 20% | **Contracts work.** Deploy, call, storage, logs, and reverts all behave correctly end to end |
| 15% | **RPC completeness.** `eth_call`, `eth_estimateGas`, `eth_getCode`, `eth_getStorageAt`, `eth_getLogs` |
| 15% | **Tests.** Coverage of the list above; tests that would actually catch regressions |
| 10% | **No silent breakage.** Supply conservation, nonce handling, and gas accounting all still hold. Pitfalls 1–3 avoided |
| 10% | **`EVM-NOTES.md`.** Clear reasoning about what you decided and why; honest about what does not work |
| 5% | **Craft.** Clean clippy, consistent style, comments that explain *why* |

**Marked separately, and heavily:** honesty. A write-up that says "`eth_getLogs`
topic filtering is unimplemented, here is where I got stuck and what I think the fix
is" scores better than code that silently returns an empty array. Shipping something
broken while claiming it works is the one thing that will cost you badly.


## Appendix A: REVM reference

**Verified against REVM 43.0.3.** Every snippet below was compiled and run.

### Dependency

```toml
# crates/node/Cargo.toml
[dependencies]
revm = "43"
```

### A complete `Database` implementation

This is deliberately shaped like Breeja's state so you can see the mapping. Adapt it
to whatever representation you chose in M1.

```rust
use std::collections::HashMap;
use std::convert::Infallible;

use revm::{
    Database, DatabaseCommit,
    primitives::{Address, AddressMap, B256, StorageKey, StorageValue, U256},
    state::{Account, AccountInfo, Bytecode},
};

#[derive(Debug, Clone, Default)]
pub struct EvmAccount {
    pub nonce: u64,
    pub balance: U256,
    pub code: Option<Bytecode>,
    pub storage: HashMap<StorageKey, StorageValue>,
}

/// A snapshot of chain state that REVM can read from and write back to.
#[derive(Debug, Default)]
pub struct BreejaDb {
    pub accounts: HashMap<Address, EvmAccount>,
    pub code_by_hash: HashMap<B256, Bytecode>,
    pub block_hashes: HashMap<u64, B256>,
}

impl Database for BreejaDb {
    // Reading from an in-memory map cannot fail. A database backed by disk would
    // use a real error type here.
    type Error = Infallible;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.accounts.get(&address).map(|a| {
            let code = a.code.clone().unwrap_or_default();
            AccountInfo {
                balance: a.balance,
                nonce: a.nonce,
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            }
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.code_by_hash.get(&code_hash).cloned().unwrap_or_default())
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        Ok(self
            .accounts
            .get(&address)
            .and_then(|a| a.storage.get(&index).copied())
            .unwrap_or_default())
    }

    /// Backs the BLOCKHASH opcode. Breeja has the data — wire it to your chain.
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.block_hashes.get(&number).copied().unwrap_or_default())
    }
}

impl DatabaseCommit for BreejaDb {
    fn commit(&mut self, changes: AddressMap<Account>) {
        for (address, account) in changes {
            // REVM reports every account it looked at. Only touched ones changed.
            if !account.is_touched() {
                continue;
            }
            if account.is_selfdestructed() {
                self.accounts.remove(&address);
                continue;
            }

            let entry = self.accounts.entry(address).or_default();
            entry.balance = account.info.balance;
            entry.nonce = account.info.nonce;

            if let Some(code) = account.info.code.clone() {
                if !code.is_empty() {
                    self.code_by_hash.insert(account.info.code_hash, code.clone());
                    entry.code = Some(code);
                }
            }

            for (slot, value) in account.storage {
                entry.storage.insert(slot, value.present_value);
            }
        }
    }
}
```

Two details worth pausing on:

- **`is_touched()`** — REVM hands back every account it *read*, not just the ones it
  changed. Committing untouched accounts is harmless here but wasteful, and it would
  corrupt a real node's change-tracking.
- **`is_selfdestructed()`** — `SELFDESTRUCT` removes an account entirely. Breeja has
  no concept of deleting an account; now you do.

### Executing a transaction

```rust
use revm::{
    Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    primitives::{Address, Bytes, TxKind, U256, hardfork::SpecId},
};

pub fn execute(db: &mut BreejaDb, block: &MyBlock, tx: &MyTx) -> MyOutcome {
    let block_env = BlockEnv {
        number: U256::from(block.number),
        beneficiary: block.miner,          // REVM pays this address the fee
        timestamp: U256::from(block.timestamp),
        gas_limit: block.gas_limit,
        basefee: 0,                        // pre-EIP-1559: no burn (Pitfall 2)
        difficulty: U256::from(block.difficulty),
        prevrandao: Some(block.parent_hash),  // Pitfall 7
        ..Default::default()
    };

    let cfg = CfgEnv::new_with_spec(SpecId::SHANGHAI)   // Pitfall 5
        .with_chain_id(CHAIN_ID);

    let tx_env = TxEnv {
        tx_type: 0,                        // legacy
        caller: tx.from,
        gas_limit: tx.gas,
        gas_price: tx.gas_price,
        kind: match tx.to {
            Some(to) => TxKind::Call(to),
            None => TxKind::Create,        // contract deployment
        },
        value: tx.value,
        data: tx.input.clone(),
        nonce: tx.nonce,
        chain_id: Some(CHAIN_ID),
        ..Default::default()
    };

    let mut evm = Context::mainnet()
        .with_db(&mut *db)                 // &mut so you keep ownership
        .with_block(block_env)
        .with_cfg(cfg)
        .build_mainnet();

    // An Err here is a *database* or *validity* failure, not a revert.
    let outcome = evm.transact(tx_env).expect("handle this properly");

    let result = outcome.result;
    let summary = MyOutcome {
        success: result.is_success(),
        gas_used: result.tx_gas_used(),       // NOT gas_used() (Pitfall 4)
        created: result.created_address(),    // Some(..) for a deployment
        logs: result.logs().to_vec(),
        output: result.output().cloned().unwrap_or_default(),
    };

    let state = outcome.state.clone();
    drop(evm);                                // release the borrow (Pitfall 6)
    db.commit(state);                         // for eth_call: SKIP this line

    summary
}
```

### Reading the result

```rust
use revm::context_interface::result::{ExecutionResult, Output};

match &result {
    // The transaction ran to completion.
    ExecutionResult::Success { reason, output, .. } => {
        if let Output::Create(_code, Some(address)) = output {
            // a contract was deployed at `address`
        }
    }
    // REVERT: the contract rejected it. Still mined, sender still pays.
    // `output` is the revert reason, if the contract gave one.
    ExecutionResult::Revert { output, .. } => { /* receipt status = 0 */ }
    // Something worse: out of gas, invalid opcode, stack overflow.
    // Consumes the entire gas limit.
    ExecutionResult::Halt { reason, .. } => { /* receipt status = 0 */ }
}
```

Convenience accessors that work across all three variants, and are usually what you
want: `is_success()`, `is_halt()`, `tx_gas_used()`, `logs()`, `output()`,
`created_address()`.

**The distinction that matters for receipts:** `Success` is `status: 0x1`. Both
`Revert` and `Halt` are `status: 0x0`. Both are still mined.

### Verified reference numbers

Use these to check your work. All measured on REVM 43 with `SpecId::SHANGHAI`,
`basefee = 0`, `gas_price = 1 gwei`:

| Operation | Gas | Notes |
| --- | --- | --- |
| Plain ETH transfer, no calldata | **21,000** | Exact parity with Breeja's constant |
| Deploying the M3 test contract | ~55,522 | `created_address()` is `Some` |
| Calling it (SSTORE + LOG0) | ~43,487 | 1 log, storage slot 0 becomes 42 |

And the beneficiary check from Pitfall 1: after one 21,000-gas transfer at 1 gwei with
`basefee = 0`, `BlockEnv.beneficiary`'s balance increases by exactly
`21_000_000_000_000` wei, paid by REVM.

---

## Appendix B: Writing your own EVM

You may write the interpreter yourself. It is a genuinely rewarding project and you
will understand the EVM far better than the REVM route gives you. Be realistic about
the cost.

**The deal:** everything in this document still applies except Appendix A. Same
milestones, same tests, same deliverables. You implement the interpreter in place of
REVM. I will help you with integration, state, gas accounting, and RPC. **I will not
debug your opcode implementations** — that is the part you are choosing to own.

**Minimum viable EVM for this project:**

1. **A stack machine** — 1024-deep, 256-bit words. `U256` from `alloy` gives you the
   arithmetic.
2. **Memory** — byte-addressable, expands in 32-byte words, and expansion costs gas
   *quadratically*. Getting this wrong is the most common source of gas mismatches.
3. **Storage** — per-contract key/value, with the warm/cold access accounting from
   EIP-2929. This is where the gas rules are most intricate.
4. **An opcode set.** You do not need all ~140. A realistic minimum to run simple
   Solidity:
   - Arithmetic: `ADD` `MUL` `SUB` `DIV` `MOD` `EXP` and the signed variants
   - Comparison and bitwise: `LT` `GT` `EQ` `ISZERO` `AND` `OR` `XOR` `NOT` `SHL` `SHR`
   - `KECCAK256`
   - Context: `ADDRESS` `BALANCE` `CALLER` `CALLVALUE` `CALLDATALOAD` `CALLDATASIZE`
     `CALLDATACOPY` `CODESIZE` `CODECOPY` `GASPRICE`
   - Block: `BLOCKHASH` `COINBASE` `TIMESTAMP` `NUMBER` `PREVRANDAO` `GASLIMIT` `CHAINID`
   - Stack/memory/storage: `POP` `MLOAD` `MSTORE` `MSTORE8` `SLOAD` `SSTORE`
     `PUSH0`–`PUSH32` `DUP1`–`DUP16` `SWAP1`–`SWAP16`
   - Control flow: `JUMP` `JUMPI` `JUMPDEST` `PC` `GAS` `STOP` `RETURN` `REVERT` `INVALID`
   - Logs: `LOG0`–`LOG4`
   - Calls: `CALL` `STATICCALL` `DELEGATECALL` `CREATE` `RETURNDATASIZE` `RETURNDATACOPY`
5. **Jump destination validation.** A `JUMP` may only land on a `JUMPDEST` that is not
   inside `PUSH` data. Pre-compute the valid set per contract.
6. **Call frames.** `CALL` and `CREATE` recurse, with their own stack, memory, and gas
   budget — and the 63/64ths gas forwarding rule from EIP-150.
7. **Revert semantics.** A reverted frame must undo *all* its state changes — storage,
   balances, nonces, created accounts — while preserving the gas already spent. A
   journal of changes with checkpoints is the usual approach. REVM calls this `Journal`;
   look at how it works even if you do not use it.

**What you may skip** (say so in your notes): precompiles, blob transactions,
EIP-7702 delegation, `SELFDESTRUCT`, and transient storage (`TLOAD`/`TSTORE`).

**Strongly recommended:** run the
[official Ethereum execution tests](https://github.com/ethereum/tests). Even a small
subset will find bugs your own tests never will. A hand-written EVM that passes a
slice of the official suite is an outstanding final project.

**My honest advice:** do the REVM integration first and get it working. *Then*, if you
have time, swap in your own interpreter behind the same `Database` interface. You get a
working node either way, plus a reference implementation to diff your gas numbers
against — which is worth a great deal when your `MSTORE` is off by 3 gas and you cannot
see why.

## Stretch goals

Only after the required milestones pass.

1. **Precompiles.** `ecrecover`, `sha256`, `ripemd160`, `identity`, and the modexp and
   BN254 curve operations. REVM gives you these for free; writing your own is a
   serious undertaking.
2. **A real fee market.** Implement EIP-1559: a per-block base fee that adjusts with
   demand, priority fees, and the burn. Then order the mempool by effective tip and
   watch a fee market emerge.
3. **A Merkle-Patricia trie** for state and storage roots.
   `algo/src/dsa/trees/patricia_trie.rs` in this repository is a starting point. Then
   serve real inclusion proofs via `eth_getProof`.
4. **Tracing.** REVM's `Inspector` trait lets you observe every opcode. Build
   `debug_traceTransaction` and watch a Solidity function execute step by step. This is
   the single best debugging tool you can build for yourself.
5. **Persistence.** Write state to disk so a restart does not start a new chain. Once
   contracts hold storage, losing state on restart hurts.
6. **A second node.** Gossip blocks over TCP, validate incoming ones with
   `pow::verify`, and apply the heaviest-chain rule using `total_difficulty`. This is
   where reorgs appear — and reorgs plus contract storage is where you learn why state
   management is the hard part of a client.

## Getting help

**Before you ask, have ready:** what you expected, what happened, the exact error, and
the smallest test that reproduces it. "My contract call reverts" is not answerable;
"this 11-byte contract deploys but calling it halts with `OutOfGas` at 43,000 gas, here
is the test" is.

**Good sources:**

- REVM's own `examples/` directory — the most reliable documentation that exists, and
  it is guaranteed to match your version
- [REVM book](https://bluealloy.github.io/revm/)
- [evm.codes](https://www.evm.codes/) — every opcode with its gas cost and semantics.
  Indispensable for Appendix B
- [Ethereum Yellow Paper](https://ethereum.github.io/yellowpaper/paper.pdf) — the
  formal spec. Hard going, but definitive on gas
- [execution-specs](https://github.com/ethereum/execution-specs) — the spec as readable
  Python, often clearer than the Yellow Paper

**A warning about AI assistants and search results on this one.** REVM's API has
changed substantially across major versions, and most material you will find targets
an older one. If a snippet does not compile, it is probably version drift rather than
your mistake. The authority is the source in
`~/.cargo/registry/src/*/revm-43.0.3/` — read it. Learning to read a dependency's
source instead of hunting for a tutorial is, genuinely, one of the more valuable
things this project will teach you.

Good luck. You are building the part of Ethereum that everyone else treats as magic.
