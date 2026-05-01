use std::path::PathBuf;
use std::time::Instant;

use alloy::primitives::{Address, FixedBytes};
use alloy::signers::local::PrivateKeySigner;
use eyre::Result;
use serde_json::Value;

use crate::cli::resolve_axelar_id;
use crate::commands;
use crate::preflight;
use crate::state::{
    State, Step, StepKind, mark_step_completed, migrate_steps, next_pending_step, read_state,
    save_state, state_path,
};
use crate::steps;
use crate::ui;
use crate::utils::{artifact_paths_for_step, deployments_root};

pub struct DeployContext {
    pub axelar_id: String,
    pub state: State,
    pub rpc_url: String,
    pub target_json: PathBuf,
}

pub async fn run(
    axelar_id: Option<String>,
    private_key: Option<String>,
    artifact_path: Option<String>,
    salt: Option<String>,
    proxy_artifact_path: Option<String>,
) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;

    if !state_path(&axelar_id)?.exists() {
        ui::info("no state file found, running init…");
        commands::init::run().await?;
    }

    let mut state = read_state(&axelar_id)?;

    // Migrate: append any new steps added since this state was created
    migrate_steps(&mut state);

    // Load ITS config from env vars if not already in state
    if state.its_deployer_private_key.is_none()
        && let Ok(pk) = std::env::var("ITS_DEPLOYER_PRIVATE_KEY")
    {
        state.its_deployer_private_key = Some(pk);
        ui::info("loaded ITS_DEPLOYER_PRIVATE_KEY from env");
    }
    if state.its_salt.is_none()
        && let Ok(s) = std::env::var("ITS_SALT")
    {
        let bytes: FixedBytes<32> = s
            .parse()
            .map_err(|e| eyre::eyre!("invalid ITS_SALT (expected 0x-prefixed 32-byte hex): {e}"))?;
        state.its_salt = Some(bytes);
        ui::info(&format!("loaded ITS_SALT from env: {s}"));
    }
    if state.its_proxy_salt.is_none()
        && let Ok(s) = std::env::var("ITS_PROXY_SALT")
    {
        let bytes: FixedBytes<32> = s.parse().map_err(|e| {
            eyre::eyre!("invalid ITS_PROXY_SALT (expected 0x-prefixed 32-byte hex): {e}")
        })?;
        state.its_proxy_salt = Some(bytes);
        ui::info(&format!("loaded ITS_PROXY_SALT from env: {s}"));
    }

    save_state(&state)?;

    let rpc_url = state.rpc_url.clone();
    let target_json = state.target_json.clone();
    let env = state.env;
    let total_steps = state.steps.len();
    let deploy_start = Instant::now();

    ui::section(&format!("Deploy {axelar_id}"));
    ui::kv("environment", env.as_str());
    ui::kv("rpc", &rpc_url);
    ui::kv("steps", &total_steps.to_string());

    let mut ctx = DeployContext {
        axelar_id,
        state,
        rpc_url,
        target_json,
    };

    // --- Pre-flight: check EVM deployer balances ---
    {
        let mut wallets: Vec<(&str, Address)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (label, pk_str) in [
            ("deployer", ctx.state.deployer_private_key.as_deref()),
            (
                "gateway deployer",
                ctx.state.gateway_deployer_private_key.as_deref(),
            ),
            (
                "gas service deployer",
                ctx.state.gas_service_deployer_private_key.as_deref(),
            ),
            (
                "ITS deployer",
                ctx.state.its_deployer_private_key.as_deref(),
            ),
        ] {
            if let Some(pk) = pk_str
                && let Ok(signer) = pk.parse::<PrivateKeySigner>()
            {
                let addr = signer.address();
                if seen.insert(addr) {
                    wallets.push((label, addr));
                }
            }
        }
        let token_symbol = std::fs::read_to_string(&ctx.target_json)
            .ok()
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .and_then(|root| {
                root.pointer(&format!("/chains/{}/tokenSymbol", ctx.axelar_id))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "ETH".to_string());

        preflight::check_evm_balances(&ctx.rpc_url, &wallets, &token_symbol).await?;
    }

    loop {
        let (step_idx, step_ref) = match next_pending_step(&ctx.state) {
            Some(s) => s,
            None => {
                print_completion_message(&ctx.axelar_id, deploy_start);
                break;
            }
        };

        // Clone the step so step handlers can mutate `ctx.state` (which
        // contains the same step) without holding an immutable borrow.
        let step: Step = step_ref.clone();
        let step_name = step.name.clone();
        let step_start = Instant::now();

        ui::step_header(step_idx + 1, total_steps, &step_name);

        // Resolve artifact paths: CLI flags override built-in defaults
        let repo_root = deployments_root(&ctx.target_json)?;
        let (resolved_artifact, resolved_proxy_artifact) = {
            let defaults = artifact_paths_for_step(&step_name, &repo_root);
            let art = artifact_path
                .clone()
                .or_else(|| defaults.as_ref().map(|(a, _)| a.clone()));
            let proxy_art = proxy_artifact_path
                .clone()
                .or_else(|| defaults.and_then(|(_, p)| p));
            (art, proxy_art)
        };

        // Resolve EVM private key: --private-key flag > state key based on step
        let resolve_evm_key = |step_name: &str| -> Result<String> {
            if let Some(ref pk) = private_key {
                return Ok(pk.clone());
            }
            let (label, pk_opt) = match step_name {
                "EvmCompatibilityCheck" | "ConstAddressDeployer" | "Create3Deployer" => (
                    "deployerPrivateKey",
                    ctx.state.deployer_private_key.as_deref(),
                ),
                "DeployInterchainTokenService" => (
                    "itsDeployerPrivateKey",
                    ctx.state.its_deployer_private_key.as_deref(),
                ),
                "AxelarGateway"
                | "Operators"
                | "RegisterOperators"
                | "TransferOperatorsOwnership"
                | "TransferGatewayOwnership" => (
                    "gatewayDeployerPrivateKey",
                    ctx.state.gateway_deployer_private_key.as_deref(),
                ),
                "TransferGasServiceOwnership" | "AxelarGasService" => (
                    "gasServiceDeployerPrivateKey",
                    ctx.state.gas_service_deployer_private_key.as_deref(),
                ),
                _ => return Err(eyre::eyre!("--private-key required for step {step_name}")),
            };
            pk_opt.map(std::string::ToString::to_string).ok_or_else(|| {
                eyre::eyre!(
                    "no {label} in state and --private-key not provided. Run init with the key or pass --private-key"
                )
            })
        };

        match &step.kind {
            StepKind::EvmCompat => {
                let pk = resolve_evm_key(&step_name)?;
                steps::evm_compat::run(&ctx, &pk).await?;
            }
            StepKind::DeployCreate => {
                let pk = resolve_evm_key(&step_name)?;
                let ap = resolved_artifact
                    .as_ref()
                    .ok_or_else(|| eyre::eyre!("--artifact-path required for deploy steps"))?;
                steps::evm_deploy::run(&mut ctx, &step_name, "deploy-create", &pk, ap, &salt)
                    .await?;
            }
            StepKind::DeployCreate2 => {
                let pk = resolve_evm_key(&step_name)?;
                let ap = resolved_artifact
                    .as_ref()
                    .ok_or_else(|| eyre::eyre!("--artifact-path required for deploy steps"))?;
                steps::evm_deploy::run(&mut ctx, &step_name, "deploy-create2", &pk, ap, &salt)
                    .await?;
            }
            StepKind::RegisterOperators => {
                let pk = resolve_evm_key(&step_name)?;
                steps::register_operators::run(&ctx, &pk).await?;
            }
            StepKind::TransferOwnership { .. } => {
                let pk = resolve_evm_key(&step_name)?;
                steps::transfer_ownership::run(&ctx, &step, &pk).await?;
            }
            StepKind::DeployGateway { .. } => {
                let pk = resolve_evm_key(&step_name)?;
                let impl_art = resolved_artifact.as_ref().ok_or_else(|| {
                    eyre::eyre!("--artifact-path required (implementation artifact)")
                })?;
                let proxy_art = resolved_proxy_artifact.as_ref().ok_or_else(|| {
                    eyre::eyre!("--proxy-artifact-path required (proxy artifact)")
                })?;
                steps::deploy_gateway::run(&mut ctx, step_idx, &step, &pk, impl_art, proxy_art)
                    .await?;
            }
            StepKind::PredictAddress => {
                steps::predict_address::run(&mut ctx).await?;
            }
            StepKind::ConfigEdit => {
                steps::config_edit::run(&ctx)?;
            }
            StepKind::CosmosTx { .. } => {
                steps::cosmos_tx::run(&mut ctx, &step, &step_name).await?;
            }
            StepKind::CosmosPoll { .. } => {
                steps::cosmos_poll::run(&ctx, &step).await?;
            }
            StepKind::CosmosQuery => {
                steps::cosmos_query::run(&ctx).await?;
            }
            StepKind::WaitVerifierSet => {
                steps::wait_verifier_set::run(&ctx).await?;
            }
            StepKind::DeployUpgradable { .. } => {
                let pk = resolve_evm_key(&step_name)?;
                let impl_art = resolved_artifact.as_ref().ok_or_else(|| {
                    eyre::eyre!("--artifact-path required (implementation artifact)")
                })?;
                let proxy_art = resolved_proxy_artifact.as_ref().ok_or_else(|| {
                    eyre::eyre!("--proxy-artifact-path required (proxy artifact)")
                })?;
                steps::deploy_upgradable::run(
                    &mut ctx, step_idx, &step, &step_name, &pk, impl_art, proxy_art,
                )
                .await?;
            }
            StepKind::DeployIts { .. } => {
                let pk = resolve_evm_key(&step_name)?;
                steps::deploy_its::run(&mut ctx, step_idx, &step, &pk).await?;
            }
        }

        mark_step_completed(&mut ctx.state, step_idx);
        save_state(&ctx.state)?;
        ui::success(&format!(
            "{step_name} completed ({})",
            ui::format_elapsed(step_start)
        ));
    }

    Ok(())
}

fn print_completion_message(axelar_id: &str, deploy_start: Instant) {
    ui::section("Deployment Complete");
    ui::success(&format!(
        "All steps completed for {axelar_id} ({})",
        ui::format_elapsed(deploy_start)
    ));
    println!();
    ui::info(&format!(
        "Run an end-to-end GMP test: cargo run -- test gmp --axelar-id {axelar_id}"
    ));
}
