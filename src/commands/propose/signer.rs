use cosmrs::crypto::secp256k1::SigningKey;
use eyre::Result;

use crate::{cosmos::derive_axelar_wallet, ui};

pub(super) fn load(variable: &str) -> Result<(SigningKey, String)> {
    ui::kv("Cosmos signing mnemonic variable", variable);
    selected(variable, |name| std::env::var(name).ok())
}

fn selected(
    variable: &str,
    lookup: impl FnOnce(&str) -> Option<String>,
) -> Result<(SigningKey, String)> {
    let mnemonic = lookup(variable)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| eyre::eyre!("{variable} is missing or empty. Set the variable selected by --mnemonic-env to the intended Cosmos signer's mnemonic"))?;
    derive_axelar_wallet(&mnemonic)
        .map_err(|_| eyre::eyre!("invalid mnemonic in {variable}: cannot derive the Cosmos signer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;

    #[test]
    fn explicitly_selected_wallet_wins_over_default() {
        let default = bip32::Mnemonic::from_entropy([42; 32], bip32::Language::English);
        let testnet = bip32::Mnemonic::from_entropy([43; 32], bip32::Language::English);
        let expected = derive_axelar_wallet(testnet.phrase()).unwrap().1;
        let (_, selected_address) = selected("TESTNET_PROPOSER_MNEMONIC", |name| match name {
            "TESTNET_PROPOSER_MNEMONIC" => Some(testnet.phrase().into()),
            "MNEMONIC" => Some(default.phrase().into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(selected_address, expected);
        assert_ne!(
            selected_address,
            derive_axelar_wallet(default.phrase()).unwrap().1
        );
    }

    #[test]
    fn invalid_override_never_falls_back_or_exposes_secret() {
        for value in [None, Some(""), Some("  "), Some("sensitive-invalid-phrase")] {
            let result = selected("TESTNET_PROPOSER_MNEMONIC", |name| {
                assert_eq!(name, "TESTNET_PROPOSER_MNEMONIC");
                value.map(str::to_owned)
            });
            let error = result.err().unwrap().to_string();
            assert!(error.contains("TESTNET_PROPOSER_MNEMONIC"));
            assert!(!error.contains("sensitive-invalid-phrase"));
        }
    }

    #[test]
    fn cli_selects_variable_name_and_preserves_default() {
        for network in ["testnet", "mainnet"] {
            for extra in [vec![], vec!["--mnemonic-env", "TESTNET_PROPOSER_MNEMONIC"]] {
                let mut args = vec!["axe", "propose", network, "robinhood"];
                args.extend(extra.clone());
                let cli = Cli::try_parse_from(args).unwrap();
                let Commands::Propose(args) = cli.command else {
                    panic!("wrong command")
                };
                assert_eq!(
                    args.mnemonic_env,
                    if extra.is_empty() {
                        "MNEMONIC"
                    } else {
                        "TESTNET_PROPOSER_MNEMONIC"
                    }
                );
            }
        }
    }
}
