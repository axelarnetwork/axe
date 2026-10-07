use std::sync::Arc;

use eyre::Result;

use super::types::{Options, Paused};
use super::{preflight, session, storage};
use crate::commands::deploy::{DeployContext, StepExecution, execute_step, resolve_evm_key};
use crate::state::{StepKind, StepStatus, mark_step_completed, next_pending_step, save_state};
use crate::ui;
use crate::utils::artifact_paths_for_step;

pub async fn run(mut ctx: DeployContext, options: &Options) -> Result<()> {
    let directory = storage::directory(&ctx.state)?;
    let _lock = storage::lock(&directory.join("run.lock"))?;
    let _config_lock = storage::config_lock(&ctx.target_json)?;
    let result = async {
        reload_committed_state(&mut ctx).await?;
        validate_journal_presence(&ctx.state, &directory.join("journal.json"))?;
        let inputs = super::inputs::prepare(&ctx.state).await?;
        super::inputs::scope(inputs, run_with_inputs(&mut ctx, options)).await
    }
    .await;
    match result {
        Err(error) if error.downcast_ref::<Paused>().is_some() => {
            save_state(&ctx.state).await?;
            super::handoff::stopped(&ctx.state, &error, true);
            Ok(())
        }
        Err(error) => {
            super::handoff::stopped(&ctx.state, &error, false);
            Err(error)
        }
        Ok(()) => Ok(()),
    }
}

async fn run_with_inputs(ctx: &mut DeployContext, options: &Options) -> Result<()> {
    let directory = storage::directory(&ctx.state)?;
    let plan = ctx
        .state
        .hardened_plan
        .clone()
        .ok_or_else(|| eyre::eyre!("missing hardened plan"))?;
    crate::commands::deploy::configuration::load_missing_environment(&mut ctx.state, |name| {
        std::env::var(name).ok()
    });
    crate::commands::deploy::configuration::validate_state(&mut ctx.state)?;
    crate::steps::prover_admin::validate(&mut ctx.state).await?;
    crate::steps::cosmos_tx::check_instantiate_permissions(&ctx.state).await?;
    session::require_hardening();
    if options.activate {
        validate_activation(&ctx.state)?;
    }
    let confirmation_policy = super::confirmations::resolve(&ctx.state, options).await?;
    let checked = preflight::validate(&ctx.state, confirmation_policy).await?;
    let fingerprint = checked.fingerprint;
    validate_journal_presence(&ctx.state, &directory.join("journal.json"))?;
    let mut session = session::Session::load(
        directory.join("journal.json"),
        fingerprint,
        plan,
        ctx.rpc_url.clone(),
    )
    .await?;
    super::inputs::commit(&session, checked.fingerprint_inputs).await?;
    session.options = options.clone();
    super::confirmations::persist(&session, confirmation_policy).await?;
    let session = Arc::new(session);
    ctx.state.hardened_fingerprint = Some(fingerprint);
    save_state(&ctx.state).await?;
    session::scope(
        session,
        Box::pin(async {
            super::protocols::approve(checked.protocols).await?;
            execute(ctx, options.activate).await
        }),
    )
    .await
}

async fn execute(ctx: &mut DeployContext, activate: bool) -> Result<()> {
    let transactions: Vec<_> = session::current()?
        .journal
        .lock()
        .await
        .actions
        .iter()
        .map(|(key, tx)| (key.clone(), tx.clone()))
        .collect();
    let (lcd, _, _, _) = crate::cosmos::read_axelar_config(&ctx.target_json).await?;
    super::recovery::validate_options(&transactions)?;
    if !ctx.state.env.deployment_uses_governance() {
        for (_, transaction) in &transactions {
            if matches!(transaction, super::types::Transaction::Cosmos { .. }) {
                super::direct::validate_submission(transaction)?;
            }
        }
    }
    for (key, transaction) in transactions {
        let step = key
            .split('/')
            .next()
            .ok_or_else(|| eyre::eyre!("invalid action key"))?;
        let signer = signer_for(ctx, step)?;
        session::step(step.into(), signer, async {
            if super::recovery::retry_failed(ctx, &key, &transaction, &lcd).await? {
                return Ok(());
            }
            super::evm::reconcile(&key, &transaction, &ctx.rpc_url).await?;
            super::cosmos::reconcile(ctx, &key, &transaction, &lcd).await
        })
        .await?;
    }
    super::evidence::check(ctx).await?;
    super::postconditions::check(ctx, None).await?;
    while let Some((index, pending)) = next_pending_step(&ctx.state) {
        let step = pending.clone();
        if step.name == "WaitForVerifierSet" && !activate {
            super::handoff::verifiers(&ctx.state).await?;
            return Err(session::pause(
                "Deployment paused for verifier setup. Progress saved; you can close axe.",
            ));
        }
        let recovered = if let StepKind::CosmosTx { proposal_key } = &step.kind {
            proposal_key != "addRewards"
                && super::governance::recover(ctx, &step.name, proposal_key).await?
        } else {
            false
        };
        if !recovered {
            super::preview::approve(ctx, &step).await?;
        }
        let key = signer_for(ctx, &step.name)?;
        let defaults = artifact_paths_for_step(&step.name, &super::inputs::root(&ctx.state)?);
        let artifact = defaults.as_ref().map(|(path, _)| path.clone());
        let proxy = defaults.and_then(|(_, proxy)| proxy);
        ui::step_header(index + 1, ctx.state.steps.len(), &step.name);
        session::step(step.name.clone(), key, async {
            if recovered {
                return Ok(());
            }

            execute_step(
                ctx,
                StepExecution {
                    step_idx: index,
                    step: &step,
                    artifact: artifact.as_ref(),
                    proxy_artifact: proxy.as_ref(),
                },
            )
            .await
        })
        .await?;
        super::postconditions::check(ctx, Some(&step.name)).await?;
        super::evidence::capture(ctx).await?;
        mark_step_completed(&mut ctx.state, index);
        save_state(&ctx.state).await?;
        if ctx.state.env.deployment_uses_governance()
            && matches!(step.kind, StepKind::CosmosTx { .. })
            && step.name != "AddRewards"
        {
            return Err(super::handoff::submitted(&ctx.state, &step));
        }
    }
    eyre::ensure!(
        ctx.state
            .steps
            .iter()
            .all(|step| step.status == StepStatus::Completed),
        "incomplete deployment"
    );
    super::verification::completed(ctx).await?;
    ui::success("Hardened deployment complete. Run the separately approved GMP/ITS smoke tests.");
    Ok(())
}

fn signer_for(ctx: &DeployContext, name: &str) -> Result<String> {
    match name {
        "AddCosmWasmConfig"
        | "PredictGatewayAddress"
        | "InstantiateChainContracts"
        | "WaitInstantiateProposal"
        | "SaveDeployedContracts"
        | "RegisterDeployment"
        | "WaitRegisterProposal"
        | "AddRewards"
        | "RegisterItsOnHub"
        | "WaitItsHubRegistration"
        | "WaitForVerifierSet" => Ok(String::new()),
        _ => resolve_evm_key(&ctx.state, name),
    }
}

async fn reload_committed_state(ctx: &mut DeployContext) -> Result<()> {
    let path = storage::directory(&ctx.state)?.join("state.json");
    if path.exists() {
        let current = crate::state::read_state_at(&path).await?;
        eyre::ensure!(
            super::loading::same_deployment(&current, &ctx.state)
                && current.hardened_plan == ctx.state.hardened_plan,
            "deployment changed while acquiring its lock"
        );
        ctx.state = current;
    }
    Ok(())
}

pub(super) fn validate_journal_presence(
    state: &crate::state::State,
    path: &std::path::Path,
) -> Result<()> {
    eyre::ensure!(
        path.exists()
            || (state.hardened_fingerprint.is_none()
                && state
                    .steps
                    .iter()
                    .all(|step| step.status == StepStatus::Pending)),
        "deployment journal is missing; restore it from backup before continuing. A replacement journal could duplicate transactions"
    );
    Ok(())
}

pub(super) fn validate_activation(state: &crate::state::State) -> Result<()> {
    let checkpoint = state
        .steps
        .iter()
        .position(|step| step.name == "WaitForVerifierSet")
        .ok_or_else(|| eyre::eyre!("missing verifier checkpoint"))?;
    eyre::ensure!(
        state.steps[..checkpoint]
            .iter()
            .all(|step| step.status == StepStatus::Completed),
        "--activate is only valid at or after the verifier checkpoint; first run without it"
    );
    Ok(())
}
