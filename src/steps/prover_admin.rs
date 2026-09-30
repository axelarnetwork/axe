use base64::Engine;
use eyre::{Result, WrapErr};

use crate::config::ChainsConfig;
use crate::cosmos::derive_axelar_wallet;
use crate::state::{State, StepStatus};
use crate::types::Network;

mod types;

#[cfg(test)]
mod tests;

use types::{ProverConfig, RawResponse};

pub(super) fn default_address(env: Network) -> &'static str {
    match env {
        Network::DevnetAmplifier => "axelar1zlr7e5qf3sz7yf890rkh9tcnu87234k6k7ytd9",
        Network::Testnet => "axelar1w7y7v26rtnrj4vrx6q3qq4hfsmc68hhsxnadlf",
        Network::Mainnet => "axelar1pczf792wf3p3xssk4dmwfxrh6hcqnrjp70danj",
        Network::Stagenet => "axelar1l7vz4m5g92kvga050vk9ycjynywdlk4zhs07dv",
    }
}

pub(super) fn planned_address(env: Network, configured: Option<&str>) -> Result<&str> {
    match env {
        Network::Testnet => Ok(default_address(env)),
        _ => configured
            .filter(|address| !address.trim().is_empty())
            .ok_or_else(|| eyre::eyre!("no adminAddress in MultisigProver config for {env}")),
    }
}

pub(crate) async fn validate(state: &mut State) -> Result<()> {
    if !state
        .steps
        .iter()
        .any(|step| step.name == "WaitForVerifierSet" && step.status == StepStatus::Pending)
    {
        return Ok(());
    }
    let config = ChainsConfig::load(&state.target_json).await?;
    let chain = config
        .chains
        .get(state.axelar_id.as_str())
        .and_then(|chain| chain.axelar_id.as_deref())
        .unwrap_or(state.axelar_id.as_str());
    let prover: ProverConfig = config
        .axelar
        .contracts
        .as_ref()
        .and_then(|contracts| contracts.get("MultisigProver"))
        .and_then(|provers| provers.get(chain))
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    let expected = if let Some(address) = prover.address {
        let lcd = config
            .axelar
            .lcd
            .as_deref()
            .ok_or_else(|| eyre::eyre!("no axelar.lcd"))?;
        query_admin(lcd, &address).await?
    } else if state
        .steps
        .iter()
        .any(|step| step.name == "AddCosmWasmConfig" && step.status == StepStatus::Pending)
    {
        default_address(state.env).to_string()
    } else {
        planned_address(state.env, prover.admin_address.as_deref())?.to_string()
    };
    state.admin_mnemonic = Some(select_mnemonic(
        state.admin_mnemonic.as_deref(),
        &state.mnemonic,
        &expected,
    )?);
    Ok(())
}

fn select_mnemonic(admin: Option<&str>, deployer: &str, expected: &str) -> Result<String> {
    let explicit = admin.filter(|mnemonic| !mnemonic.trim().is_empty());
    let mnemonic = explicit.unwrap_or(deployer);
    let name = if explicit.is_some() {
        "MULTISIG_PROVER_MNEMONIC"
    } else {
        "MNEMONIC"
    };
    let (_, derived) = derive_axelar_wallet(mnemonic)
        .map_err(|_| eyre::eyre!("invalid {name}: cannot derive the prover admin address"))?;
    eyre::ensure!(
        derived == expected,
        "prover admin key required before deployment: expected {expected}, but {name} derives {derived}. Set MULTISIG_PROVER_MNEMONIC to the mnemonic for {expected} and rerun `axe deploy run`. A mnemonic cannot be derived from an address"
    );
    Ok(mnemonic.to_string())
}

async fn query_admin(lcd: &str, prover: &str) -> Result<String> {
    let key =
        base64::engine::general_purpose::STANDARD.encode("permission_control_contract_admin_addr");
    let url = format!(
        "{}/cosmwasm/wasm/v1/contract/{prover}/raw/{key}",
        lcd.trim_end_matches('/')
    );
    let response: RawResponse = crate::http::client()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .wrap_err("could not read the on-chain prover admin")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(response.data)?;
    serde_json::from_slice(&bytes).wrap_err("missing or invalid on-chain prover admin")
}
