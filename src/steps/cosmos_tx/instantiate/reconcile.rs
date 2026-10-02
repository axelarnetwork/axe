use base64::Engine;
use eyre::Result;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::types::{
    ContractInfo, ContractResponse, Deployment, InstantiatePlan, ProverConfig, QueryErrorBody,
    RawResponse, VerifierConfig,
};
use crate::commands::deploy::DeployContext;
use crate::cosmos::{lcd_cosmwasm_smart_query, lcd_cosmwasm_smart_query_typed};
use crate::evm::get_salt_from_key;
use crate::ui;

#[cfg(test)]
mod tests;

pub(super) fn chain_salt_key(chain: &str, salt: &str) -> String {
    format!("Coordinator:{chain}:{salt}")
}

fn deployment_not_found(body: &str, name: &str) -> bool {
    serde_json::from_str::<QueryErrorBody>(body).is_ok_and(|error| {
        error.code == 2
            && error
                .message
                .starts_with(&format!("deployment {name} not found:"))
    })
}

pub(super) async fn find_deployment(
    lcd: &str,
    coordinator: &str,
    name: &str,
) -> Result<Option<Deployment>> {
    let query = json!({"deployment": {"deployment_name": name}});
    match lcd_cosmwasm_smart_query_typed(lcd, coordinator, &query).await {
        Ok(value) => Ok(Some(serde_json::from_value(value)?)),
        Err(error)
            if error
                .contract_error_body()
                .is_some_and(|body| deployment_not_found(body, name)) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

async fn predicted_addresses(
    lcd: &str,
    coordinator: &str,
    plan: &InstantiatePlan,
    salt_key: &str,
) -> Result<[String; 3]> {
    let salt = base64::engine::general_purpose::STANDARD.encode(get_salt_from_key(salt_key));
    let mut addresses = [String::new(), String::new(), String::new()];
    for (address, code_id) in
        addresses
            .iter_mut()
            .zip([plan.codes.gateway, plan.codes.verifier, plan.codes.prover])
    {
        let query = json!({"instantiate2_address": {"code_id": code_id, "salt": salt}});
        *address =
            serde_json::from_value(lcd_cosmwasm_smart_query(lcd, coordinator, &query).await?)?;
    }
    Ok(addresses)
}

async fn contract_info(lcd: &str, address: &str) -> Result<Option<ContractInfo>> {
    let url = format!(
        "{}/cosmwasm/wasm/v1/contract/{address}",
        lcd.trim_end_matches('/')
    );
    let response = crate::http::client().get(url).send().await?;
    let status = response.status();
    let body = response.text().await?;
    if contract_not_found(status, &body, address) {
        return Ok(None);
    }
    eyre::ensure!(
        status.is_success(),
        "contract info query for {address} failed: HTTP {status}"
    );
    Ok(Some(
        serde_json::from_str::<ContractResponse>(&body)?.contract_info,
    ))
}

fn contract_not_found(status: reqwest::StatusCode, body: &str, address: &str) -> bool {
    serde_json::from_str::<QueryErrorBody>(body).is_ok_and(|error| {
        (status == reqwest::StatusCode::NOT_FOUND && error.code == 5)
            || (error.code == 2
                && error.message
                    == format!("codespace wasm code 22: no such contract: address {address}"))
    })
}

async fn raw_config<T: DeserializeOwned>(lcd: &str, address: &str) -> Result<T> {
    let url = format!(
        "{}/cosmwasm/wasm/v1/contract/{address}/raw/Y29uZmln",
        lcd.trim_end_matches('/')
    );
    let response: RawResponse = crate::http::client()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(response.data)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn validate_identity(
    chain: &str,
    gateway: alloy::primitives::Address,
    plan: &InstantiatePlan,
    deployment: &Deployment,
    verifier: &VerifierConfig,
    prover: &ProverConfig,
) -> Result<()> {
    eyre::ensure!(
        deployment.chain_name == chain
            && verifier.source_chain == chain
            && prover.chain_name == chain,
        "existing deployment belongs to a different chain"
    );
    eyre::ensure!(
        verifier
            .source_gateway_address
            .parse::<alloy::primitives::Address>()?
            == gateway,
        "existing deployment uses a different source gateway"
    );
    eyre::ensure!(
        alloy::hex::encode(prover.domain_separator) == plan.domain_separator
            && prover.gateway == deployment.gateway_address
            && prover.voting_verifier == deployment.verifier_address,
        "existing deployment has incompatible prover configuration"
    );
    Ok(())
}

pub(super) async fn reuse_existing(
    ctx: &DeployContext,
    lcd: &str,
    coordinator: &str,
    chain: &str,
    plan: &mut InstantiatePlan,
) -> Result<bool> {
    let Some(deployment) = find_deployment(lcd, coordinator, &plan.deployment_name).await? else {
        return Ok(false);
    };
    eyre::ensure!(
        deployment.chain_name == chain,
        "deployment name is already used by another chain"
    );
    let actual = [
        &deployment.gateway_address,
        &deployment.verifier_address,
        &deployment.prover_address,
    ];
    let predicted = predicted_addresses(lcd, coordinator, plan, &plan.salt_key).await?;
    if actual != predicted.each_ref() {
        let legacy = predicted_addresses(lcd, coordinator, plan, &ctx.state.cosm_salt).await?;
        eyre::ensure!(
            actual == legacy.each_ref(),
            "existing deployment uses different contract addresses or salt"
        );
        plan.salt_key = ctx.state.cosm_salt.clone();
    }
    for (address, code_id) in
        actual
            .into_iter()
            .zip([plan.codes.gateway, plan.codes.verifier, plan.codes.prover])
    {
        let info = contract_info(lcd, address)
            .await?
            .ok_or_else(|| eyre::eyre!("existing deployment contract {address} is missing"))?;
        eyre::ensure!(
            info.code_id == code_id.to_string()
                && info.creator == coordinator
                && info.admin == plan.contract_admin,
            "existing deployment contract {address} has incompatible code, creator or admin"
        );
    }
    let verifier = raw_config(lcd, &deployment.verifier_address).await?;
    let prover = raw_config(lcd, &deployment.prover_address).await?;
    let gateway = ctx
        .state
        .predicted_gateway_address
        .ok_or_else(|| eyre::eyre!("predicted source gateway address is missing"))?;
    validate_identity(chain, gateway, plan, &deployment, &verifier, &prover)?;
    ui::success(&format!(
        "reusing existing deployment {}",
        plan.deployment_name
    ));
    Ok(true)
}

pub(super) async fn check_addresses_available(
    lcd: &str,
    coordinator: &str,
    plan: &InstantiatePlan,
) -> Result<()> {
    for address in predicted_addresses(lcd, coordinator, plan, &plan.salt_key).await? {
        if let Some(info) = contract_info(lcd, &address).await? {
            eyre::bail!(
                "cannot instantiate {}: address {address} is already occupied by '{}'. No proposal was submitted",
                plan.deployment_name,
                info.label
            );
        }
    }
    Ok(())
}
