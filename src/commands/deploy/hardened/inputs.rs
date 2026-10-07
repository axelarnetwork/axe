use super::{
    input_types::{InputSnapshot, PreparedInputs},
    session::Session,
    storage,
    types::Journal,
};
use crate::{
    state::State,
    utils::{artifact_paths_for_step, deployments_root},
};
use alloy::primitives::keccak256;
use eyre::{Result, WrapErr};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Component, Path, PathBuf},
};

tokio::task_local! { static INPUTS: PreparedInputs; }

pub async fn scope<T>(inputs: PreparedInputs, work: impl Future<Output = T>) -> T {
    INPUTS.scope(inputs, work).await
}

pub fn root(state: &State) -> Result<PathBuf> {
    INPUTS
        .try_with(|inputs| inputs.directory.join("artifacts"))
        .map_or_else(|_| deployments_root(&state.target_json), Ok)
}

pub(super) fn artifact_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for name in [
        "ConstAddressDeployer",
        "Create3Deployer",
        "Operators",
        "AxelarGateway",
        "AxelarGasService",
    ] {
        let (implementation, proxy) = artifact_paths_for_step(name, root)
            .ok_or_else(|| eyre::eyre!("missing artifacts for {name}"))?;
        paths.push(PathBuf::from(implementation));
        paths.extend(proxy.map(PathBuf::from));
    }
    let base =
        root.join("node_modules/@axelar-network/interchain-token-service/artifacts/contracts");
    for relative in [
        "utils/TokenManagerDeployer.sol/TokenManagerDeployer.json",
        "interchain-token/InterchainToken.sol/InterchainToken.json",
        "utils/InterchainTokenDeployer.sol/InterchainTokenDeployer.json",
        "token-manager/TokenManager.sol/TokenManager.json",
        "TokenHandler.sol/TokenHandler.json",
        "InterchainTokenService.sol/InterchainTokenService.json",
        "InterchainTokenFactory.sol/InterchainTokenFactory.json",
        "proxies/InterchainProxy.sol/InterchainProxy.json",
    ] {
        paths.push(base.join(relative));
    }
    Ok(paths)
}

pub async fn prepare(state: &State) -> Result<PreparedInputs> {
    prepare_at(state, storage::directory(state)?).await
}

pub(super) async fn prepare_at(state: &State, directory: PathBuf) -> Result<PreparedInputs> {
    let path = directory.join("journal.json");
    let journal: Option<Journal> = if path.exists() {
        Some(serde_json::from_slice(&tokio::fs::read(path).await?)?)
    } else {
        None
    };
    let snapshot = if let Some(hash) = journal.as_ref().and_then(|j| j.inputs_hash) {
        let bytes = tokio::fs::read(directory.join("inputs.json")).await
            .wrap_err("pinned deployment inputs are missing; restore inputs.json from this deployment's backup")?;
        decode(&bytes, hash)?
    } else {
        capture(state).await?
    };
    let inputs = PreparedInputs {
        snapshot,
        directory,
    };
    materialize(&inputs)?;
    Ok(inputs)
}

pub(super) fn decode(bytes: &[u8], expected: alloy::primitives::B256) -> Result<InputSnapshot> {
    eyre::ensure!(
        keccak256(bytes) == expected,
        "deployment inputs.json changed; restore the journal-matching backup, do not edit the fingerprint"
    );
    let snapshot: InputSnapshot = serde_json::from_slice(bytes)?;
    eyre::ensure!(
        snapshot.version == 1,
        "unsupported deployment input snapshot"
    );
    Ok(snapshot)
}

async fn capture(state: &State) -> Result<InputSnapshot> {
    let root = deployments_root(&state.target_json)?;
    let mut artifacts = BTreeMap::new();
    for path in artifact_paths(&root)? {
        let source: Value = serde_json::from_slice(&tokio::fs::read(&path).await
            .wrap_err_with(|| format!("restore original artifact {} before bootstrapping this journal's input snapshot", path.display()))?)?;
        let artifact = ["abi", "bytecode", "deployedBytecode"]
            .into_iter()
            .map(|key| {
                source
                    .get(key)
                    .cloned()
                    .map(|value| (key.into(), value))
                    .ok_or_else(|| eyre::eyre!("{} missing {key}", path.display()))
            })
            .collect::<Result<serde_json::Map<String, Value>>>()?;
        artifacts.insert(
            path.strip_prefix(&root)?.to_string_lossy().into_owned(),
            Value::Object(artifact),
        );
    }
    let mut cosmos_codes = BTreeMap::new();
    for name in ["Gateway", "VotingVerifier", "MultisigProver"] {
        cosmos_codes.insert(
            name.into(),
            crate::cosmos::read_axelar_contract_field(
                &state.target_json,
                &format!("/axelar/contracts/{name}/storeCodeProposalCodeHash"),
            )
            .await?,
        );
    }
    Ok(InputSnapshot {
        version: 1,
        artifacts,
        cosmos_codes,
        fingerprint_inputs: None,
    })
}

pub(super) fn materialize(inputs: &PreparedInputs) -> Result<()> {
    for (name, artifact) in &inputs.snapshot.artifacts {
        eyre::ensure!(
            !name.is_empty()
                && Path::new(name)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "invalid artifact snapshot path"
        );
        storage::atomic_write(
            &inputs.directory.join("artifacts").join(name),
            &serde_json::to_vec(artifact)?,
        )?;
    }
    Ok(())
}

pub async fn cosmos_code_hash(target: &Path, name: &str) -> Result<String> {
    if let Ok(value) = INPUTS.try_with(|i| i.snapshot.cosmos_codes.get(name).cloned()) {
        return value.ok_or_else(|| eyre::eyre!("missing pinned Cosmos code hash for {name}"));
    }
    crate::cosmos::read_axelar_contract_field(
        target,
        &format!("/axelar/contracts/{name}/storeCodeProposalCodeHash"),
    )
    .await
}

pub(super) fn check_fingerprint(actual: &Value) -> Result<()> {
    INPUTS.try_with(|inputs| {
        if let Some(expected) = &inputs.snapshot.fingerprint_inputs {
            let changed = changed_fields(expected, actual);
            eyre::ensure!(changed.is_empty(), "pinned deployment inputs differ at: {}. Restore the listed public settings; artifact/code inputs are already pinned. No transaction was sent", changed.join(", "));
        }
        Ok(())
    }).unwrap_or(Ok(()))
}

pub(super) fn changed_fields(expected: &Value, actual: &Value) -> Vec<String> {
    expected
        .as_object()
        .into_iter()
        .flat_map(|map| map.iter())
        .filter(|(key, value)| actual.get(*key) != Some(*value))
        .map(|(key, _)| key.clone())
        .collect()
}

pub async fn commit(session: &Session, fingerprint_inputs: Value) -> Result<()> {
    if session.journal.lock().await.inputs_hash.is_some() {
        return Ok(());
    }
    let (path, mut snapshot) = INPUTS.try_with(|inputs| {
        Ok::<_, eyre::Report>((
            inputs.directory.join("inputs.json"),
            serde_json::to_value(&inputs.snapshot)?,
        ))
    })??;
    snapshot["fingerprint_inputs"] = fingerprint_inputs;
    let bytes = serde_json::to_vec_pretty(&snapshot)?;
    storage::atomic_write(&path, &bytes)?;
    let mut journal = session.journal.lock().await;
    journal.inputs_hash = Some(keccak256(&bytes));
    storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)?;
    crate::ui::kv("pinned deployment inputs", &path.display().to_string());
    Ok(())
}
