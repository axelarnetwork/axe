use eyre::Result;

use super::StepTxContext;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{build_submit_proposal_any, extract_proposal_id, sign_and_broadcast_cosmos_tx};
use crate::ui;

pub(super) async fn submit(
    ctx: &mut DeployContext,
    tx: StepTxContext<'_>,
    inner: Vec<cosmrs::Any>,
    title: &str,
) -> Result<()> {
    let governance = ctx.state.env.deployment_uses_governance();
    let messages = messages(ctx.state.env, tx, inner, title).await?;
    let response = sign_and_broadcast_cosmos_tx(
        tx.signing_key,
        tx.axelar_address,
        tx.lcd,
        tx.chain_id,
        tx.fee_denom,
        tx.gas_price,
        messages,
    )
    .await?;
    if governance {
        let id = extract_proposal_id(&response)?;
        ctx.state.proposals.insert(tx.proposal_key.into(), id);
        ui::kv("proposal submitted", &id.to_string());
    } else {
        ui::success("Direct execution confirmed; no governance wait");
    }
    Ok(())
}

async fn messages(
    network: crate::types::Network,
    tx: StepTxContext<'_>,
    inner: Vec<cosmrs::Any>,
    title: &str,
) -> Result<Vec<cosmrs::Any>> {
    let messages = if network.deployment_uses_governance() {
        let deposit =
            crate::commands::deploy::hardened::governance::deposit(tx.lcd, tx.fee_denom).await?;
        vec![build_submit_proposal_any(
            tx.axelar_address,
            inner,
            title,
            title,
            &deposit,
            tx.fee_denom,
            true,
        )?]
    } else {
        inner
    };
    Ok(messages)
}

#[cfg(test)]
#[path = "submission_tests.rs"]
mod tests;
