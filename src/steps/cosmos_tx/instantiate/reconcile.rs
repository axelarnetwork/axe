use base64::Engine;
use eyre::Result;
use serde_json::json;

use super::types::{ContractInfo, ContractResponse, Deployment, InstantiatePlan, QueryErrorBody};
use crate::cosmos::{lcd_cosmwasm_smart_query, lcd_cosmwasm_smart_query_typed};
use crate::evm::get_salt_from_key;

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
