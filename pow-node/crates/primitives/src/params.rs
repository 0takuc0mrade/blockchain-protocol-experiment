use alloy::primitives::{Address, Bytes, U256};
use serde::{Deserialize, Serialize};

/// Which block a state query refers to.
///
/// Breeja has no reorgs and no notion of finality — once a block is mined it is
/// final — so `Safe` and `Finalized` resolve to the same block as `Latest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockTag {
    Earliest,
    Latest,
    Pending,
    Safe,
    Finalized,
    Number(u64),
}

impl BlockTag {
    /// Parses a block parameter as it arrives over JSON-RPC.
    pub fn parse(raw: &str) -> Result<Self, anyhow::Error> {
        match raw {
            "earliest" => Ok(Self::Earliest),
            "latest" => Ok(Self::Latest),
            "pending" => Ok(Self::Pending),
            "safe" => Ok(Self::Safe),
            "finalized" => Ok(Self::Finalized),
            other => {
                let hex = other.strip_prefix("0x").ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid block parameter {other:?}: expected a block tag or a 0x-prefixed number"
                    )
                })?;
                let number = u64::from_str_radix(hex, 16)
                    .map_err(|e| anyhow::anyhow!("invalid block number {other:?}: {e}"))?;
                Ok(Self::Number(number))
            }
        }
    }

    /// Resolves this tag against a chain whose head is at `head`.
    ///
    /// `Pending` resolves to the head too: the pending block does not exist until
    /// the miner seals it, and Breeja answers pending account queries from the
    /// mempool instead.
    pub fn resolve(&self, head: u64) -> Result<u64, anyhow::Error> {
        match self {
            Self::Earliest => Ok(0),
            Self::Latest | Self::Pending | Self::Safe | Self::Finalized => Ok(head),
            Self::Number(number) if *number <= head => Ok(*number),
            Self::Number(number) => {
                anyhow::bail!("block {number} does not exist yet; chain head is {head}")
            }
        }
    }
}

impl<'de> Deserialize<'de> for BlockTag {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The transaction-shaped object `eth_call` and `eth_estimateGas` take.
///
/// Every field is optional, because a caller may omit anything the node can
/// default. `data` and `input` are the same field under two names; Ethereum
/// clients accept both, so Breeja does too.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRequest {
    pub from: Option<Address>,
    pub to: Option<Address>,
    pub gas: Option<U256>,
    pub gas_price: Option<U256>,
    pub value: Option<U256>,
    pub data: Option<Bytes>,
    pub input: Option<Bytes>,
}

impl CallRequest {
    /// The calldata this request carries, under whichever name it was sent.
    ///
    /// Breeja uses this to decide whether it can answer at all: calldata means
    /// contract code, and there is no EVM to run it.
    pub fn calldata(&self) -> Bytes {
        self.data
            .clone()
            .or_else(|| self.input.clone())
            .unwrap_or_default()
    }

    /// Whether this request needs an EVM to answer.
    pub fn needs_evm(&self) -> bool {
        !self.calldata().is_empty() || self.to.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_block_tag_ethereum_defines() {
        assert_eq!(BlockTag::parse("latest").unwrap(), BlockTag::Latest);
        assert_eq!(BlockTag::parse("earliest").unwrap(), BlockTag::Earliest);
        assert_eq!(BlockTag::parse("pending").unwrap(), BlockTag::Pending);
        assert_eq!(BlockTag::parse("safe").unwrap(), BlockTag::Safe);
        assert_eq!(BlockTag::parse("finalized").unwrap(), BlockTag::Finalized);
        assert_eq!(BlockTag::parse("0x1f").unwrap(), BlockTag::Number(31));
        assert!(BlockTag::parse("31").is_err());
        assert!(BlockTag::parse("0xzz").is_err());
    }

    #[test]
    fn resolves_tags_against_the_chain_head() {
        assert_eq!(BlockTag::Latest.resolve(9).unwrap(), 9);
        assert_eq!(BlockTag::Pending.resolve(9).unwrap(), 9);
        assert_eq!(BlockTag::Earliest.resolve(9).unwrap(), 0);
        assert_eq!(BlockTag::Number(4).resolve(9).unwrap(), 4);
        // A block that has not been mined yet is an error, not a silent default.
        assert!(BlockTag::Number(10).resolve(9).is_err());
    }

    #[test]
    fn a_call_needs_an_evm_when_it_carries_calldata_or_creates_a_contract() {
        let transfer = CallRequest {
            to: Some(Address::ZERO),
            ..Default::default()
        };
        assert!(!transfer.needs_evm());

        let contract_call = CallRequest {
            to: Some(Address::ZERO),
            data: Some(Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef])),
            ..Default::default()
        };
        assert!(contract_call.needs_evm());

        // No `to` at all is a contract deployment.
        let deployment = CallRequest::default();
        assert!(deployment.needs_evm());
    }
}
