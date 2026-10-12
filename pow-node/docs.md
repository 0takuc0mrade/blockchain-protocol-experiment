## How it works

### Proof of work

A block header is RLP-encoded and hashed with keccak256. The miner tries nonce
after nonce until the hash starts with enough zero bits. There is no shortcut —
that search *is* the work.

Verification is a single hash. That asymmetry is the whole trick: a block that
cost millions of attempts to find takes microseconds to check, so any node can
reject a miner that lies about its work. `pow::verify` is the half every node
runs, and `every_block_is_a_valid_proof_of_work_linked_to_its_parent` checks it
across a freshly mined chain.

### Difficulty retargeting

Difficulty is a count of required leading zero bits, so one step is a *doubling*.
Breeja only adjusts when the last block's time was off by more than that factor
of two; otherwise the difficulty would oscillate forever around a target it can
never land on.

This is precisely why real Ethereum does not count zero bits. It adjusts a
256-bit difficulty *number* in steps of about 1/2048, so it tracks its target
closely instead of bracketing it. Watching Breeja settle at ±1 bit is a good way
to see why that resolution matters.

Expect wide variation in block times even at a settled difficulty — proof of work
is a memoryless search, so a block taking 4x the average is unremarkable.

### Execution

The state transition is in `crates/node/src/executor.rs`, and it is short enough
to read in one sitting: check the nonce, debit `value + fee` from the sender,
credit `value` to the recipient, credit the fee to the miner, bump the nonce.
Afterwards the miner is paid the block reward — the only way new ETH is ever
created.

### Signatures

A transaction never states who sent it. The sender's address is *recovered* from
the signature, in `pool.rs`. That is why transactions cannot be forged: producing
a signature that recovers to someone else's address means finding their private
key.

### Concurrency

The miner owns a thread; the RPC server runs on Tokio. Both hold a `Chain`, which
wraps the state in an `RwLock`. The miner writes account state and blocks; the RPC
server only appends to the mempool.

The important detail is *when the lock is held*. Mining can take many seconds, so
the miner snapshots state, releases the lock, hashes, and re-acquires the lock
only for the instant it takes to commit. The RPC server never blocks on mining.

The miner also abandons a block in progress when a new transaction arrives, rather
than making that transaction wait out the current search. Real miners do the same,
for the same reason.
