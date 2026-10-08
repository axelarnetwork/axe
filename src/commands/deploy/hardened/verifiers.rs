use eyre::{Result, WrapErr};
use serde_json::{Value, json};

use super::session;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, derive_axelar_wallet, lcd_cosmwasm_smart_query, read_axelar_config,
    read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::state::State;

pub async fn contract(state: &State, name: &str, per_chain: bool) -> Result<String> {
    let path = if per_chain {
        format!("/axelar/contracts/{name}/{}/address", state.axelar_id)
    } else {
        format!("/axelar/contracts/{name}/address")
    };
    read_axelar_contract_field(&state.target_json, &path).await
}

pub async fn execute(state: &State, mnemonic: &str, target: &str, message: &Value) -> Result<()> {
    let (lcd, chain, denom, price) = read_axelar_config(&state.target_json).await?;
    let (key, address) = derive_axelar_wallet(mnemonic)?;
    let message = build_execute_msg_any(&address, target, message)?;
    sign_and_broadcast_cosmos_tx(&key, &address, &lcd, &chain, &denom, price, vec![message])
        .await?;
    Ok(())
}

pub(super) async fn service(
    state: &State,
    lcd: &str,
    registry: &str,
) -> Result<super::types::Service> {
    let name = state.env.verifier_service_name();
    let response = lcd_cosmwasm_smart_query(
        lcd,
        registry,
        &json!({"service":{"service_name":name,"chain_name":state.axelar_id}}),
    )
    .await
    .wrap_err_with(|| {
        format!(
            "cannot query ServiceRegistry service {name} on {} before deployment",
            state.env
        )
    })?;
    serde_json::from_value(response).wrap_err_with(|| {
        format!(
            "invalid or missing ServiceRegistry service {name} on {}",
            state.env
        )
    })
}

pub async fn initialize(ctx: &mut DeployContext) -> Result<()> {
    if session::current()?
        .journal
        .lock()
        .await
        .initial_signers
        .is_some()
    {
        return Ok(());
    }
    let prover = contract(&ctx.state, "MultisigProver", true).await?;
    let existing = super::verifier_set::query(&ctx.state).await?;
    if existing.is_none() {
        super::verifier_set::show_candidates(&ctx.state).await?;
        crate::steps::prover_admin::validate(&mut ctx.state).await?;
        let mnemonic = ctx
            .state
            .admin_mnemonic
            .as_deref()
            .ok_or_else(|| eyre::eyre!("missing prover admin credential"))?;
        execute(&ctx.state, mnemonic, &prover, &json!("update_verifier_set")).await?;
    }
    let current = super::verifier_set::query(&ctx.state)
        .await?
        .ok_or_else(|| session::pause("prover set is not available yet; resume with --activate"))?;
    super::verifier_set::approve(&ctx.state, current).await
}
