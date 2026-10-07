use base64::Engine;
use eyre::Result;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::types::{GatewayConfig, ProverConfig, RawResponse, VerifierConfig};
use super::verifiers::contract;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{lcd_fetch_code_id, read_axelar_config, read_axelar_contract_field};

pub async fn raw<T: DeserializeOwned>(lcd: &str, address: &str, key: &str) -> Result<T> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    let response: RawResponse = crate::http::client()
        .get(format!(
            "{}/cosmwasm/wasm/v1/contract/{address}/raw/{encoded}",
            lcd.trim_end_matches('/')
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD.decode(response.data)?,
    )?)
}

pub async fn instantiated(ctx: &DeployContext) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(&ctx.target_json).await?;
    let coordinator = contract(&ctx.state, "Coordinator", false).await?;
    for name in ["Gateway", "VotingVerifier", "MultisigProver"] {
        let address = contract(&ctx.state, name, true).await?;
        let expected_hash = super::inputs::cosmos_code_hash(&ctx.target_json, name).await?;
        let expected_code = lcd_fetch_code_id(&lcd, &expected_hash).await?;
        let info: super::types::ContractInfoResponse = crate::http::client()
            .get(format!("{lcd}/cosmwasm/wasm/v1/contract/{address}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let admin = read_axelar_contract_field(
            &ctx.target_json,
            &format!("/axelar/contracts/{name}/{}/contractAdmin", ctx.axelar_id),
        )
        .await?;
        eyre::ensure!(
            info.contract_info.code_id == expected_code.to_string()
                && info.contract_info.creator == coordinator
                && info.contract_info.admin == admin,
            "{name}: code, creator or contract admin mismatch"
        );
    }
    verifier(ctx, &lcd).await?;
    prover(ctx, &lcd).await?;
    let address = contract(&ctx.state, "Gateway", true).await?;
    let gateway: GatewayConfig = raw(&lcd, &address, "config").await?;
    eyre::ensure!(
        gateway.router == contract(&ctx.state, "Router", false).await?
            && gateway.verifier == contract(&ctx.state, "VotingVerifier", true).await?,
        "Cosmos gateway wiring mismatch"
    );
    Ok(())
}

async fn verifier(ctx: &DeployContext, lcd: &str) -> Result<()> {
    let plan = super::session::current()?.plan.clone();
    let address = contract(&ctx.state, "VotingVerifier", true).await?;
    let config: VerifierConfig = raw(lcd, &address, "config").await?;
    let gateway = ctx
        .state
        .predicted_gateway_address
        .ok_or_else(|| eyre::eyre!("missing deployed gateway"))?;
    eyre::ensure!(
        config.source_chain == ctx.axelar_id
            && config
                .source_gateway_address
                .parse::<alloy::primitives::Address>()?
                == gateway
            && config.voting_threshold == plan.voting_threshold.map(|n| n.to_string())
            && config.block_expiry == plan.block_expiry.to_string()
            && config.confirmation_height == plan.confirmation_height
            && config.service_name == ctx.state.env.verifier_service_name()
            && config.msg_id_format == "hex_tx_hash_and_event_index",
        "VotingVerifier settings differ from the approved plan"
    );
    eyre::ensure!(
        config.service_registry_contract == contract(&ctx.state, "ServiceRegistry", false).await?
            && config.rewards_contract == contract(&ctx.state, "Rewards", false).await?
            && config.chain_codec_address == contract(&ctx.state, "ChainCodecEvm", false).await?,
        "VotingVerifier protocol wiring mismatch"
    );
    Ok(())
}

async fn prover(ctx: &DeployContext, lcd: &str) -> Result<()> {
    let plan = super::session::current()?.plan.clone();
    let address = contract(&ctx.state, "MultisigProver", true).await?;
    let config: ProverConfig = raw(lcd, &address, "config").await?;
    let domain = crate::utils::compute_domain_separator(&ctx.target_json, &ctx.axelar_id).await?;
    eyre::ensure!(
        config.chain_name == ctx.axelar_id
            && config.signing_threshold == plan.signing_threshold.map(|n| n.to_string())
            && config.service_name == ctx.state.env.verifier_service_name()
            && config.key_type == "ecdsa"
            && config.domain_separator == domain.0
            && config.verifier_set_diff_threshold == 0
            && !config.notify_signing_session
            && !config.expect_full_message_payloads,
        "MultisigProver settings differ from the approved plan"
    );
    eyre::ensure!(
        config.gateway == contract(&ctx.state, "Gateway", true).await?
            && config.voting_verifier == contract(&ctx.state, "VotingVerifier", true).await?
            && config.multisig == contract(&ctx.state, "Multisig", false).await?
            && config.coordinator == contract(&ctx.state, "Coordinator", false).await?
            && config.service_registry == contract(&ctx.state, "ServiceRegistry", false).await?
            && config.chain_codec == contract(&ctx.state, "ChainCodecEvm", false).await?,
        "MultisigProver protocol wiring mismatch"
    );
    eyre::ensure!(
        super::preflight::raw_admin(lcd, &address).await? == plan.prover_admin,
        "prover admin mismatch"
    );
    Ok(())
}

pub async fn protocol(state: &crate::state::State, lcd: &str, denom: &str) -> Result<Value> {
    let coordinator = contract(state, "Coordinator", false).await?;
    let wiring: super::types::ProtocolContracts = raw(lcd, &coordinator, "protocol").await?;
    eyre::ensure!(
        wiring.router == contract(state, "Router", false).await?
            && wiring.multisig == contract(state, "Multisig", false).await?
            && wiring.service_registry == contract(state, "ServiceRegistry", false).await?,
        "Coordinator protocol wiring mismatch"
    );
    for name in ["Router", "Multisig"] {
        let address = contract(state, name, false).await?;
        let config: super::types::ProxyConfig = raw(lcd, &address, "config").await?;
        eyre::ensure!(
            config.coordinator == coordinator,
            "{name}: Coordinator is not the authorized proxy"
        );
    }
    let rewards = contract(state, "Rewards", false).await?;
    let rewards_config: super::types::RewardsConfig = raw(lcd, &rewards, "config").await?;
    eyre::ensure!(
        rewards_config.rewards_denom == denom,
        "Rewards denomination differs from funding denomination"
    );
    let expected =
        read_axelar_contract_field(&state.target_json, "/axelar/governanceAddress").await?;
    for name in [
        "Coordinator",
        "Router",
        "Multisig",
        "Rewards",
        "InterchainTokenService",
    ] {
        let address = contract(state, name, false).await?;
        let governor: String = raw(lcd, &address, "permission_control_governance_addr").await?;
        eyre::ensure!(
            governor == expected,
            "{name}: unexpected governance authority"
        );
    }
    if !state.env.deployment_uses_governance() {
        let (_, signer) = crate::cosmos::derive_axelar_wallet(&state.mnemonic)?;
        super::direct::validate_authority(&signer, &expected)?;
    }
    Ok(json!({"wiring":wiring,"rewards":rewards_config}))
}

pub async fn available_before_start(state: &crate::state::State, lcd: &str) -> Result<()> {
    if state.hardened_fingerprint.is_some() {
        return Ok(());
    }
    let router = contract(state, "Router", false).await?;
    let mut start_after: Option<String> = None;
    loop {
        let response = crate::cosmos::lcd_cosmwasm_smart_query(
            lcd,
            &router,
            &json!({"chains":{"start_after":start_after,"limit":100}}),
        )
        .await?;
        let chains: Vec<super::types::RegisteredChain> = serde_json::from_value(response)?;
        eyre::ensure!(
            !chains
                .iter()
                .any(|chain| chain.name == state.axelar_id.as_str()),
            "chain is already registered in Router; a new hardened deployment cannot adopt it"
        );
        if chains.len() < 100 {
            break;
        }
        let last = chains
            .last()
            .ok_or_else(|| eyre::eyre!("missing chain pagination cursor"))?
            .name
            .clone();
        eyre::ensure!(
            start_after.as_ref().is_none_or(|previous| previous < &last),
            "Router pagination did not advance"
        );
        start_after = Some(last);
    }
    let hub = contract(state, "InterchainTokenService", false).await?;
    let chain = crate::cosmos::lcd_cosmwasm_smart_query(
        lcd,
        &hub,
        &json!({"its_chain":{"chain":state.axelar_id}}),
    )
    .await?;
    eyre::ensure!(
        chain.is_null(),
        "chain is already registered on the ITS Hub; explicit migration is required"
    );
    Ok(())
}
