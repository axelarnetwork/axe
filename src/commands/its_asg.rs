mod checks;
mod config;
mod contracts;
mod deploy;
mod handoff;
mod signing;
mod transactions;
pub mod types;

#[cfg(test)]
mod tests;

use eyre::{Result, ensure};

use crate::commands::deploy::hardened::storage;
use crate::{evm::connect_evm, types::Network, ui};
use types::AsgAction;

pub async fn run(network: Network, chain: &str, action: &AsgAction) -> Result<()> {
    let options = match action {
        AsgAction::DeployAsg { options, .. } | AsgAction::HandoffAsg { options, .. } => options,
    };
    if options.apply {
        ui::require_interactive("ASG --apply")?;
        ensure!(
            network != Network::Mainnet || options.confirm_mainnet,
            "mainnet writes require --confirm-mainnet"
        );
    }
    ensure!(!options.from.is_zero(), "--from cannot be zero");
    let mut ctx = config::load(network, chain, options).await?;
    if let AsgAction::DeployAsg { salt, .. } = action {
        deploy::validate_inputs(&ctx.settings, options.from, salt.as_deref())?;
    }
    if options.apply {
        signing::require_operator(ctx.settings.operator)?;
    }
    let _lock = if options.apply {
        Some(storage::config_lock(&ctx.path)?)
    } else {
        None
    };
    config::unchanged(&ctx)?;
    let provider = connect_evm(std::slice::from_ref(&ctx.rpc))?;
    checks::network(&provider, &ctx).await?;
    match action {
        AsgAction::DeployAsg { .. } => deploy::run(&provider, &mut ctx, options).await,
        AsgAction::HandoffAsg { asg, .. } => handoff::run(&provider, &mut ctx, options, *asg).await,
    }
}
