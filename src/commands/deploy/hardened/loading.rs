use eyre::Result;

use super::types::Options;
use crate::state::{State, loading::read};
use crate::types::Network;

pub async fn prepare(chain: &str, network: Option<Network>, options: &Options) -> Result<State> {
    let existing = read(chain, network).await?;
    if let Some(state) = &existing {
        validate_supported_state(state)?;
    }
    super::session::require_hardening();
    let env = existing
        .as_ref()
        .map(|state| state.env)
        .or(network)
        .or_else(|| std::env::var("ENV").ok().and_then(|env| env.parse().ok()))
        .ok_or_else(|| eyre::eyre!("specify --network or ENV"))?;
    let supplied = match &options.plan {
        Some(path) => serde_json::from_slice::<super::types::Plan>(&tokio::fs::read(path).await?)?,
        None => super::environment::load(
            env,
            existing
                .as_ref()
                .and_then(|state| state.hardened_plan.as_ref()),
            |name| std::env::var(name).ok(),
        )?,
    };
    super::plan::validate(&supplied)?;
    if existing.is_none() {
        super::plan::validate_network_thresholds(&supplied, env)?;
    }
    if options.activate {
        let state = existing.as_ref().ok_or_else(|| {
            eyre::eyre!("--activate is only valid at or after the verifier checkpoint")
        })?;
        super::runner::validate_activation(state)?;
    }
    let state = if let Some(state) = existing {
        state
    } else {
        validate_initial_network(chain, env)?;
        crate::commands::init::run(supplied.clone()).await?;
        read(chain, Some(env))
            .await?
            .ok_or_else(|| eyre::eyre!("initialization did not produce state"))?
    };
    eyre::ensure!(
        state.hardened_plan.as_ref() == Some(&supplied),
        "deployment settings differ from the saved plan; restore the original settings to resume"
    );
    Ok(state)
}

pub(super) fn same_deployment(first: &State, second: &State) -> bool {
    first.env == second.env
        && first.axelar_id == second.axelar_id
        && first.target_json == second.target_json
        && first.rpc_url == second.rpc_url
        && first.cosm_salt == second.cosm_salt
        && first.its_salt == second.its_salt
        && first.its_proxy_salt == second.its_proxy_salt
}

/// Initialization uses the same required public plan as `deploy run`.
pub async fn initialize(network: Option<Network>) -> Result<()> {
    crate::ui::require_interactive_deployment()?;
    let chain = std::env::var("CHAIN").map_err(|_| eyre::eyre!("missing CHAIN"))?;
    let env = network
        .or_else(|| std::env::var("ENV").ok().and_then(|env| env.parse().ok()))
        .ok_or_else(|| eyre::eyre!("specify --network or ENV"))?;
    validate_initial_network(&chain, env)?;
    super::session::require_hardening();
    let plan = super::environment::load(env, None, |name| std::env::var(name).ok())?;
    crate::commands::init::run(plan).await
}

fn validate_initial_network(chain: &str, env: Network) -> Result<()> {
    eyre::ensure!(
        std::env::var("CHAIN").ok().as_deref() == Some(chain),
        "CHAIN must match --axelar-id during initialization"
    );
    eyre::ensure!(
        std::env::var("ENV").ok().as_deref() == Some(env.as_str()),
        "ENV must match the selected network during initialization"
    );
    Ok(())
}

pub(super) fn validate_supported_state(state: &State) -> Result<()> {
    eyre::ensure!(
        state.hardened_plan.is_some(),
        "unsupported pre-journal deployment state for {}; this binary cannot resume or migrate it. Preserve the state and reconcile its on-chain actions before proceeding",
        state.axelar_id
    );
    Ok(())
}
