use alloy::primitives::{Address, address};
use eyre::{Result, WrapErr, ensure};
use serde_json::{Value, json};

use super::types::{Context, Options, Settings};
use crate::commands::deploy::hardened::storage;
use crate::config::{ChainConfig, ChainsConfig};
use crate::{config_source, types::Network, ui, utils};

pub const GOVERNANCE_ADDRESS: &str = "axelar10d07y265gmmuvt4z0w9aw880jnsr700j7v9daj";
pub const GOVERNANCE_CHAIN: &str = "axelar";
pub const CREATE3_FACTORY: Address = address!("6513Aedb4D1593BA12e50644401D976aebDc90d8");
const SDK: &str = "node_modules/@axelar-network/axelar-gmp-sdk-solidity/artifacts/contracts";

pub fn settings(network: Network) -> Result<Settings> {
    match network {
        Network::Mainnet => Ok(Settings {
            deployer: address!("ba76c6980428A0b10CFC5d8ccb61949677A61233"),
            salt: "v6.0.4-axelar",
            asg: address!("7Acbae6CBa67d78AAf69e47000884aE00F9B2525"),
            operator: address!("c8a8399c3D1207f4e109673be7047604737c1D56"),
            delay: 259_200,
            hub_chain_id: "axelar-dojo-1",
        }),
        Network::Testnet => Ok(Settings {
            deployer: address!("81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC"),
            salt: "v6.1.0",
            asg: address!("b55A09E9fde39755c39EF9A7911431Ba0c96332A"),
            operator: address!("6e079CD1c6bBb72680DeDF2687d711AE9427eE8e"),
            delay: 300,
            hub_chain_id: "axelar-testnet-lisbon-3",
        }),
        _ => Err(eyre::eyre!(
            "ASG deployment/handoff supports testnet and mainnet only"
        )),
    }
}

pub async fn load(network: Network, chain: &str, options: &Options) -> Result<Context> {
    let settings = settings(network)?;
    let path = config_source::resolve(network, options.config.clone())
        .await?
        .require_checkout()?;
    let original = std::fs::read(&path)?;
    let config: ChainsConfig = serde_json::from_slice(&original)?;
    ensure!(
        config.axelar.chain_id.as_deref() == Some(settings.hub_chain_id),
        "Axelar network mismatch in config"
    );
    let (key, cfg) = config
        .chains
        .iter()
        .find(|(key, cfg)| {
            key.eq_ignore_ascii_case(chain) || cfg.axelar_id_or(key).eq_ignore_ascii_case(chain)
        })
        .ok_or_else(|| eyre::eyre!("chain '{chain}' not found"))?;
    ensure!(
        cfg.chain_type.as_deref() == Some("evm"),
        "ASG handoff requires an EVM chain"
    );
    let gateway = cfg.contract_by_name("AxelarGateway", key)?;
    ensure!(
        gateway.extra.get("connectionType").and_then(Value::as_str) == Some("amplifier"),
        "this command supports amplifier gateways only, not legacy governance/mint-limiter roles"
    );
    let artifact_path = match &options.artifact {
        Some(path) => path.clone(),
        None => utils::deployments_root(&path)?
            .join(SDK)
            .join("governance/AxelarServiceGovernance.sol/AxelarServiceGovernance.json"),
    };
    Ok(Context {
        network,
        gateway: required_address(cfg, key, "AxelarGateway")?,
        its: required_address(cfg, key, "InterchainTokenService")?,
        configured_asg: optional_address(cfg, "AxelarServiceGovernance")?,
        chain_id: cfg
            .evm_chain_id
            .ok_or_else(|| eyre::eyre!("missing EVM chainId"))?,
        rpc: cfg
            .rpc
            .clone()
            .ok_or_else(|| eyre::eyre!("missing EVM rpc"))?,
        key: key.clone().into(),
        path,
        original,
        settings,
        artifact_path,
    })
}

fn required_address(cfg: &ChainConfig, key: &str, name: &str) -> Result<Address> {
    let value: Address = cfg.contract_address_by_name(name, key)?.parse()?;
    ensure!(!value.is_zero(), "{name} address is zero");
    Ok(value)
}

fn optional_address(cfg: &ChainConfig, name: &str) -> Result<Option<Address>> {
    let value = cfg
        .contracts
        .as_ref()
        .and_then(|c| c.get(name))
        .and_then(|c| c.address.as_deref());
    value
        .map(|s| {
            s.parse()
                .wrap_err_with(|| format!("invalid {name} address"))
        })
        .transpose()
}

pub fn unchanged(ctx: &Context) -> Result<()> {
    ensure!(
        std::fs::read(&ctx.path)? == ctx.original,
        "chains config changed during review, rerun to rebuild the plan"
    );
    Ok(())
}

pub fn save(ctx: &mut Context, updates: &[(&str, Value)]) -> Result<()> {
    unchanged(ctx)?;
    let mut root: Value = serde_json::from_slice(&ctx.original)?;
    let contracts = root["chains"][ctx.key.as_str()]["contracts"]
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("missing contracts object"))?;
    for (name, patch) in updates {
        let entry = contracts
            .entry((*name).to_string())
            .or_insert_with(|| json!({}));
        let entry = entry
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("invalid {name} record"))?;
        let fields = patch
            .as_object()
            .ok_or_else(|| eyre::eyre!("invalid config patch"))?;
        entry.extend(fields.clone());
    }
    let bytes = (serde_json::to_string_pretty(&root)? + "\n").into_bytes();
    storage::atomic_config_write(&ctx.path, &bytes)?;
    ctx.original = bytes;
    ui::success(&format!(
        "verified state recorded in {}",
        ctx.path.display()
    ));
    Ok(())
}
