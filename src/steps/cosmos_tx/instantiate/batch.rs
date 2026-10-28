use eyre::Result;
use serde_json::json;

use super::{
    StepTxContext, reconcile,
    types::{ChainContractAddresses, InstantiatePlan, QueryErrorBody},
};
use crate::commands::deploy::{
    DeployContext,
    hardened::{session, storage, types::CosmosDeployment},
};
use crate::cosmos::{build_execute_msg_any, lcd_cosmwasm_smart_query_typed};
use crate::ui;

pub(super) async fn prepare(
    ctx: &DeployContext,
    tx: &StepTxContext<'_>,
    addresses: &ChainContractAddresses,
    plan: &InstantiatePlan,
) -> Result<CosmosDeployment> {
    let [gateway, verifier, prover] =
        reconcile::predicted_addresses(tx.lcd, &addresses.coordinator, plan).await?;
    let predicted = CosmosDeployment {
        deployment_name: plan.deployment_name.clone(),
        coordinator: addresses.coordinator.clone(),
        salt: plan.salt.clone(),
        code_ids: [plan.codes.gateway, plan.codes.verifier, plan.codes.prover],
        gateway,
        verifier,
        prover,
    };
    for contract in [&predicted.verifier, &addresses.multisig] {
        check_pool_available(tx.lcd, &addresses.rewards, &ctx.axelar_id, contract).await?;
    }
    pin_predictions(&predicted).await?;
    ui::section("Predicted Cosmos contracts — batch 1");
    ui::address("Gateway", &predicted.gateway);
    ui::address(
        "VotingVerifier / verification reward pool",
        &predicted.verifier,
    );
    ui::address("MultisigProver", &predicted.prover);
    ui::info(
        "One atomic batch: instantiate all three contracts, register the deployment, then create both reward pools. Initial reward funding is a separate approved transaction after verification.",
    );
    Ok(predicted)
}

async fn pin_predictions(predicted: &CosmosDeployment) -> Result<()> {
    let session = session::current()?;
    let mut journal = session.journal.lock().await;
    if let Some(saved) = &journal.cosmos_deployment {
        eyre::ensure!(
            saved == predicted,
            "Cosmos deployment predictions changed; restore the approved inputs before submitting"
        );
    } else {
        journal.cosmos_deployment = Some(predicted.clone());
        storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)?;
    }
    Ok(())
}

pub(super) fn registration_messages(
    sender: &str,
    env: &str,
    chain: &str,
    addresses: &ChainContractAddresses,
    predicted: &CosmosDeployment,
) -> Result<Vec<cosmrs::Any>> {
    let mut messages = vec![build_execute_msg_any(
        sender,
        &addresses.coordinator,
        &json!({"register_deployment":{"deployment_name":predicted.deployment_name}}),
    )?];
    for payload in super::super::reward_pools::reward_pool_messages(
        env,
        chain,
        &predicted.verifier,
        &addresses.multisig,
    ) {
        messages.push(build_execute_msg_any(sender, &addresses.rewards, &payload)?);
    }
    Ok(messages)
}

async fn check_pool_available(lcd: &str, rewards: &str, chain: &str, contract: &str) -> Result<()> {
    let query = json!({"rewards_pool":{"pool_id":{"chain_name":chain,"contract":contract}}});
    match lcd_cosmwasm_smart_query_typed(lcd, rewards, &query).await {
        Ok(_) => {
            eyre::bail!(
                "reward pool already exists for {chain} / {contract}; combined deployment would fail. No proposal was sent"
            );
        }
        Err(error) if error.contract_error_body().is_some_and(pool_not_found) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn pool_not_found(body: &str) -> bool {
    serde_json::from_str::<QueryErrorBody>(body).is_ok_and(|error| {
        error.code == 2 && error.message == "rewards pool not found: query wasm contract failed"
    })
}

#[cfg(test)]
mod tests;
