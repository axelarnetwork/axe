use eyre::Result;
use serde_json::{Value, json};

use super::verifiers::contract;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{lcd_cosmwasm_smart_query, read_axelar_config};
use crate::state::StepStatus;
use crate::utils::read_contract_address_by_name;

pub async fn check(ctx: &DeployContext, completed: Option<&str>) -> Result<()> {
    let done = |name: &str| {
        completed == Some(name)
            || ctx
                .state
                .steps
                .iter()
                .any(|s| s.name == name && s.status == StepStatus::Completed)
    };
    if done("SaveDeployedContracts") {
        super::contract_checks::instantiated(ctx).await?;
    }
    if done("AxelarGateway") {
        super::verification::gateway(ctx).await?;
    }
    if done("RegisterOperators") {
        operators(ctx).await?;
    }
    if done("WaitRegisterProposal") {
        registration(ctx).await?;
    }
    if done("WaitItsHubRegistration") {
        its(ctx).await?;
    }
    Ok(())
}

async fn registration(ctx: &DeployContext) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(&ctx.target_json).await?;
    let router = contract(&ctx.state, "Router", false).await?;
    let chain =
        lcd_cosmwasm_smart_query(&lcd, &router, &json!({"chain_info":ctx.axelar_id})).await?;
    let expected_gateway = contract(&ctx.state, "Gateway", true).await?;
    eyre::ensure!(
        chain.pointer("/gateway/address").and_then(Value::as_str)
            == Some(expected_gateway.as_str()),
        "router gateway registration mismatch"
    );
    eyre::ensure!(
        chain["frozen_status"].as_u64() == Some(0),
        "chain routing is frozen externally; investigate before continuing"
    );
    let multisig = contract(&ctx.state, "Multisig", false).await?;
    let prover = contract(&ctx.state, "MultisigProver", true).await?;
    let authorized = lcd_cosmwasm_smart_query(
        &lcd,
        &multisig,
        &json!({"is_caller_authorized":{"contract_address":prover,"chain_name":ctx.axelar_id}}),
    )
    .await?;
    eyre::ensure!(
        authorized == json!(true),
        "prover is not authorized in Multisig"
    );
    let rewards = contract(&ctx.state, "Rewards", false).await?;
    for pool_contract in [
        multisig,
        contract(&ctx.state, "VotingVerifier", true).await?,
    ] {
        let pool = lcd_cosmwasm_smart_query(&lcd, &rewards, &json!({"rewards_pool":{"pool_id":{"chain_name":ctx.axelar_id,"contract":pool_contract}}})).await?;
        let expected = crate::steps::cosmos_tx::reward_pool_messages(
            ctx.state.env.as_str(),
            &ctx.axelar_id,
            &pool_contract,
            &pool_contract,
        );
        for field in [
            "epoch_duration",
            "rewards_per_epoch",
            "participation_threshold",
        ] {
            eyre::ensure!(
                pool[field] == expected[0]["create_pool"]["params"][field],
                "reward pool parameter mismatch: {field}"
            );
        }
    }
    Ok(())
}

async fn its(ctx: &DeployContext) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(&ctx.target_json).await?;
    let hub = contract(&ctx.state, "InterchainTokenService", false).await?;
    let actual =
        lcd_cosmwasm_smart_query(&lcd, &hub, &json!({"its_chain":{"chain":ctx.axelar_id}})).await?;
    let edge =
        read_contract_address_by_name(&ctx.target_json, &ctx.axelar_id, "InterchainTokenService")
            .await?;
    let actual_edge = actual["its_edge_contract"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("ITS Hub registration missing"))?
        .parse::<alloy::primitives::Address>()?;
    eyre::ensure!(actual_edge == edge, "ITS Hub edge address mismatch");
    let translator = contract(&ctx.state, "ItsAbiTranslator", false).await?;
    eyre::ensure!(
        actual["msg_translator"].as_str() == Some(translator.as_str()),
        "ITS translator mismatch"
    );
    eyre::ensure!(
        actual["truncation"] == json!({"max_uint_bits":256,"max_decimals_when_truncating":255}),
        "ITS truncation mismatch"
    );
    Ok(())
}

async fn operators(ctx: &DeployContext) -> Result<()> {
    let provider = alloy::providers::ProviderBuilder::new().connect_http(ctx.rpc_url.parse()?);
    let address =
        read_contract_address_by_name(&ctx.target_json, &ctx.axelar_id, "Operators").await?;
    let contract = crate::evm::Operators::new(address, &provider);
    for operator in ctx.state.env.axelar_operators() {
        eyre::ensure!(
            contract.isOperator(*operator).call().await?,
            "operator {operator} is missing"
        );
    }
    Ok(())
}
