//! `InstantiateChainContracts` step. Asks Coordinator to instantiate the
//! per-chain Gateway / VotingVerifier / MultisigProver trio via governance.

use std::path::Path;

use base64::Engine;
use eyre::Result;
use serde_json::{Value, json};

use super::StepTxContext;
use super::defaults::DEFAULT_VV_BLOCK_EXPIRY;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, lcd_fetch_code_id, read_axelar_config, read_axelar_contract_field,
};
use crate::evm::get_salt_from_key;
use crate::state::{State, StepStatus};
use crate::ui;
use crate::utils::compute_domain_separator;

mod permissions;
mod reconcile;
mod types;

use types::{ChainCodeIds, ChainContractAddresses, InstantiatePlan};

async fn read_chain_contract_addresses(ctx: &DeployContext) -> Result<ChainContractAddresses> {
    read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Router/address").await?;
    Ok(ChainContractAddresses {
        coordinator: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/contracts/Coordinator/address",
        )
        .await?,
        rewards: read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address")
            .await?,
        multisig: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/contracts/Multisig/address",
        )
        .await?,
        codec: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/contracts/ChainCodecEvm/address",
        )
        .await?,
        governance: read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress")
            .await?,
    })
}

async fn fetch_chain_code_ids(target_json: &Path, lcd: &str) -> Result<ChainCodeIds> {
    ui::info("fetching code IDs...");
    let gateway_hash = read_code_hash(target_json, "Gateway").await?;
    let verifier_hash = read_code_hash(target_json, "VotingVerifier").await?;
    let prover_hash = read_code_hash(target_json, "MultisigProver").await?;

    let gateway = lcd_fetch_code_id(lcd, &gateway_hash).await?;
    let verifier = lcd_fetch_code_id(lcd, &verifier_hash).await?;
    let prover = lcd_fetch_code_id(lcd, &prover_hash).await?;
    ui::kv(
        "code IDs",
        &format!("gateway={gateway}, verifier={verifier}, prover={prover}"),
    );
    Ok(ChainCodeIds {
        gateway,
        verifier,
        prover,
    })
}

async fn read_code_hash(target_json: &Path, contract: &str) -> Result<String> {
    crate::commands::deploy::hardened::inputs::cosmos_code_hash(target_json, contract).await
}

pub async fn check_instantiate_permissions(state: &State) -> Result<()> {
    if !state.steps.iter().any(|step| {
        step.status == StepStatus::Pending
            && matches!(
                step.name.as_str(),
                "InstantiateChainContracts" | "WaitInstantiateProposal"
            )
    }) {
        return Ok(());
    }
    let (lcd, _, _, _) = read_axelar_config(&state.target_json).await?;
    let coordinator =
        read_axelar_contract_field(&state.target_json, "/axelar/contracts/Coordinator/address")
            .await?;
    let codes = fetch_chain_code_ids(&state.target_json, &lcd).await?;
    let chain = state.axelar_id.as_str();
    let name = format!(
        "{chain}-{}-{}-{}",
        codes.gateway, codes.verifier, codes.prover
    );
    if let Some(deployment) = reconcile::find_deployment(&lcd, &coordinator, &name).await? {
        eyre::ensure!(
            deployment.chain_name == chain,
            "deployment name is already used by another chain"
        );

        let path =
            crate::commands::deploy::hardened::storage::directory(state)?.join("journal.json");
        eyre::ensure!(
            path.exists(),
            "existing Coordinator deployment has no journal; adoption is unsupported"
        );
        let journal: crate::commands::deploy::hardened::types::Journal =
            serde_json::from_slice(&tokio::fs::read(path).await?)?;
        eyre::ensure!(
            journal
                .actions
                .contains_key("InstantiateChainContracts/cosmos"),
            "existing Coordinator deployment has no recorded hardened submission"
        );

        return Ok(());
    }
    permissions::check(&lcd, &coordinator, &codes).await
}

fn contract_admin(env: &str) -> &'static str {
    match env {
        "devnet-amplifier" => "axelar1zlr7e5qf3sz7yf890rkh9tcnu87234k6k7ytd9",
        "testnet" => "axelar1wxej3l9aczsns3harrtdzk7rct29jl47tvu8mp",
        "mainnet" => "axelar1nctnr9x0qexemeld5w7w752rmqdsqqv92dw9am",
        _ => "axelar12qvsvse32cjyw60ztysd3v655aj5urqeup82ky",
    }
}

async fn build_instantiate_plan(
    ctx: &DeployContext,
    tx: &StepTxContext<'_>,
    addresses: &ChainContractAddresses,
    codes: ChainCodeIds,
) -> Result<InstantiatePlan> {
    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let root: Value = serde_json::from_str(&content)?;
    let verifier = root
        .pointer(&format!(
            "/axelar/contracts/VotingVerifier/{}",
            tx.chain_axelar_id
        ))
        .ok_or_else(|| eyre::eyre!("no VotingVerifier.{} config", tx.chain_axelar_id))?;
    let prover = root
        .pointer(&format!(
            "/axelar/contracts/MultisigProver/{}",
            tx.chain_axelar_id
        ))
        .ok_or_else(|| eyre::eyre!("no MultisigProver.{} config", tx.chain_axelar_id))?;
    let salt_key = reconcile::chain_salt_key(tx.chain_axelar_id, &ctx.state.cosm_salt);
    let salt =
        base64::engine::general_purpose::STANDARD.encode(get_salt_from_key(&salt_key).as_slice());
    let domain_separator = alloy::hex::encode(
        compute_domain_separator(&ctx.target_json, &ctx.axelar_id)
            .await?
            .as_slice(),
    );
    let admin = contract_admin(tx.env);
    let deployment_name = format!(
        "{}-{}-{}-{}",
        tx.chain_axelar_id, codes.gateway, codes.verifier, codes.prover
    );
    let admin_address = &ctx
        .state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing deployment plan"))?
        .prover_admin;
    let execute_msg = json!({
        "instantiate_chain_contracts": {
            "deployment_name": deployment_name,
            "salt": salt,
            "params": { "manual": {
                "gateway": {
                    "code_id": codes.gateway,
                    "label": format!("Gateway-{}", tx.chain_axelar_id),
                    "msg": null,
                    "contract_admin": admin
                },
                "verifier": {
                    "code_id": codes.verifier,
                    "label": format!("VotingVerifier-{}", tx.chain_axelar_id),
                    "msg": {
                        "governance_address": verifier["governanceAddress"],
                        "service_name": verifier["serviceName"],
                        "source_gateway_address": verifier["sourceGatewayAddress"],
                        "voting_threshold": verifier["votingThreshold"],
                        "block_expiry": verifier["blockExpiry"].as_u64().unwrap_or(DEFAULT_VV_BLOCK_EXPIRY).to_string(),
                        "confirmation_height": verifier["confirmationHeight"],
                        "source_chain": tx.chain_axelar_id,
                        "rewards_address": addresses.rewards,
                        "msg_id_format": verifier["msgIdFormat"],
                        "chain_codec_address": addresses.codec,
                        "address_format": verifier["addressFormat"]
                    },
                    "contract_admin": admin
                },
                "prover": {
                    "code_id": codes.prover,
                    "label": format!("MultisigProver-{}", tx.chain_axelar_id),
                    "msg": {
                        "governance_address": prover["governanceAddress"],
                        "admin_address": admin_address,
                        "multisig_address": addresses.multisig,
                        "signing_threshold": prover["signingThreshold"],
                        "service_name": prover["serviceName"],
                        "chain_name": tx.chain_axelar_id,
                        "verifier_set_diff_threshold": prover["verifierSetDiffThreshold"],
                        "key_type": prover["keyType"],
                        "domain_separator": domain_separator,
                        "notify_signing_session": false,
                        "expect_full_message_payloads": false,
                        "sig_verifier_address": null,
                        "chain_codec_address": addresses.codec
                    },
                    "contract_admin": admin
                }
            }}
        }
    });
    Ok(InstantiatePlan {
        execute_msg,
        deployment_name,
        salt_key,
        domain_separator,
        contract_admin: admin,
        codes,
    })
}

async fn save_instantiate_plan(
    ctx: &DeployContext,
    chain_axelar_id: &str,
    plan: &InstantiatePlan,
) -> Result<()> {
    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;
    let coordinator = root
        .pointer_mut("/axelar/contracts/Coordinator")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| eyre::eyre!("no Coordinator config"))?;
    if coordinator.get("deployments").is_none() {
        coordinator.insert("deployments".to_string(), json!({}));
    }
    coordinator["deployments"]
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("Coordinator.deployments is not an object"))?
        .insert(
            chain_axelar_id.to_string(),
            json!({
                "deploymentName": plan.deployment_name,
                "salt": plan.salt_key
            }),
        );
    if let Some(verifier) = root.pointer_mut(&format!(
        "/axelar/contracts/VotingVerifier/{chain_axelar_id}"
    )) {
        verifier["codeId"] = json!(plan.codes.verifier);
        verifier["contractAdmin"] = json!(plan.contract_admin);
    }
    if let Some(prover) = root.pointer_mut(&format!(
        "/axelar/contracts/MultisigProver/{chain_axelar_id}"
    )) {
        prover["codeId"] = json!(plan.codes.prover);
        prover["domainSeparator"] = json!(format!("0x{}", plan.domain_separator));
        prover["contractAdmin"] = json!(plan.contract_admin);
    }
    if let Some(gateway) = root.pointer_mut(&format!("/axelar/contracts/Gateway/{chain_axelar_id}"))
    {
        gateway["codeId"] = json!(plan.codes.gateway);
        gateway["contractAdmin"] = json!(plan.contract_admin);
    } else if let Some(gateways) = root
        .pointer_mut("/axelar/contracts/Gateway")
        .and_then(Value::as_object_mut)
    {
        gateways.insert(
            chain_axelar_id.to_string(),
            json!({
                "codeId": plan.codes.gateway,
                "contractAdmin": plan.contract_admin
            }),
        );
    }
    crate::commands::deploy::hardened::storage::atomic_config_write(
        &ctx.target_json,
        (serde_json::to_string_pretty(&root)? + "\n").as_bytes(),
    )?;
    Ok(())
}

pub(super) async fn run_instantiate(ctx: &mut DeployContext, tx: StepTxContext<'_>) -> Result<()> {
    ui::info(&format!(
        "instantiating chain contracts for {}...",
        tx.chain_axelar_id
    ));
    let addresses = read_chain_contract_addresses(ctx).await?;
    let codes = fetch_chain_code_ids(&ctx.target_json, tx.lcd).await?;
    let plan = build_instantiate_plan(ctx, &tx, &addresses, codes).await?;

    eyre::ensure!(
        reconcile::find_deployment(tx.lcd, &addresses.coordinator, &plan.deployment_name)
            .await?
            .is_none(),
        "an existing Coordinator deployment has no recorded submission; adoption is unsupported"
    );

    permissions::check(tx.lcd, &addresses.coordinator, &plan.codes).await?;
    reconcile::check_addresses_available(tx.lcd, &addresses.coordinator, &plan).await?;
    save_instantiate_plan(ctx, tx.chain_axelar_id, &plan).await?;
    submit_instantiate(ctx, tx, &addresses, &plan).await
}

async fn submit_instantiate(
    ctx: &mut DeployContext,
    tx: StepTxContext<'_>,
    addresses: &ChainContractAddresses,
    plan: &InstantiatePlan,
) -> Result<()> {
    let json_str = serde_json::to_string_pretty(&plan.execute_msg)?;
    ui::info(&format!(
        "execute msg: {}",
        ui::truncated_json(&json_str, 3)
    ));
    let sender = if ctx.state.env.deployment_uses_governance() {
        addresses.governance.as_str()
    } else {
        tx.axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &addresses.coordinator, &plan.execute_msg)?;
    let title = format!("Instantiate chain contracts for {}", tx.chain_axelar_id);
    super::submission::submit(ctx, tx, vec![inner_msg], &title).await
}
