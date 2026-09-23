use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use eyre::Result;

use crate::cosmos::derive_axelar_wallet;
use crate::state::State;

#[cfg(test)]
mod tests;

pub(crate) fn validate_init_environment(lookup: impl Fn(&str) -> Option<String>) -> Result<()> {
    let missing: Vec<_> = [
        "CHAIN",
        "CHAIN_NAME",
        "CHAIN_ID",
        "RPC_URL",
        "TOKEN_SYMBOL",
        "DECIMALS",
        "MNEMONIC",
        "ENV",
        "SALT",
        "DEPLOYER_PRIVATE_KEY",
        "GATEWAY_DEPLOYER_PRIVATE_KEY",
        "GAS_SERVICE_DEPLOYER_PRIVATE_KEY",
        "ITS_DEPLOYER_PRIVATE_KEY",
        "ITS_SALT",
        "ITS_PROXY_SALT",
    ]
    .into_iter()
    .filter(|name| lookup(name).is_none_or(|value| value.trim().is_empty()))
    .collect();
    eyre::ensure!(
        missing.is_empty(),
        "missing required deployment environment variables: {}",
        missing.join(", ")
    );
    Ok(())
}

pub(crate) fn load_missing_environment(state: &mut State, lookup: impl Fn(&str) -> Option<String>) {
    for (name, destination) in [
        ("DEPLOYER_PRIVATE_KEY", &mut state.deployer_private_key),
        (
            "GATEWAY_DEPLOYER_PRIVATE_KEY",
            &mut state.gateway_deployer_private_key,
        ),
        (
            "GAS_SERVICE_DEPLOYER_PRIVATE_KEY",
            &mut state.gas_service_deployer_private_key,
        ),
        (
            "ITS_DEPLOYER_PRIVATE_KEY",
            &mut state.its_deployer_private_key,
        ),
        ("ITS_SALT", &mut state.its_salt),
        ("ITS_PROXY_SALT", &mut state.its_proxy_salt),
    ] {
        if destination
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            *destination = lookup(name);
        }
    }
    if let Some(mnemonic) =
        lookup("MULTISIG_PROVER_MNEMONIC").filter(|mnemonic| !mnemonic.trim().is_empty())
    {
        state.admin_mnemonic = Some(mnemonic);
    }
}

pub(crate) fn validate_state(state: &mut State, key_override: Option<&str>) -> Result<()> {
    let mut errors = Vec::new();
    if !reqwest::Url::parse(&state.rpc_url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
    {
        errors.push("RPC_URL must be an HTTP or HTTPS URL".into());
    }
    for (name, value) in [
        ("MNEMONIC", Some(state.mnemonic.as_str())),
        ("SALT", Some(state.cosm_salt.as_str())),
        ("ITS_SALT", state.its_salt.as_deref()),
        ("ITS_PROXY_SALT", state.its_proxy_salt.as_deref()),
    ] {
        if value.is_none_or(|value| value.trim().is_empty()) {
            errors.push(format!("missing {name}"));
        }
    }
    if !state.mnemonic.trim().is_empty() && derive_axelar_wallet(&state.mnemonic).is_err() {
        errors.push("invalid MNEMONIC".into());
    }
    if let Some(mnemonic) = &state.admin_mnemonic
        && derive_axelar_wallet(mnemonic).is_err()
    {
        errors.push("invalid MULTISIG_PROVER_MNEMONIC".into());
    }
    for (name, key) in [
        (
            "DEPLOYER_PRIVATE_KEY",
            state.deployer_private_key.as_deref(),
        ),
        (
            "GAS_SERVICE_DEPLOYER_PRIVATE_KEY",
            state.gas_service_deployer_private_key.as_deref(),
        ),
        (
            "ITS_DEPLOYER_PRIVATE_KEY",
            state.its_deployer_private_key.as_deref(),
        ),
    ] {
        validate_key(name, key_override.or(key), &mut errors);
    }
    let gateway = validate_key(
        "GATEWAY_DEPLOYER_PRIVATE_KEY",
        key_override.or(state.gateway_deployer_private_key.as_deref()),
        &mut errors,
    );
    if let (Some(saved), Some(derived)) = (state.gateway_deployer, gateway)
        && saved != derived
    {
        errors.push("gateway deployer key does not match the saved gatewayDeployer address".into());
    }
    eyre::ensure!(
        errors.is_empty(),
        "deployment configuration is incomplete or invalid:\n  - {}\nSet the missing variables and retry. For an existing deployment, rerun `axe deploy run`, not `init`.",
        errors.join("\n  - ")
    );
    state.gateway_deployer = gateway;
    Ok(())
}

fn validate_key(name: &str, key: Option<&str>, errors: &mut Vec<String>) -> Option<Address> {
    let Some(key) = key.filter(|key| !key.trim().is_empty()) else {
        errors.push(format!("missing {name}"));
        return None;
    };
    match key.parse::<PrivateKeySigner>() {
        Ok(signer) => Some(signer.address()),
        Err(_) => {
            errors.push(format!("invalid {name}"));
            None
        }
    }
}
