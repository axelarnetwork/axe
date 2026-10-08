use std::path::PathBuf;

use eyre::Result;

use super::{State, StepStatus, data_dir, read_state_at, state_path};
use crate::types::Network;

pub fn candidates(chain: &str, network: Option<Network>) -> Result<Vec<PathBuf>> {
    eyre::ensure!(
        chain.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "invalid chain identifier"
    );
    let networks = [
        Network::Mainnet,
        Network::Testnet,
        Network::Stagenet,
        Network::DevnetAmplifier,
    ];
    let mut paths = Vec::new();
    for env in networks {
        if network.is_none_or(|network| network == env) {
            let path = data_dir()?
                .join("deployments")
                .join(env.as_str())
                .join(chain)
                .join("state.json");
            if path.exists() {
                paths.push(path);
            }
        }
    }
    let legacy = state_path(chain)?;
    if legacy.exists() {
        paths.push(legacy);
    }
    Ok(paths)
}

pub async fn read(chain: &str, network: Option<Network>) -> Result<Option<State>> {
    read_paths(candidates(chain, network)?, network).await
}

pub(crate) async fn read_paths(
    paths: Vec<PathBuf>,
    network: Option<Network>,
) -> Result<Option<State>> {
    let mut states = Vec::new();
    for path in paths {
        let state = read_state_at(&path).await?;
        if network.is_none_or(|env| env == state.env) {
            states.push((path, state));
        }
    }
    let stale: Vec<_> = states
        .iter()
        .enumerate()
        .filter_map(|(index, (_, legacy))| {
            states
                .iter()
                .any(|(path, current)| {
                    path.file_name().is_some_and(|name| name == "state.json")
                        && path.with_file_name("journal.json").is_file()
                        && current.hardened_plan.is_some()
                        && current.hardened_fingerprint.is_some()
                        && unused_legacy(legacy, current)
                })
                .then_some(index)
        })
        .collect();
    for index in stale.into_iter().rev() {
        let (path, _) = states.remove(index);
        crate::ui::info(&format!(
            "Ignoring unused legacy state {}; the journaled deployment takes precedence. The file is retained.",
            path.display()
        ));
    }
    eyre::ensure!(
        states.len() <= 1,
        "multiple deployments match: {}. Select --network for different networks; for conflicting files on the same network, inspect and explicitly archive the stale file outside the deployment directory before resuming",
        states
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(states.pop().map(|(_, state)| state))
}

fn unused_legacy(legacy: &State, current: &State) -> bool {
    legacy.hardened_plan.is_none()
        && legacy.hardened_fingerprint.is_none()
        && legacy.env == current.env
        && legacy.axelar_id == current.axelar_id
        && legacy.target_json == current.target_json
        && legacy.cosm_salt == current.cosm_salt
        && legacy.proposals.is_empty()
        && legacy
            .steps
            .iter()
            .all(|step| step.status == StepStatus::Pending)
        && legacy.predicted_gateway_address.is_none()
        && legacy.sender_receiver_address.is_none()
}
