use std::collections::BTreeMap;
use std::path::PathBuf;

use alloy::signers::local::PrivateKeySigner;
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::configuration;
use crate::config_source;
use crate::cosmos::{derive_axelar_wallet, read_axelar_config};
use crate::state::{State, default_steps, save_state, state_path};
use crate::types::{ChainKey, Network};
use crate::ui;

fn print_deployer_addresses(state: &State) -> Result<()> {
    ui::section("Deployer Addresses");
    let (_, axelar_address) = derive_axelar_wallet(&state.mnemonic)?;
    ui::address("axelar deployer", &axelar_address);

    if let Some(mnemonic) = &state.admin_mnemonic {
        let (_, address) = derive_axelar_wallet(mnemonic)?;
        ui::address("prover admin", &address);
    }
    for (label, key) in [
        ("deployer", &state.deployer_private_key),
        ("gateway deployer", &state.gateway_deployer_private_key),
        (
            "gas service deployer",
            &state.gas_service_deployer_private_key,
        ),
        ("ITS deployer", &state.its_deployer_private_key),
    ] {
        if let Some(private_key) = key {
            let signer: PrivateKeySigner = private_key
                .parse()
                .map_err(|_| eyre::eyre!("invalid {label} private key"))?;
            ui::address(label, &format!("{}", signer.address()));
        }
    }
    if let Some(salt) = &state.its_salt {
        ui::kv("ITS salt", salt);
    }
    if let Some(salt) = &state.its_proxy_salt {
        ui::kv("ITS proxy salt", salt);
    }
    Ok(())
}

async fn print_axelar_balance(target_json: &std::path::Path, axelar_address: &str) -> Result<()> {
    if !target_json.exists() {
        return Ok(());
    }
    let (lcd, _, fee_denom, _) = read_axelar_config(target_json).await?;
    let url = format!("{lcd}/cosmos/bank/v1beta1/balances/{axelar_address}");
    match crate::http::client().get(&url).send().await {
        Ok(response) => {
            let data: Value = response.json().await?;
            if let Some(balances) = data["balances"].as_array() {
                let balance = balances
                    .iter()
                    .find(|entry| entry["denom"].as_str() == Some(&fee_denom))
                    .and_then(|entry| entry["amount"].as_str())
                    .unwrap_or("0");
                let display_denom = fee_denom.strip_prefix('u').unwrap_or(&fee_denom);
                let major = balance.parse::<f64>().unwrap_or(0.0) / 1_000_000.0;
                ui::kv("balance", &format!("{major:.6} {display_denom}"));
            }
        }
        Err(error) => ui::warn(&format!("could not query balance: {error}")),
    }
    Ok(())
}

pub async fn run() -> Result<()> {
    configuration::validate_init_environment(|name| std::env::var(name).ok())?;
    let require = |name: &str| -> Result<String> {
        std::env::var(name).map_err(|_| eyre::eyre!("missing required env var: {name}"))
    };

    let axelar_id = require("CHAIN")?;
    let state_file = state_path(&axelar_id)?;
    eyre::ensure!(
        !state_file.exists(),
        "deployment state already exists for '{axelar_id}'. Use `axe deploy run` to resume, or explicitly reset it with `axe deploy reset` before initializing again"
    );
    let chain_name = require("CHAIN_NAME")?;
    let chain_id: u64 = require("CHAIN_ID")?
        .parse()
        .map_err(|_| eyre::eyre!("CHAIN_ID must be a number"))?;
    let rpc_url = require("RPC_URL")?;
    let token_symbol = require("TOKEN_SYMBOL")?;
    let decimals: u8 = require("DECIMALS")?
        .parse()
        .map_err(|_| eyre::eyre!("DECIMALS must be a number"))?;
    let mnemonic = require("MNEMONIC")?;
    let env = require("ENV")?;
    let salt = require("SALT")?;

    let env_parsed: Network = env
        .parse()
        .map_err(|e| eyre::eyre!("invalid ENV value '{env}': {e}"))?;

    // TARGET_JSON stays the primary input; without it we fall back to the
    // sibling checkout (deploy writes back to the config, so a read-only
    // cached fetch is rejected).
    let target_json = match std::env::var_os("TARGET_JSON") {
        Some(path) => PathBuf::from(path),
        None => config_source::resolve(env_parsed, None)
            .await?
            .require_checkout()?,
    };

    let mut state = State {
        axelar_id: ChainKey::new(axelar_id.clone()),
        rpc_url: rpc_url.clone(),
        target_json: target_json.clone(),
        mnemonic: mnemonic.clone(),
        env: env_parsed,
        cosm_salt: salt,
        admin_mnemonic: None,
        deployer_private_key: None,
        gateway_deployer_private_key: None,
        gateway_deployer: None,
        gas_service_deployer_private_key: None,
        its_deployer_private_key: None,
        its_salt: None,
        its_proxy_salt: None,
        predicted_gateway_address: None,
        sender_receiver_address: None,
        proposals: BTreeMap::new(),
        steps: default_steps(),
    };

    configuration::load_missing_environment(&mut state, |name| std::env::var(name).ok());
    configuration::validate_state(&mut state, None)?;
    read_axelar_config(&target_json).await?;
    crate::steps::prover_admin::validate(&mut state).await?;
    crate::steps::cosmos_tx::check_instantiate_permissions(&state).await?;
    print_deployer_addresses(&state)?;
    write_chain_config(&state, &chain_name, chain_id, &token_symbol, decimals).await?;

    ui::section("State");
    save_state(&state).await?;
    ui::kv("state file", &state_file.display().to_string());
    ui::success(&format!("init complete for '{axelar_id}' (env={env})"));

    let (_, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    print_axelar_balance(&target_json, &axelar_address).await
}

async fn write_chain_config(
    state: &State,
    chain_name: &str,
    chain_id: u64,
    token_symbol: &str,
    decimals: u8,
) -> Result<()> {
    let axelar_id = state.axelar_id.as_str();
    let target_json = &state.target_json;
    let mut chain_entry = json!({
        "name": chain_name,
        "axelarId": axelar_id,
        "chainId": chain_id,
        "rpc": state.rpc_url,
        "tokenSymbol": token_symbol,
        "confirmations": 1,
        "finality": "finalized",
        "decimals": decimals,
        "approxFinalityWaitTime": 1,
        "chainType": "evm",
        "contracts": {}
    });
    if let (Ok(name), Ok(url)) = (
        std::env::var("EXPLORER_NAME"),
        std::env::var("EXPLORER_URL"),
    ) {
        chain_entry["explorer"] = json!({ "name": name, "url": url });
    }
    let content = tokio::fs::read_to_string(target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;
    let chains = root
        .get_mut("chains")
        .and_then(|c| c.as_object_mut())
        .ok_or_else(|| eyre::eyre!("no 'chains' object in {}", target_json.display()))?;
    if chains.contains_key(axelar_id) {
        ui::info(&format!(
            "chain '{axelar_id}' already exists in {}, skipping",
            target_json.display()
        ));
    } else {
        chains.insert(axelar_id.to_string(), chain_entry);
        tokio::fs::write(target_json, serde_json::to_string_pretty(&root)? + "\n").await?;
        ui::success(&format!(
            "added chain '{axelar_id}' to {}",
            target_json.display()
        ));
    }
    Ok(())
}
