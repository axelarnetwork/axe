use eyre::Result;

use super::{session, types::Plan};
use crate::commands::deploy::DeployContext;
use crate::cosmos::{read_axelar_config, read_axelar_contract_field};
use crate::state::Step;
use crate::ui;

#[cfg(test)]
mod tests;

pub async fn show(ctx: &DeployContext, step: &Step) -> Result<()> {
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
    if matches!(
        step.name.as_str(),
        "AddCosmWasmConfig" | "InstantiateChainContracts"
    ) {
        show_chain_contract_settings(ctx, &plan).await?;
    }
    if step.name == "RegisterDeployment" {
        show_reward_pool_settings(ctx).await?;
    }
    if step.name == "AddRewards" {
        show_initial_reward_funding(ctx, &plan).await?;
    }
    if step.name == "AxelarGateway" {
        ui::kv(
            "Minimum signer rotation delay",
            &format!("{} seconds", ctx.state.env.gateway_rotation_delay_seconds()),
        );
    }
    Ok(())
}

fn show_verifier_thresholds(plan: &Plan) {
    let [votes, voters] = plan.voting_threshold;
    let [signatures, signers] = plan.signing_threshold;
    ui::section("Verifier agreement and proof signatures");
    ui::info(
        "New deployments use fixed network thresholds. Resumed deployments keep their saved thresholds.",
    );
    ui::kv(
        "Votes needed to verify a source-chain event",
        &format!("{votes}/{voters} of total verifier voting weight"),
    );
    ui::info(
        "Verifiers must agree on the event. This does not set governance proposal voting rules.",
    );
    ui::kv(
        "Signatures needed for a gateway proof",
        &format!("{signatures}/{signers} of total signer weight"),
    );
    ui::info("The gateway requires this weight of signatures before it accepts the proof.");
}

async fn show_chain_contract_settings(ctx: &DeployContext, plan: &Plan) -> Result<()> {
    show_verifier_thresholds(plan);
    ui::section("Event verification timing");
    ui::kv(
        "Voting window (BLOCK_EXPIRY)",
        &format!("{} Axelar blocks", plan.block_expiry),
    );
    ui::info("Verifiers must submit their votes before this window expires.");
    ui::kv(
        "Source confirmation setting (CONFIRMATION_HEIGHT)",
        &format!("{} source-chain blocks", plan.confirmation_height),
    );
    ui::info(
        "Stored in VotingVerifier for event verification. Handlers using chain-specific finality may use that policy instead. This is separate from deployment transaction confirmations.",
    );
    ui::section("Contract authorities");
    let governance =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress").await?;
    ui::address("Governance authority", &governance);
    ui::info("Controls governance-only contract settings, including changing the prover admin.");
    ui::address("Prover operational admin", &plan.prover_admin);
    ui::info(
        "Can request verifier-set updates. Derived from the supplied prover-admin mnemonic, or the proposer mnemonic when it is omitted.",
    );
    ui::address(
        "Cosmos contract upgrade admin",
        crate::steps::cosmos_tx::contract_admin(ctx.state.env.as_str()),
    );
    ui::info(
        "Can migrate the Gateway, VotingVerifier and MultisigProver contracts. This role is separate from the prover operational admin.",
    );
    Ok(())
}

async fn show_initial_reward_funding(ctx: &DeployContext, plan: &Plan) -> Result<()> {
    let (_, _, denom, _) = read_axelar_config(&ctx.target_json).await?;
    let amount: u128 = plan.reward_amount.parse()?;
    let total = amount
        .checked_mul(2)
        .ok_or_else(|| eyre::eyre!("total initial reward funding exceeds the supported amount"))?;
    ui::section("Initial reward-pool funding (REWARD_AMOUNT)");
    ui::kv(
        "Deposit into EACH pool",
        &reward_amount_label(amount, &denom),
    );
    ui::kv(
        "Total for both pools, before fees",
        &reward_amount_label(total, &denom),
    );
    ui::info("This funds future verifier rewards. It does not change the rewards per epoch.");
    ui::info("You can top up later. Resuming deployment will not repeat a completed deposit.");
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
        "Reward calculation window (epoch)",
        &format!("{} Axelar blocks", settings.epoch_blocks),
    );
    ui::kv(
        "Rewards shared per epoch, per pool",
        &reward_amount_label(u128::from(settings.rewards_per_epoch_base_units), &denom),
    );
    ui::kv(
        "Participation needed to earn rewards",
        &format!("{numerator}/{denominator}"),
    );
    ui::info(
        "Each verifier must participate in at least this fraction of the pool's events during the epoch to qualify.",
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

fn reward_amount_label(amount: u128, denom: &str) -> String {
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
            "Batch 1: submit an expedited proposal to instantiate Gateway, VotingVerifier and MultisigProver. Display messages and live deposit before approval, then save progress and proceed to the governance wait."
        }
        "WaitInstantiateProposal" | "WaitRegisterProposal" | "WaitItsHubRegistration" => {
            "Monitor the saved proposal until voting ends. Continue only if it passed and matches the approved submission. Ctrl+C is safe while waiting; resume recovers the same proposal."
        }
        "SaveDeployedContracts" => {
            "Read the Coordinator deployment and save its three contract addresses locally, then verify their code and configuration."
        }
        "RegisterDeployment" => {
            "Batch 2: register the deployment and create both reward pools atomically. Submit, save progress and proceed to the governance wait."
        }
        "AddRewards" => {
            "Fund both reward pools with the plan's reward amount. These payments are journaled to prevent duplicate funding on resume."
        }
        "RegisterItsOnHub" => {
            "Batch 3: register the deployed ITS edge with the ITS Hub. Submit the expedited proposal, save progress and proceed to the governance wait."
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
