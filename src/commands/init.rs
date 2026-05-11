use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use alloy::signers::local::PrivateKeySigner;
use eyre::Result;
use serde_json::{Value, json};

use crate::cosmos::{derive_axelar_wallet, read_axelar_config};
use crate::state::{State, data_dir, default_steps, save_state, state_path};
use crate::types::{ChainKey, Network};
use crate::ui;

pub async fn run() -> Result<()> {
    let require = |name: &str| -> Result<String> {
        std::env::var(name).map_err(|_| eyre::eyre!("missing required env var: {name}"))
    };

    let axelar_id = require("CHAIN")?;
    let chain_name = require("CHAIN_NAME")?;
    let chain_id: u64 = require("CHAIN_ID")?
        .parse()
        .map_err(|_| eyre::eyre!("CHAIN_ID must be a number"))?;
    let rpc_url = require("RPC_URL")?;
    let token_symbol = require("TOKEN_SYMBOL")?;
    let decimals: u8 = require("DECIMALS")?
        .parse()
        .map_err(|_| eyre::eyre!("DECIMALS must be a number"))?;
    let target_json = PathBuf::from(require("TARGET_JSON")?);
    let mnemonic = require("MNEMONIC")?;
    let env = require("ENV")?;
    let salt = require("SALT")?;

    // Optional env vars
    let explorer_name = std::env::var("EXPLORER_NAME").ok();
    let explorer_url = std::env::var("EXPLORER_URL").ok();
    let admin_mnemonic = std::env::var("MULTISIG_PROVER_MNEMONIC").ok();
    let deployer_private_key = std::env::var("DEPLOYER_PRIVATE_KEY").ok();
    let gateway_deployer_private_key = std::env::var("GATEWAY_DEPLOYER_PRIVATE_KEY").ok();
    let gas_service_deployer_private_key = std::env::var("GAS_SERVICE_DEPLOYER_PRIVATE_KEY").ok();
    let its_deployer_private_key = std::env::var("ITS_DEPLOYER_PRIVATE_KEY").ok();
    let its_salt = std::env::var("ITS_SALT").ok();
    let its_proxy_salt = std::env::var("ITS_PROXY_SALT").ok();

    // --- Chain config → target json ---
    let mut chain_entry = json!({
        "name": chain_name,
        "axelarId": axelar_id,
        "chainId": chain_id,
        "rpc": rpc_url,
        "tokenSymbol": token_symbol,
        "confirmations": 1,
        "finality": "finalized",
        "decimals": decimals,
        "approxFinalityWaitTime": 1,
        "chainType": "evm",
        "contracts": {}
    });

    if let (Some(name), Some(url)) = (&explorer_name, &explorer_url) {
        chain_entry["explorer"] = json!({ "name": name, "url": url });
    }

    let content = fs::read_to_string(&target_json)?;
    let mut root: Value = serde_json::from_str(&content)?;
    let chains = root
        .get_mut("chains")
        .and_then(|c| c.as_object_mut())
        .ok_or_else(|| eyre::eyre!("no 'chains' object in {}", target_json.display()))?;

    if chains.contains_key(&axelar_id) {
        ui::info(&format!(
            "chain '{axelar_id}' already exists in {}, skipping",
            target_json.display()
        ));
    } else {
        chains.insert(axelar_id.clone(), chain_entry);
        fs::write(&target_json, serde_json::to_string_pretty(&root)? + "\n")?;
        ui::success(&format!(
            "added chain '{axelar_id}' to {}",
            target_json.display()
        ));
    }

    // --- State file ---
    let dir = data_dir()?;
    fs::create_dir_all(&dir)?;

    let env_parsed: Network = env
        .parse()
        .map_err(|e| eyre::eyre!("invalid ENV value '{env}': {e}"))?;

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

    ui::section("Deployer Addresses");

    let (_, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    ui::address("axelar deployer", &axelar_address);

    if let Some(admin_mn) = admin_mnemonic {
        let (_, admin_address) = derive_axelar_wallet(&admin_mn)?;
        ui::address("prover admin", &admin_address);
        state.admin_mnemonic = Some(admin_mn);
    }

    if let Some(pk) = deployer_private_key {
        let signer: PrivateKeySigner = pk
            .parse()
            .map_err(|e| eyre::eyre!("invalid deployer private key: {e}"))?;
        ui::address("deployer", &format!("{}", signer.address()));
        state.deployer_private_key = Some(pk);
    }
    if let Some(pk) = gateway_deployer_private_key {
        let signer: PrivateKeySigner = pk
            .parse()
            .map_err(|e| eyre::eyre!("invalid gateway deployer private key: {e}"))?;
        let gw_addr = signer.address();
        ui::address("gateway deployer", &format!("{gw_addr}"));
        state.gateway_deployer_private_key = Some(pk);
        state.gateway_deployer = Some(gw_addr);
    }
    if let Some(pk) = gas_service_deployer_private_key {
        let signer: PrivateKeySigner = pk
            .parse()
            .map_err(|e| eyre::eyre!("invalid gas service deployer private key: {e}"))?;
        ui::address("gas service deployer", &format!("{}", signer.address()));
        state.gas_service_deployer_private_key = Some(pk);
    }
    if let Some(pk) = its_deployer_private_key {
        let signer: PrivateKeySigner = pk
            .parse()
            .map_err(|e| eyre::eyre!("invalid ITS deployer private key: {e}"))?;
        ui::address("ITS deployer", &format!("{}", signer.address()));
        state.its_deployer_private_key = Some(pk);
    }
    if let Some(s) = its_salt {
        ui::kv("ITS salt", &s);
        state.its_salt = Some(s);
    }
    if let Some(s) = its_proxy_salt {
        ui::kv("ITS proxy salt", &s);
        state.its_proxy_salt = Some(s);
    }

    ui::section("State");
    let state_file = state_path(&axelar_id)?;
    save_state(&state)?;
    ui::kv("state file", &state_file.display().to_string());
    ui::success(&format!("init complete for '{axelar_id}' (env={env})"));

    // Query and display the deployer balance
    if target_json.exists() {
        let (lcd, _, fee_denom, _) = read_axelar_config(&target_json)?;
        let url = format!("{lcd}/cosmos/bank/v1beta1/balances/{axelar_address}");
        match reqwest::get(&url).await {
            Ok(resp) => {
                let data: Value = resp.json().await?;
                if let Some(balances) = data["balances"].as_array() {
                    let bal = balances
                        .iter()
                        .find(|b| b["denom"].as_str() == Some(&fee_denom))
                        .and_then(|b| b["amount"].as_str())
                        .unwrap_or("0");
                    let display_denom = fee_denom.strip_prefix('u').unwrap_or(&fee_denom);
                    let bal_major: f64 = bal.parse::<f64>().unwrap_or(0.0) / 1_000_000.0;
                    ui::kv("balance", &format!("{bal_major:.6} {display_denom}"));
                }
            }
            Err(e) => ui::warn(&format!("could not query balance: {e}")),
        }
    }

    Ok(())
}
