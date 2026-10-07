use eyre::Result;

use super::{session, types::Plan};
use crate::commands::deploy::DeployContext;
use crate::cosmos::read_axelar_config;
use crate::state::Step;
use crate::ui;

#[cfg(test)]
mod tests;

pub async fn approve(ctx: &DeployContext, step: &Step) -> Result<()> {
    let plan = session::current()?.plan.clone();
    ui::section(&step.name);
    ui::kv(
        "EVM chain",
        &format!("{} ({})", ctx.axelar_id, plan.evm_chain_id),
    );
    ui::kv("Axelar chain", &plan.axelar_chain_id);
    if !ctx.state.env.deployment_uses_governance()
        && matches!(
            step.kind,
            crate::state::StepKind::CosmosTx { .. } | crate::state::StepKind::CosmosPoll { .. }
        )
        && step.name != "AddRewards"
    {
        ui::info(
            "Devnet: execute directly with the deployer authority or verify the recorded execution. No governance proposal, deposit or voting wait.",
        );
    } else {
        ui::info(description(&step.name));
    }
    show_authority(&plan, &step.name);
    if step.name == "RegisterDeployment" {
        show_reward_pool_settings(ctx).await?;
    }
    if step.name == "AxelarGateway" {
        ui::kv(
            "Minimum signer rotation delay",
            &format!("{} seconds", ctx.state.env.gateway_rotation_delay_seconds()),
        );
    }
    if !ui::confirm("Proceed with this step?").await {
        return Err(session::pause(
            "Step declined. This step remains pending. Run the continue command when you are ready to review and approve it.",
        ));
    }
    Ok(())
}

async fn show_reward_pool_settings(ctx: &DeployContext) -> Result<()> {
    let settings = crate::steps::cosmos_tx::reward_pool_settings(ctx.state.env.as_str());
    let (_, _, denom, _) = read_axelar_config(&ctx.target_json).await?;
    let [numerator, denominator] = settings.participation_threshold;
    ui::section("Reward settings for both pools");
    ui::kv(
        "Pools",
        "Verification (VotingVerifier) and signing (Multisig)",
    );
    ui::kv(
        "Epoch length",
        &format!("{} Axelar blocks", settings.epoch_blocks),
    );
    ui::kv(
        "Rewards per epoch, per pool",
        &reward_amount_label(settings.rewards_per_epoch_base_units, &denom),
    );
    ui::kv(
        "Required participation",
        &format!("{numerator}/{denominator}"),
    );
    ui::info(
        "Each pool's reward amount is shared among qualifying verifiers, not paid to each verifier.",
    );
    ui::info("These network settings will be submitted in the pool-creation messages below.");
    ui::info(
        "REWARD_AMOUNT is a separate initial deposit into each pool, sent later in AddRewards.",
    );
    Ok(())
}

fn reward_amount_label(amount: u64, denom: &str) -> String {
    if denom == "uaxl" {
        format!(
            "{}.{:06} AXL ({amount} uaxl)",
            amount / 1_000_000,
            amount % 1_000_000
        )
    } else {
        format!("{amount} {denom}")
    }
}

fn description(name: &str) -> &'static str {
    match name {
        "EvmCompatibilityCheck" => {
            "Check RPC capabilities and deploy/update a small compatibility probe. This costs gas."
        }
        "ConstAddressDeployer" | "Create3Deployer" => {
            "Deploy deterministic deployment infrastructure. Each transaction receives its own preview and approval."
        }
        "AxelarGateway" => {
            "Deploy gateway implementation and proxy with the verified initial signer set, as in the original flow. No later bootstrap transaction is needed."
        }
        "Operators" => "Deploy the operator access-control contract.",
        "RegisterOperators" => {
            "Register the network's configured Axelar operator addresses. Each new address is displayed before submission."
        }
        "AxelarGasService" => "Deploy and initialize the gas service implementation and proxy.",
        "DeployInterchainTokenService" => {
            "Deploy ITS helpers, implementations, service proxy and factory proxy. Initial proxy owners are set to the configured recipients in the deployment transactions; no extra ownership transfers are sent."
        }
        "PredictGatewayAddress" => {
            "Pin the future gateway proxy address from its deployer nonce. Do not use that wallet for other EVM transactions until gateway deployment completes."
        }
        "AddCosmWasmConfig" => {
            "Write the approved verifier/prover settings and predicted gateway address to the local deployment configuration."
        }
        "InstantiateChainContracts" => {
            "Batch 1: submit an expedited proposal to instantiate Gateway, VotingVerifier and MultisigProver. Display messages and live deposit before approval, then save and exit."
        }
        "WaitInstantiateProposal" | "WaitRegisterProposal" | "WaitItsHubRegistration" => {
            "Check the saved proposal once. Continue only if it passed and its contents match the approved submission; otherwise save and exit."
        }
        "SaveDeployedContracts" => {
            "Read the Coordinator deployment and save its three contract addresses locally, then verify their code and configuration."
        }
        "RegisterDeployment" => {
            "Batch 2: register the deployment and create both reward pools atomically. Submit, save and exit."
        }
        "AddRewards" => {
            "Fund both reward pools with the plan's reward amount. These payments are journaled to prevent duplicate funding on resume."
        }
        "RegisterItsOnHub" => {
            "Batch 3: register the deployed ITS edge with the ITS Hub. Submit the expedited proposal, save and exit."
        }
        "WaitForVerifierSet" => {
            "Discover eligible verifiers and check the service minimum, initialize the prover if needed, then review and approve its actual initial signer set. Known names are informational."
        }
        _ => {
            "Transfer authority to the approved recipient below and verify the on-chain result. A previously completed transfer is not resent."
        }
    }
}

fn show_authority(plan: &Plan, name: &str) {
    let address = match name {
        "TransferGatewayOwnership" => Some(plan.gateway_owner),
        "TransferOperatorsOwnership" => Some(plan.operators_owner),
        "TransferGasServiceOwnership" => Some(plan.gas_service_owner),
        _ => None,
    };
    if name == "DeployInterchainTokenService" {
        ui::address("initial ITS owner", &plan.its_owner.to_string());
        ui::address("initial factory owner", &plan.factory_owner.to_string());
    }
    if name == "AxelarGateway" {
        ui::address(
            "initial gateway operator",
            &plan.gateway_operator.to_string(),
        );
    }
    if let Some(address) = address {
        ui::address("authority recipient", &address.to_string());
    }
}
