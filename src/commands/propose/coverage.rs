//! Per-call claims prevent resubmission across different proposal batches.
use std::fs::File;
use std::path::PathBuf;

use alloy::primitives::keccak256;
use eyre::{Result, WrapErr};

use super::types::ProposalPlan;
use crate::commands::deploy::hardened::storage;

pub(super) fn lock(plan: &ProposalPlan) -> Result<File> {
    lock_network(&plan.hub().chain_id)
}

fn lock_network(chain: &str) -> Result<File> {
    let key = keccak256(chain.as_bytes());
    storage::lock(
        &crate::state::data_dir()?
            .join("locks")
            .join(format!("governance-{key}.lock")),
    )
}

pub(super) fn paths(plan: &ProposalPlan) -> Result<Vec<PathBuf>> {
    plan.calls
        .iter()
        .map(|call| {
            let key = keccak256(serde_json::to_vec(&(
                &call.cfg.chain_id,
                &call.cfg.axelarnet_gateway,
                &call.cfg.gov_module,
                &call.cfg.edge_axelar_id,
                call.cfg.asg_address.to_lowercase(),
                call.target,
                &call.calldata,
            ))?);
            Ok(crate::state::data_dir()?
                .join("proposals/calls")
                .join(format!("{key}.pending")))
        })
        .collect()
}

pub(super) async fn claim(plan: &ProposalPlan, replacement: bool) -> Result<()> {
    check(plan, replacement).await?;
    for path in paths(plan)? {
        super::submission_guard::claim(&path, replacement).await?;
    }
    Ok(())
}

pub(super) async fn check(plan: &ProposalPlan, replacement: bool) -> Result<()> {
    for path in paths(plan)? {
        super::submission_guard::check(&path, replacement).await.wrap_err_with(|| {
            let hash = std::fs::read_to_string(&path).ok().filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).unwrap_or_else(|| "unknown".into());
            format!("unresolved per-call claim at {} (signed transaction hash {hash}). Check its transaction and proposal history first. If the original action provably was never submitted, explicitly review propose --new-proposal; saved claims are never discarded automatically", path.display())
        })?;
    }
    Ok(())
}
