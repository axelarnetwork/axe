//! Deployment salts follow the release, but historical Interop signers and addresses do not.
//! Those keys are not controlled by this deployment team. Use the configured role keys.
//! predictions, journaled nonces and contract wiring checks bind the resulting addresses.
use eyre::Result;

use crate::{state::State, types::Network};

pub fn salt<'a>(network: Network, name: &'a str, configured: &'a str) -> &'a str {
    if network == Network::Mainnet {
        name
    } else {
        configured
    }
}

pub fn validate(state: &State) -> Result<()> {
    if state.env == Network::Mainnet {
        eyre::ensure!(
            state.its_salt.as_deref() == Some("v2.2.0")
                && state.its_proxy_salt.as_deref() == Some("v1.0.0"),
            "mainnet requires ITS_SALT=v2.2.0 and ITS_PROXY_SALT=v1.0.0 for this release"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
