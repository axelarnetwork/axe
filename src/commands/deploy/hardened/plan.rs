use eyre::Result;

use super::types::Plan;
use crate::state::{Step, StepKind, StepStatus, default_steps};

pub fn steps(plan: &Plan) -> Result<Vec<Step>> {
    let mut steps = default_steps();
    for step in &mut steps {
        if let StepKind::TransferOwnership {
            new_owner,
            contract,
        } = &mut step.kind
        {
            *new_owner = match contract.as_str() {
                "Operators" => plan.operators_owner,
                "AxelarGateway" => plan.gateway_owner,
                "AxelarGasService" => plan.gas_service_owner,
                _ => {
                    eyre::bail!("unexpected ownership step");
                }
            };
        }
    }
    Ok(steps)
}

pub fn validate(plan: &Plan) -> Result<()> {
    eyre::ensure!(
        plan.evm_chain_id != 0 && !plan.axelar_chain_id.is_empty(),
        "network identities required"
    );
    let admin: cosmrs::AccountId = plan.prover_admin.parse()?;
    eyre::ensure!(
        admin.prefix() == "axelar",
        "expected an Axelar prover admin address"
    );
    for owner in [
        plan.gateway_owner,
        plan.operators_owner,
        plan.gas_service_owner,
        plan.its_owner,
        plan.factory_owner,
        plan.gateway_operator,
    ] {
        eyre::ensure!(!owner.is_zero(), "zero owner/operator is not allowed");
    }
    for threshold in [plan.voting_threshold, plan.signing_threshold] {
        eyre::ensure!(
            threshold[0] > 0
                && threshold[0] <= threshold[1]
                && u128::from(threshold[0]) * 2 > u128::from(threshold[1]),
            "invalid threshold"
        );
    }
    for amount in [
        &plan.evm_gas_budget,
        &plan.cosmos_fee_budget,
        &plan.reward_amount,
    ] {
        eyre::ensure!(
            amount.parse::<u128>()? > 0,
            "budgets and reward amount must be positive integers"
        );
    }
    eyre::ensure!(
        plan.block_expiry > 0 && plan.confirmation_height > 0,
        "invalid confirmation/poll settings"
    );
    Ok(())
}

pub fn validate_steps(state: &crate::state::State) -> Result<()> {
    let plan = state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing plan"))?;
    let expected = steps(plan)?;
    eyre::ensure!(
        expected.len() == state.steps.len(),
        "deployment step list changed"
    );
    let mut pending = false;
    for (expected, actual) in expected.iter().zip(&state.steps) {
        let fields = serde_json::to_value(expected)?;
        let actual_fields = serde_json::to_value(actual)?;
        for (key, value) in fields
            .as_object()
            .ok_or_else(|| eyre::eyre!("invalid step"))?
        {
            if key != "status" {
                eyre::ensure!(
                    actual_fields.get(key) == Some(value),
                    "{}: step definition differs from approved plan",
                    expected.name
                );
            }
        }
        match actual.status {
            StepStatus::Pending => pending = true,
            StepStatus::Completed => {
                eyre::ensure!(
                    !pending,
                    "non-sequential deployment progress; restore the last valid checkpoint"
                );
            }
        }
    }
    Ok(())
}
