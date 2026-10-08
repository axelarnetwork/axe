use std::path::PathBuf;

use eyre::Result;

use crate::cli::resolve_axelar_id;
use crate::state::{State, Step, StepKind};
use crate::steps;

pub(crate) mod configuration;
pub(crate) mod hardened;

pub struct DeployContext {
    pub axelar_id: String,
    pub state: State,
    pub rpc_url: String,
    pub target_json: PathBuf,
}

fn resolve_evm_key(state: &State, step_name: &str) -> Result<String> {
    let (label, private_key) = match step_name {
        "EvmCompatibilityCheck" | "ConstAddressDeployer" | "Create3Deployer" => {
            ("deployerPrivateKey", state.deployer_private_key.as_deref())
        }
        "DeployInterchainTokenService" => (
            "itsDeployerPrivateKey",
            state.its_deployer_private_key.as_deref(),
        ),
        "AxelarGateway"
        | "Operators"
        | "RegisterOperators"
        | "TransferOperatorsOwnership"
        | "TransferGatewayOwnership" => (
            "gatewayDeployerPrivateKey",
            state.gateway_deployer_private_key.as_deref(),
        ),
        "TransferGasServiceOwnership" | "AxelarGasService" => (
            "gasServiceDeployerPrivateKey",
            state.gas_service_deployer_private_key.as_deref(),
        ),
        _ => return Err(eyre::eyre!("unknown EVM signing role for step {step_name}")),
    };
    private_key.map(str::to_string).ok_or_else(|| {
        eyre::eyre!("missing {label}; restore the deployment role key in the environment")
    })
}

struct StepExecution<'a> {
    step_idx: usize,
    step: &'a Step,
    artifact: Option<&'a String>,
    proxy_artifact: Option<&'a String>,
}

async fn execute_step(ctx: &mut DeployContext, execution: StepExecution<'_>) -> Result<()> {
    let step_name = &execution.step.name;
    let private_key = |state: &State| resolve_evm_key(state, step_name);
    let artifact = || {
        execution
            .artifact
            .ok_or_else(|| eyre::eyre!("deployment artifact path is missing"))
    };
    match &execution.step.kind {
        StepKind::EvmCompat => steps::evm_compat::run(ctx, &private_key(&ctx.state)?).await,
        StepKind::DeployCreate => {
            steps::evm_deploy::run(
                ctx,
                step_name,
                "deploy-create",
                &private_key(&ctx.state)?,
                artifact()?,
            )
            .await
        }
        StepKind::DeployCreate2 => {
            steps::evm_deploy::run(
                ctx,
                step_name,
                "deploy-create2",
                &private_key(&ctx.state)?,
                artifact()?,
            )
            .await
        }
        StepKind::RegisterOperators => {
            steps::register_operators::run(ctx, &private_key(&ctx.state)?).await
        }
        StepKind::TransferOwnership { .. } => {
            steps::transfer_ownership::run(ctx, execution.step, &private_key(&ctx.state)?).await
        }
        StepKind::DeployGateway { .. } => {
            let proxy = execution
                .proxy_artifact
                .ok_or_else(|| eyre::eyre!("proxy artifact path is missing"))?;
            steps::deploy_gateway::run(
                ctx,
                execution.step_idx,
                &private_key(&ctx.state)?,
                artifact()?,
                proxy,
            )
            .await
        }
        StepKind::PredictAddress => steps::predict_address::run(ctx).await,
        StepKind::ConfigEdit => steps::config_edit::run(ctx).await,
        StepKind::CosmosTx { .. } => steps::cosmos_tx::run(ctx, execution.step, step_name).await,
        StepKind::CosmosPoll { .. } => steps::cosmos_poll::run(ctx, execution.step).await,
        StepKind::CosmosQuery => steps::cosmos_query::run(ctx).await,
        StepKind::WaitVerifierSet => hardened::verifiers::initialize(ctx).await,
        StepKind::DeployUpgradable { .. } => {
            let implementation = execution
                .artifact
                .ok_or_else(|| eyre::eyre!("implementation artifact path is missing"))?;
            let proxy = execution
                .proxy_artifact
                .ok_or_else(|| eyre::eyre!("proxy artifact path is missing"))?;
            steps::deploy_upgradable::run(
                ctx,
                execution.step_idx,
                step_name,
                &private_key(&ctx.state)?,
                implementation,
                proxy,
            )
            .await
        }
        StepKind::DeployIts { .. } => {
            steps::deploy_its::run(
                ctx,
                execution.step_idx,
                execution.step,
                &private_key(&ctx.state)?,
            )
            .await
        }
    }
}

pub async fn run(
    axelar_id: Option<String>,
    options: hardened::types::Options,
    network: Option<crate::types::Network>,
) -> Result<()> {
    crate::ui::require_interactive_deployment()?;
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let state = hardened::loading::prepare(&axelar_id, network, &options).await?;
    let ctx = DeployContext {
        axelar_id,
        rpc_url: state.rpc_url.clone(),
        target_json: state.target_json.clone(),
        state,
    };
    hardened::runner::run(ctx, &options).await
}
