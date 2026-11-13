use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use clap::ValueEnum;
use eyre::{Result, WrapErr, eyre};
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

/// The EVM chain a route starts from.
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

/// Who the quote is for, and whether anything is sent.
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

    /// The signer when a key was given, else a dry run for `sender`.
    pub fn resolve(
        dry_run: bool,
        sender: Option<Address>,
        private_key: Option<String>,
    ) -> Result<Self> {
        let signer = private_key
            .filter(|key| !key.trim().is_empty())
            .map(|key| key.parse::<PrivateKeySigner>())
            .transpose()
            .wrap_err("the EVM private key is not valid hex")?;
        match (dry_run, sender, signer) {
            (true, Some(sender), _) => Ok(Self::DryRun(sender)),
            (true, None, Some(signer)) => Ok(Self::DryRun(signer.address())),
            (true, None, None) => Err(eyre!(
                "a dry run needs --sender or an EVM key (EVM_PRIVATE_KEY, PRIVATE_KEY, or --private-key)"
            )),
            (false, _, Some(signer)) => Ok(Self::Signer(signer)),
            (false, _, None) => Err(eyre!(
                "the rfq check needs an EVM signing key; set EVM_PRIVATE_KEY or PRIVATE_KEY, or pass --private-key"
            )),
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
    pub route: RfqRoute,
    pub source: RfqSource,
    /// Source amount in base units, or `None` for the route's default.
    pub amount: Option<U256>,
    /// Destination recipient as typed, or `None` for the route's default.
    pub recipient: Option<String>,
    pub timeout: Duration,
    pub rfq_url: Option<String>,
    /// Chains config for the source RPC, or `None` to resolve it from the network.
    pub config: Option<PathBuf>,
    pub wallet: Wallet,
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_runs_prefer_the_named_sender_and_need_no_key() {
        let sender = Address::from([7u8; 20]);
        let signer = PrivateKeySigner::random();
        let key = signer.to_bytes().to_string();

        let wallet = Wallet::resolve(true, Some(sender), Some(key.clone())).unwrap();
        assert!(matches!(wallet, Wallet::DryRun(address) if address == sender));

        let wallet = Wallet::resolve(true, None, Some(key)).unwrap();
        assert!(matches!(wallet, Wallet::DryRun(address) if address == signer.address()));

        assert!(Wallet::resolve(true, None, None).is_err());
        assert!(Wallet::resolve(true, None, Some("  ".to_owned())).is_err());
    }

    #[test]
    fn execution_needs_a_valid_key() {
        let signer = PrivateKeySigner::random();
        let wallet = Wallet::resolve(false, None, Some(signer.to_bytes().to_string())).unwrap();

        assert_eq!(wallet.address(), signer.address());
        assert!(matches!(wallet, Wallet::Signer(_)));
        assert!(Wallet::resolve(false, None, None).is_err());
        assert!(Wallet::resolve(false, None, Some("not hex".to_owned())).is_err());
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
