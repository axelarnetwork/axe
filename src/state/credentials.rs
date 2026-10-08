use alloy::signers::local::PrivateKeySigner;
use eyre::Result;

use super::State;
use crate::cosmos::derive_axelar_wallet;

/// Smoke tests need an EVM sender and Cosmos relay signer, not deployment roles.
pub fn load_test_credentials(
    state: &mut State,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    let supplied = |name| lookup(name).filter(|value| !value.trim().is_empty());
    if state
        .deployer_private_key
        .as_deref()
        .is_none_or(|key| key.trim().is_empty())
    {
        state.deployer_private_key =
            supplied("DEPLOYER_PRIVATE_KEY").or_else(|| supplied("EVM_PRIVATE_KEY"));
    }
    if state.mnemonic.trim().is_empty() {
        state.mnemonic = supplied("MNEMONIC").unwrap_or_default();
    }
    let mut missing = Vec::new();
    if state.deployer_private_key.is_none() {
        missing.push("DEPLOYER_PRIVATE_KEY (or EVM_PRIVATE_KEY) for EVM transactions");
    }
    if state.mnemonic.is_empty() {
        missing.push("MNEMONIC for Cosmos relay transactions");
    }
    eyre::ensure!(
        missing.is_empty(),
        "missing smoke-test credentials: {}. Set them in .env or the environment; credentials are not saved in deployment state",
        missing.join("; ")
    );
    let key = state
        .deployer_private_key
        .as_deref()
        .ok_or_else(|| eyre::eyre!("missing smoke-test EVM key"))?;
    key.parse::<PrivateKeySigner>()
        .map_err(|_| eyre::eyre!("invalid smoke-test EVM key; set DEPLOYER_PRIVATE_KEY (or EVM_PRIVATE_KEY) in .env or the environment"))?;
    derive_axelar_wallet(&state.mnemonic)
        .map_err(|_| eyre::eyre!("invalid smoke-test MNEMONIC; set it in .env or the environment for Cosmos relay transactions"))?;
    Ok(())
}
