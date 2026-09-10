use eyre::Result;

use super::types::Proposal;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{lcd_query_proposal, read_axelar_config};
use crate::state::{Step, StepStatus, next_pending_step, save_state};
use crate::ui;

#[cfg(test)]
mod tests;

fn is_retryable_instantiation_failure(proposal: &Proposal) -> bool {
    proposal.status == "PROPOSAL_STATUS_FAILED"
        && (proposal.failed_reason == "can not instantiate: unauthorized"
            || proposal
                .failed_reason
                .contains("contract address already exists")
            || (proposal.failed_reason.contains("deployment name")
                && proposal.failed_reason.contains("already in use")))
}

fn reset_after_failure(steps: &mut [Step], proposal: &Proposal) -> bool {
    if !is_retryable_instantiation_failure(proposal) {
        return false;
    }
    let Some(step) = steps
        .iter_mut()
        .find(|step| step.name == "InstantiateChainContracts")
    else {
        return false;
    };
    step.status = StepStatus::Pending;
    true
}

pub async fn recover_failed_instantiation(ctx: &mut DeployContext) -> Result<()> {
    if !next_pending_step(&ctx.state)
        .is_some_and(|(_, step)| step.name == "WaitInstantiateProposal")
    {
        return Ok(());
    }
    let Some(id) = ctx.state.proposals.get("instantiate").copied() else {
        return Ok(());
    };
    let (lcd, _, _, _) = read_axelar_config(&ctx.target_json).await?;
    let proposal: Proposal = serde_json::from_value(lcd_query_proposal(&lcd, id).await?)?;
    if reset_after_failure(&mut ctx.state.steps, &proposal) {
        ui::warn(&format!(
            "proposal {id} failed: {}. Rechecking the deployment before retrying instantiation",
            proposal.failed_reason
        ));
        save_state(&ctx.state).await?;
    }
    Ok(())
}

pub(super) async fn has_pending_proposal(ctx: &DeployContext, lcd: &str) -> Result<bool> {
    let Some(id) = ctx.state.proposals.get("instantiate").copied() else {
        return Ok(false);
    };
    let proposal: Proposal = serde_json::from_value(lcd_query_proposal(lcd, id).await?)?;
    if is_retryable_instantiation_failure(&proposal) {
        return Ok(false);
    }
    match proposal.status.as_str() {
        "PROPOSAL_STATUS_DEPOSIT_PERIOD" | "PROPOSAL_STATUS_VOTING_PERIOD" => {
            ui::info(&format!("reusing pending instantiation proposal {id}"));
            Ok(true)
        }
        _ => eyre::bail!(
            "instantiation proposal {id} is {} but the expected deployment was not found: {}. No replacement proposal was submitted",
            proposal.status,
            proposal.failed_reason
        ),
    }
}
