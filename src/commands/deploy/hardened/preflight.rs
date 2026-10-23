use std::collections::BTreeMap;

use alloy::primitives::{B256, U256, keccak256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use base64::Engine;
use eyre::Result;
use serde_json::{Value, json};

use super::plan;
use super::types::{Preflight, ProtocolIdentity};
use crate::cosmos::read_axelar_config;
use crate::state::{State, StepStatus};

mod funding;

pub async fn validate(
    state: &State,
    confirmation_policy: super::types::EvmConfirmationPolicy,
) -> Result<Preflight> {
    let plan = state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing hardened plan"))?;
    plan::validate(plan)?;
    plan::validate_steps(state)?;
    let root: Value = serde_json::from_slice(&tokio::fs::read(&state.target_json).await?)?;
    let chain = &root["chains"][state.axelar_id.as_str()];
    validate_chain_configuration(state, chain, plan)?;
    let provider = ProviderBuilder::new().connect_http(state.rpc_url.parse()?);
    eyre::ensure!(
        provider.get_chain_id().await? == plan.evm_chain_id,
        "RPC is on the wrong EVM network"
    );
    super::evm_policy::validate(state)?;
    eyre::ensure!(
        provider
            .get_block_by_number(confirmation_policy.block_tag())
            .await?
            .is_some(),
        "RPC does not support the requested confirmation policy ({confirmation_policy})"
    );
    let (lcd, chain_id, denom, gas_price) = read_axelar_config(&state.target_json).await?;
    eyre::ensure!(
        gas_price.is_finite() && gas_price > 0.0,
        "invalid Cosmos gas price"
    );
    eyre::ensure!(
        chain_id == plan.axelar_chain_id,
        "Axelar configuration and plan chain IDs differ"
    );
    validate_lcd_network(&lcd, &chain_id).await?;
    let mut identities = BTreeMap::new();
    for (name, key) in evm_keys(state) {
        let signer: PrivateKeySigner = key.ok_or_else(|| eyre::eyre!("missing {name}"))?.parse()?;
        identities.insert(name, signer.address().to_string());
        let remaining = remaining_budget(state, &signer.address().to_string(), false).await?;
        eyre::ensure!(
            provider.get_balance(signer.address()).await? >= U256::from(remaining),
            "{name} lacks its remaining approved gas budget"
        );
    }
    identities.extend(funding::check(state, &lcd, &chain_id, &denom).await?);
    check_verifier_count(state, &root, &lcd).await?;
    validate_lcd_override(&lcd)?;
    super::contract_checks::available_before_start(state, &lcd).await?;
    let wiring = super::contract_checks::protocol(state, &lcd, &denom).await?;
    let protocols = protocol_inputs(&root, &lcd).await?;
    let artifacts = artifacts(state).await?;
    let mut cosmos_codes = Vec::new();
    for name in ["Gateway", "VotingVerifier", "MultisigProver"] {
        cosmos_codes.push(super::inputs::cosmos_code_hash(&state.target_json, name).await?);
    }
    let addresses: BTreeMap<_, _> = protocols
        .iter()
        .map(|(name, identity)| (name, &identity.address))
        .collect();
    let mut fingerprint = json!({"plan":plan,"network":state.env,"chain":state.axelar_id,"identities":identities,
        "salt":state.cosm_salt,"itsSalt":state.its_salt,"itsProxySalt":state.its_proxy_salt,
        "chainName":chain["name"],"tokenSymbol":chain["tokenSymbol"],"decimals":chain["decimals"],"operators":state.env.axelar_operators(),
        "protocolAddresses":addresses,"wiring":wiring,"artifacts":artifacts,"cosmosCodes":cosmos_codes,"governance":root["axelar"]["governanceAddress"]});
    if state.env == crate::types::Network::Mainnet {
        fingerprint["mainnetPolicy"] = json!({"evmAddresses":"configured-signers-v1", "upgradeAdmin":crate::steps::cosmos_tx::contract_admin("mainnet")});
    }
    fingerprint["proposalFlow"] = json!("instantiate-register-v1");
    super::inputs::check_fingerprint(&fingerprint)?;
    Ok(Preflight {
        fingerprint: keccak256(serde_json::to_vec(&fingerprint)?),
        fingerprint_inputs: fingerprint,
        protocols,
    })
}

pub async fn raw_admin(lcd: &str, contract: &str) -> Result<String> {
    let key =
        base64::engine::general_purpose::STANDARD.encode("permission_control_contract_admin_addr");
    let response: Value = crate::http::client()
        .get(format!(
            "{lcd}/cosmwasm/wasm/v1/contract/{contract}/raw/{key}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let data = response["data"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("missing admin data"))?;
    Ok(serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD.decode(data)?,
    )?)
}

fn evm_keys(state: &State) -> [(&'static str, Option<&str>); 4] {
    [
        ("deployer", state.deployer_private_key.as_deref()),
        ("gateway", state.gateway_deployer_private_key.as_deref()),
        (
            "gasService",
            state.gas_service_deployer_private_key.as_deref(),
        ),
        ("its", state.its_deployer_private_key.as_deref()),
    ]
}

fn required<'a>(root: &'a Value, pointer: &str) -> Result<&'a str> {
    root.pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| eyre::eyre!("missing {pointer}"))
}

async fn protocol_inputs(root: &Value, lcd: &str) -> Result<BTreeMap<String, ProtocolIdentity>> {
    let mut result = BTreeMap::new();
    for name in [
        "Coordinator",
        "Router",
        "Rewards",
        "Multisig",
        "ServiceRegistry",
        "ChainCodecEvm",
        "ItsAbiTranslator",
        "InterchainTokenService",
    ] {
        let address = required(root, &format!("/axelar/contracts/{name}/address"))?;
        let info: Value = crate::http::client()
            .get(format!("{lcd}/cosmwasm/wasm/v1/contract/{address}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let contract: super::types::ContractInfo =
            serde_json::from_value(info["contract_info"].clone())?;
        let code: Value = crate::http::client()
            .get(format!("{lcd}/cosmwasm/wasm/v1/code/{}", contract.code_id))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let checksum = required(&code, "/code_info/data_hash")?.to_owned();
        result.insert(
            name.into(),
            ProtocolIdentity {
                address: address.into(),
                code_id: contract.code_id,
                checksum,
                creator: contract.creator,
                admin: contract.admin,
            },
        );
    }
    Ok(result)
}

async fn artifacts(state: &State) -> Result<BTreeMap<String, B256>> {
    let root = super::inputs::root(state)?;
    let paths = super::inputs::artifact_paths(&root)?;
    let mut hashes = BTreeMap::new();
    for path in paths {
        let path = path.to_string_lossy().into_owned();
        let bytes = tokio::fs::read(&path).await?;
        let artifact: Value = serde_json::from_slice(&bytes)?;
        validate_artifact_capabilities(&path, &artifact)?;
        let creation = crate::evm::read_artifact_bytecode(&path).await?;
        let runtime = crate::evm::artifact::read_artifact_runtime_hash(&path).await?;
        let mut code = creation;
        code.extend_from_slice(runtime.as_slice());
        let name = std::path::Path::new(&path)
            .strip_prefix(&root)?
            .to_string_lossy()
            .into_owned();
        hashes.insert(name, keccak256(code));
    }
    Ok(hashes)
}

pub(super) fn proposal_deposit_status(
    state: &State,
    recorded: &impl Fn(&str) -> bool,
) -> (bool, bool) {
    let mut remaining = false;
    let mut escrowed = false;
    for submit in &state.steps {
        let crate::state::StepKind::CosmosTx { proposal_key } = &submit.kind else {
            continue;
        };
        let Some(wait) = state.steps.iter().find(|step| {
            matches!(&step.kind, crate::state::StepKind::CosmosPoll { proposal_key: key } if key == proposal_key)
        }) else {
            continue;
        };
        let submitted = recorded(&submit.name) || state.proposals.contains_key(proposal_key);
        remaining |= !submitted;
        escrowed |= submitted && wait.status == StepStatus::Pending;
    }
    (remaining, escrowed)
}

async fn remaining_budget(state: &State, sender: &str, cosmos: bool) -> Result<u128> {
    let plan = state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing plan"))?;
    let budget: u128 = if cosmos {
        &plan.cosmos_fee_budget
    } else {
        &plan.evm_gas_budget
    }
    .parse()?;
    let path = super::storage::directory(state)?.join("journal.json");
    if !path.exists() {
        return Ok(budget);
    }
    let journal: super::types::Journal = serde_json::from_slice(&tokio::fs::read(path).await?)?;
    let reserved = if cosmos {
        super::cosmos_funding::fee_liability(&journal, sender, None)?
    } else {
        super::evm_funding::liability(&journal, sender.parse()?, None)?.try_into()?
    };
    budget
        .checked_sub(reserved)
        .ok_or_else(|| eyre::eyre!("journal exceeds approved fee budget"))
}

async fn check_verifier_count(state: &State, root: &Value, lcd: &str) -> Result<()> {
    let path = super::storage::directory(state)?.join("journal.json");
    if path.exists() {
        let journal: super::types::Journal = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        if journal.initial_signers.is_some() {
            return Ok(());
        }
    }
    let registry = required(root, "/axelar/contracts/ServiceRegistry/address")?;
    let service = super::verifiers::service(state, lcd, registry).await?;
    super::verifier_set::validate_limits(&service)?;
    crate::ui::kv(
        "minimum initial verifiers",
        &service.min_num_verifiers.to_string(),
    );
    crate::ui::info("Verifier identities will be discovered and approved at activation.");
    Ok(())
}

fn validate_chain_configuration(
    state: &State,
    chain: &Value,
    plan: &super::types::Plan,
) -> Result<()> {
    if state.hardened_fingerprint.is_none() {
        eyre::ensure!(
            chain["contracts"]
                .as_object()
                .is_none_or(|contracts| contracts.is_empty()),
            "new hardened deployments require an empty EVM contract configuration; adopting an existing deployment requires explicit migration"
        );
    }
    eyre::ensure!(
        chain["chainId"].as_u64() == Some(plan.evm_chain_id),
        "plan and configuration EVM chain IDs differ"
    );
    eyre::ensure!(
        chain["axelarId"].as_str() == Some(state.axelar_id.as_str()),
        "chain key and axelarId must match"
    );
    Ok(())
}

fn validate_artifact_capabilities(path: &str, artifact: &Value) -> Result<()> {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let functions: &[&str] = match name {
        "AxelarAmplifierGateway.json" => &[
            "epoch",
            "signersHashByEpoch",
            "owner",
            "operator",
            "transferOwnership",
        ],
        "InterchainTokenService.json" => &["owner", "implementation", "transferOwnership"],
        "InterchainTokenFactory.json" | "AxelarGasService.json" => {
            &["owner", "implementation", "transferOwnership"]
        }
        "Operators.json" => &["owner", "isOperator", "addOperator", "transferOwnership"],
        _ => &[],
    };
    for function in functions {
        eyre::ensure!(
            artifact["abi"].as_array().is_some_and(|abi| abi
                .iter()
                .any(|entry| entry["type"] == "function" && entry["name"] == *function)),
            "{name} artifact lacks required {function} capability"
        );
    }
    Ok(())
}

fn validate_lcd_override(lcd: &str) -> Result<()> {
    if let Ok(overridden) = std::env::var("AXELAR_LCD_URL") {
        eyre::ensure!(
            overridden.trim_end_matches('/') == lcd.trim_end_matches('/'),
            "AXELAR_LCD_URL differs from the validated deployment LCD; update the deployment configuration or remove the override"
        );
    }
    Ok(())
}

pub(super) fn sequential_deposit(outstanding: bool, escrowed: bool, deposit: u128) -> u128 {
    if outstanding && !escrowed { deposit } else { 0 }
}

async fn validate_lcd_network(lcd: &str, chain_id: &str) -> Result<()> {
    let node: Value = crate::http::client()
        .get(format!("{lcd}/cosmos/base/tendermint/v1beta1/node_info"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    eyre::ensure!(
        node.pointer("/default_node_info/network")
            .and_then(Value::as_str)
            == Some(chain_id),
        "LCD is on the wrong Axelar network"
    );
    Ok(())
}
