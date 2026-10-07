use std::collections::BTreeMap;

use alloy::primitives::{Address, keccak256};
use alloy::providers::{Provider, ProviderBuilder};
use eyre::Result;
use serde_json::Value;

use super::types::ContractEvidence;
use super::verification::ManagedContract;
use super::{session, storage};
use crate::commands::deploy::DeployContext;

pub async fn check(ctx: &DeployContext) -> Result<()> {
    let session = session::current()?;
    let expected = session.journal.lock().await.evidence.clone();
    let pending = crate::state::next_pending_step(&ctx.state).map(|(_, step)| step.name.as_str());
    let journaled = if let Some(pending) = pending {
        session
            .journal
            .lock()
            .await
            .actions
            .keys()
            .any(|key| key.starts_with(&format!("{pending}/")))
    } else {
        false
    };
    let observed = snapshot(ctx).await?;
    for (name, saved) in expected {
        let observed = observed
            .get(&name)
            .ok_or_else(|| eyre::eyre!("{name} missing from deployment configuration"))?;
        let action = if journaled { pending } else { None };
        eyre::ensure!(
            compatible(
                &saved,
                observed,
                action.filter(|step| affected_contract(step) == Some(name.as_str()))
            ),
            "{name}: on-chain state differs from the verified checkpoint; expected {saved:?}, observed {observed:?}"
        );
    }
    Ok(())
}

async fn snapshot(ctx: &DeployContext) -> Result<BTreeMap<String, ContractEvidence>> {
    let root: Value = serde_json::from_slice(&tokio::fs::read(&ctx.target_json).await?)?;
    let mut evidence = BTreeMap::new();
    if let Some(contracts) = root["chains"][ctx.axelar_id.as_str()]["contracts"].as_object() {
        for (name, config) in contracts {
            if let Some(address) = config["address"].as_str() {
                evidence.insert(name.clone(), observe(ctx, name, address.parse()?).await?);
            }
            if let Some(address) = config["implementation"].as_str() {
                evidence.insert(
                    format!("{name}Implementation"),
                    observe(ctx, "implementation", address.parse()?).await?,
                );
            }
        }
    }
    // Include every ITS helper, even though it is not a top-level config entry.
    for step in &ctx.state.steps {
        for helper in [
            "TokenManagerDeployer",
            "InterchainToken",
            "InterchainTokenDeployer",
            "TokenManager",
            "TokenHandler",
        ] {
            if let Some(address) = step.its_address(helper) {
                evidence.insert(helper.into(), observe(ctx, helper, address).await?);
            }
        }
    }
    Ok(evidence)
}

pub async fn capture(ctx: &DeployContext) -> Result<()> {
    let evidence = snapshot(ctx).await?;
    let session = session::current()?;
    let mut journal = session.journal.lock().await;
    let pending = crate::state::next_pending_step(&ctx.state).map(|(_, step)| step.name.as_str());
    for (name, previous) in &journal.evidence {
        let observed = evidence
            .get(name)
            .ok_or_else(|| eyre::eyre!("{name} missing from deployment configuration"))?;
        eyre::ensure!(
            compatible(
                previous,
                observed,
                pending.filter(|step| affected_contract(step) == Some(name.as_str()))
            ),
            "{name} changed outside the current action"
        );
    }
    journal.evidence = evidence;
    storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)
}

async fn observe(ctx: &DeployContext, name: &str, address: Address) -> Result<ContractEvidence> {
    let provider = ProviderBuilder::new().connect_http(ctx.rpc_url.parse()?);
    let code = provider.get_code_at(address).await?;
    eyre::ensure!(!code.is_empty(), "{name} has no code at {address}");
    let contract = ManagedContract::new(address, &provider);
    let owned = matches!(
        name,
        "Operators"
            | "AxelarGateway"
            | "AxelarGasService"
            | "InterchainTokenService"
            | "InterchainTokenFactory"
    );
    let proxy = owned && name != "Operators";
    let gateway = name == "AxelarGateway";
    Ok(ContractEvidence {
        address,
        code_hash: keccak256(code),
        owner: if owned {
            Some(contract.owner().call().await?)
        } else {
            None
        },
        implementation: if proxy {
            Some(contract.implementation().call().await?)
        } else {
            None
        },
        operator: if gateway {
            Some(contract.operator().call().await?)
        } else {
            None
        },
        signer_hash: if gateway {
            Some(
                contract
                    .signersHashByEpoch(alloy::primitives::U256::from(1))
                    .call()
                    .await?,
            )
        } else {
            None
        },
    })
}

pub(super) fn affected_contract(step: &str) -> Option<&'static str> {
    match step {
        "TransferGatewayOwnership" => Some("AxelarGateway"),
        "TransferOperatorsOwnership" => Some("Operators"),
        "TransferGasServiceOwnership" => Some("AxelarGasService"),
        _ => None,
    }
}

pub(super) fn compatible(
    saved: &ContractEvidence,
    observed: &ContractEvidence,
    action: Option<&str>,
) -> bool {
    let mut expected = saved.clone();
    if let Some(
        "TransferOperatorsOwnership" | "TransferGatewayOwnership" | "TransferGasServiceOwnership",
    ) = action
    {
        expected.owner = observed.owner;
    }
    expected == *observed
}
