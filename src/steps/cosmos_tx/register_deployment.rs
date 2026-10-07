//! `RegisterDeployment` step. After the chain contracts are instantiated,
//! we ask the Coordinator to mark the deployment as registered. The
//! proposal also creates both reward pools.

use eyre::Result;
use serde_json::json;

use super::StepTxContext;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{build_execute_msg_any, read_axelar_contract_field};
use crate::ui;

pub(super) async fn run_register_deployment(
    ctx: &mut DeployContext,
    tx: StepTxContext<'_>,
) -> Result<()> {
    let StepTxContext {
        axelar_address,
        chain_axelar_id,
        env,
        ..
    } = tx;
    ui::info(&format!("registering deployment for {chain_axelar_id}..."));

    let coordinator_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Coordinator/address")
            .await?;
    let governance_address =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress").await?;

    let deployment_name = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/Coordinator/deployments/{chain_axelar_id}/deploymentName"),
    )
    .await?;

    let execute_msg = json!({
        "register_deployment": {
            "deployment_name": deployment_name
        }
    });

    let sender = if ctx.state.env.deployment_uses_governance() {
        governance_address.as_str()
    } else {
        axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &coordinator_addr, &execute_msg)?;

    let inner_messages =
        registration_messages(ctx, sender, chain_axelar_id, env, inner_msg).await?;
    let title = format!("Register {chain_axelar_id} deployment and reward pools");
    super::submission::submit(ctx, tx, inner_messages, &title).await
}

async fn registration_messages(
    ctx: &DeployContext,
    sender: &str,
    chain_axelar_id: &str,
    env: &str,
    inner_msg: cosmrs::Any,
) -> Result<Vec<cosmrs::Any>> {
    let mut inner_messages = vec![inner_msg];

    inner_messages
        .extend(super::reward_pools::batch_messages(ctx, sender, chain_axelar_id, env).await?);

    Ok(inner_messages)
}
