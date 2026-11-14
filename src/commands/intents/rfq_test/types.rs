use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use clap::ValueEnum;
use serde::Serialize;
use solana_sdk::pubkey::Pubkey;

use crate::types::Network;

/// An rfq-api route the end-to-end check exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RfqRoute {
    /// The source chain's wrapped native token over ITS to the other EVM chain
    Its,
    /// Native gas, wrapped on the source, over ITS to the other EVM chain
    NativeWrap,
    /// The gateway USDC token to the other EVM chain, plain or express
    Gateway,
    /// The source chain's wrapped native token over ITS to Solana
    Solana,
}

impl Display for RfqRoute {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.to_possible_value()
            .map_or(Ok(()), |value| formatter.write_str(value.get_name()))
    }
}

/// The EVM chain the routes start from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RfqSource {
    /// Avalanche (Fuji on testnet)
    #[default]
    #[value(alias = "fuji")]
    Avalanche,
    /// Ethereum (Sepolia on testnet)
    #[value(alias = "sepolia")]
    Ethereum,
}

impl RfqSource {
    pub const fn other(self) -> Self {
        match self {
            Self::Avalanche => Self::Ethereum,
            Self::Ethereum => Self::Avalanche,
        }
    }
}

/// Who the quotes are for, and whether anything is sent.
pub enum Wallet {
    Signer(PrivateKeySigner),
    /// Quote for this address and send nothing.
    DryRun(Address),
}

impl Wallet {
    pub fn address(&self) -> Address {
        match self {
            Self::Signer(signer) => signer.address(),
            Self::DryRun(address) => *address,
        }
    }
}

/// Where the destination chain delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipient {
    Evm(Address),
    Solana(Pubkey),
}

impl Display for Recipient {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Evm(address) => address.fmt(formatter),
            Self::Solana(pubkey) => pubkey.fmt(formatter),
        }
    }
}

pub struct RfqTestArgs {
    pub network: Network,
    /// Routes to check, in the order their actions are sent.
    pub routes: Vec<RfqRoute>,
    pub source: RfqSource,
    /// Source amount in base units for a single route, or `None` for each
    /// route's default.
    pub amount: Option<U256>,
    /// Destination recipient for every route as typed, or `None` for each
    /// route's default.
    pub recipient: Option<String>,
    pub timeout: Duration,
    pub rfq_url: Option<String>,
    /// Chains config for the source RPC, or `None` to resolve it from the
    /// network.
    pub config: Option<PathBuf>,
    pub wallet: Wallet,
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_display_their_cli_names() {
        assert_eq!(RfqRoute::NativeWrap.to_string(), "native-wrap");
    }

    #[test]
    fn source_aliases_name_the_testnet_chains() {
        assert_eq!(
            RfqSource::from_str("fuji", false).unwrap(),
            RfqSource::Avalanche
        );
        assert_eq!(
            RfqSource::from_str("sepolia", false).unwrap(),
            RfqSource::Ethereum
        );
        assert_eq!(RfqSource::Avalanche.other(), RfqSource::Ethereum);
        assert_eq!(RfqSource::Ethereum.other(), RfqSource::Avalanche);
    }
}
