use eyre::Result;

use crate::{cosmos::derive_axelar_wallet, state::State, types::Network};

#[cfg(test)]
mod tests;

pub(crate) const MAINNET_REWARDS_FUNDER: &str = "axelar1rhhzwt6nwuzgezelv987hz602rlyz9ej6psqa9";
const MAINNET_PROPOSER: &str = "axelar1ykl3n6xjh00nyynknr4yzxvhtwwkd5vhcwrxye";
const MAINNET_PROVER_ADMIN: &str = "axelar1w2ey0ek9e8q2dfmeznz6ah49zdywpdme0z0kly";

pub(crate) fn validate_mainnet(state: &State) -> Result<()> {
    if state.env != Network::Mainnet {
        return Ok(());
    }
    for (step, name, expected) in [
        ("InstantiateChainContracts", "MNEMONIC", MAINNET_PROPOSER),
        (
            "WaitForVerifierSet",
            "MULTISIG_PROVER_MNEMONIC",
            MAINNET_PROVER_ADMIN,
        ),
        (
            "AddRewards",
            "REWARDS_FUNDER_MNEMONIC",
            MAINNET_REWARDS_FUNDER,
        ),
    ] {
        let (_, address) = derive_axelar_wallet(mnemonic(state, step)?)
            .map_err(|_| eyre::eyre!("invalid {name}"))?;
        validate_address(name, &address, expected)?;
    }
    Ok(())
}

fn validate_address(name: &str, address: &str, expected: &str) -> Result<()> {
    eyre::ensure!(
        address == expected,
        "{name} derives {address}, but mainnet requires {expected}. Supply that account's mnemonic before deploying"
    );
    Ok(())
}

pub(crate) fn mnemonic<'a>(state: &'a State, step: &str) -> Result<&'a str> {
    let (name, value) = match step {
        "AddRewards" => (
            "REWARDS_FUNDER_MNEMONIC",
            state.rewards_funder_mnemonic.as_deref(),
        ),
        "WaitForVerifierSet" => ("MULTISIG_PROVER_MNEMONIC", state.admin_mnemonic.as_deref()),
        "InstantiateChainContracts" | "RegisterItsOnHub" => {
            ("MNEMONIC", Some(state.mnemonic.as_str()))
        }
        _ => {
            eyre::bail!("unknown Cosmos signing role for step {step}");
        }
    };
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| eyre::eyre!("missing {name}: explicitly supply the signer for {step}"))
}

pub(crate) fn rewards_funder_address(state: &State) -> Result<String> {
    let (_, address) = derive_axelar_wallet(mnemonic(state, "AddRewards")?)
        .map_err(|_| eyre::eyre!("invalid REWARDS_FUNDER_MNEMONIC"))?;
    validate_rewards_funder(state.env, &address)?;
    Ok(address)
}

fn validate_rewards_funder(network: Network, address: &str) -> Result<()> {
    if network == Network::Mainnet {
        validate_address("REWARDS_FUNDER_MNEMONIC", address, MAINNET_REWARDS_FUNDER)?;
    }
    Ok(())
}
