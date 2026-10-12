use std::time::Instant;

use alloy::primitives::B256;
use primitives::node::{BlockHeader, meets_difficulty};

/// A sealed proof of work: the nonce that solved the puzzle, and what it cost.
#[derive(Debug, Clone, Copy)]
pub struct Seal {
    pub nonce: u64,
    pub hash: B256,
    /// How many hashes were tried before this one worked.
    pub attempts: u64,
    pub elapsed_secs: f64,
}

impl Seal {
    /// Hashes per second achieved while finding this seal.
    pub fn hash_rate(&self) -> f64 {
        if self.elapsed_secs <= 0.0 {
            return 0.0;
        }
        self.attempts as f64 / self.elapsed_secs
    }
}

/// Searches for a nonce that makes `header` hash below the difficulty target.
///
/// `should_stop` is polled periodically so a shutdown signal does not have to
/// wait for the puzzle to be solved.
pub fn mine(
    header: &mut BlockHeader,
    difficulty: u8,
    should_stop: &dyn Fn() -> bool,
) -> Option<Seal> {
    /// How often to check `should_stop`. Checking every attempt would cost more
    /// than the hashing does.
    const STOP_CHECK_INTERVAL: u64 = 50_000;

    let started = Instant::now();
    let mut attempts: u64 = 0;

    for nonce in 0..=u64::MAX {
        header.nonce = nonce;
        let hash = header.hash();
        attempts += 1;

        if meets_difficulty(&hash, difficulty) {
            return Some(Seal {
                nonce,
                hash,
                attempts,
                elapsed_secs: started.elapsed().as_secs_f64(),
            });
        }

        if attempts.is_multiple_of(STOP_CHECK_INTERVAL) && should_stop() {
            return None;
        }
    }

    None
}

/// Re-derives a block's hash and checks the proof of work.
///
/// This is the half of proof of work every node runs: it never mines, but it
/// refuses any block whose nonce does not actually solve the puzzle. A miner that
/// lies about its hash is caught here.
pub fn verify(
    header: &BlockHeader,
    claimed_hash: &B256,
    difficulty: u8,
) -> Result<(), anyhow::Error> {
    let computed = header.hash();
    if &computed != claimed_hash {
        anyhow::bail!(
            "block hash mismatch: header hashes to {computed}, block claims {claimed_hash}"
        );
    }
    if !meets_difficulty(&computed, difficulty) {
        anyhow::bail!(
            "block hash {computed} does not meet difficulty of {difficulty} leading zero bits"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, Bytes};
    use primitives::node::leading_zero_bits;

    use super::*;

    fn test_header() -> BlockHeader {
        BlockHeader {
            number: 1,
            parent_hash: B256::ZERO,
            state_root: B256::ZERO,
            transaction_root: B256::ZERO,
            receipts_root: B256::ZERO,
            difficulty: 8,
            timestamp: 1_700_000_000,
            gas_limit: 3_141_592,
            gas_used: 0,
            miner: Address::ZERO,
            extra_data: Bytes::from_static(b"breeja"),
            nonce: 0,
        }
    }

    #[test]
    fn mining_finds_a_nonce_whose_hash_meets_the_difficulty() {
        let mut header = test_header();
        let seal = mine(&mut header, 12, &|| false).expect("12 bits is quick to solve");

        assert!(leading_zero_bits(&seal.hash) >= 12);
        assert!(seal.attempts > 0);

        // The seal is reproducible: the nonce is all another node needs.
        header.nonce = seal.nonce;
        assert_eq!(header.hash(), seal.hash);
    }

    #[test]
    fn harder_difficulty_costs_more_attempts() {
        let easy = mine(&mut test_header(), 4, &|| false).unwrap();
        let hard = mine(&mut test_header(), 16, &|| false).unwrap();
        assert!(
            hard.attempts > easy.attempts,
            "16 bits took {} attempts, 4 bits took {}",
            hard.attempts,
            easy.attempts
        );
    }

    #[test]
    fn verification_accepts_a_real_seal_and_rejects_a_forged_one() {
        let mut header = test_header();
        let seal = mine(&mut header, 12, &|| false).unwrap();
        header.nonce = seal.nonce;

        assert!(verify(&header, &seal.hash, 12).is_ok());

        // Claiming a hash the header does not produce is caught.
        assert!(verify(&header, &B256::ZERO, 12).is_err());

        // So is a real hash that simply is not good enough.
        assert!(verify(&header, &seal.hash, 250).is_err());
    }

    #[test]
    fn mining_gives_up_when_told_to_stop() {
        // 64 leading zero bits would take longer than this test has, so the only
        // way this returns is via the stop signal.
        let result = mine(&mut test_header(), 64, &|| true);
        assert!(result.is_none());
    }
}
