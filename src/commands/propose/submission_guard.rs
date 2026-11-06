//! An interrupted broadcast must not become a second submission while the LCD catches up.
use std::path::{Path, PathBuf};

use alloy::primitives::keccak256;
use eyre::{Result, WrapErr};

use super::types::ProposalPlan;

/// One marker per distinct proposal: the hub route, the command, and every
/// edge call it carries. Two plans with the same calls in any order share it.
pub fn path(plan: &ProposalPlan) -> Result<PathBuf> {
    let hub = plan.hub();
    let mut calls: Vec<_> = plan
        .calls
        .iter()
        .map(|call| {
            (
                call.cfg.edge_axelar_id.clone(),
                call.cfg.asg_address.to_lowercase(),
                call.target,
                call.calldata.clone(),
            )
        })
        .collect();
    calls.sort();
    let operation = serde_json::to_vec(&(
        &hub.chain_id,
        &hub.axelarnet_gateway,
        &hub.gov_module,
        plan.ptype.command(),
        calls,
    ))?;
    Ok(crate::state::data_dir()?
        .join("proposals")
        .join(format!("{:x}.pending", keccak256(operation))))
}

pub async fn check(path: &Path, new_proposal: bool) -> Result<()> {
    eyre::ensure!(
        new_proposal || !tokio::fs::try_exists(path).await?,
        "an earlier submission of this call may still be pending, but it is not in LCD history yet; refusing a duplicate. Wait and rerun. Only use --new-proposal after verifying the earlier submission failed (marker: {})",
        path.display()
    );
    Ok(())
}

/// Keep the marker even on error: losing a response does not prove a failed broadcast.
/// It contains no keys, mnemonic or signed transaction. Confirmed proposals are recovered
/// from on-chain history before this marker is consulted.
pub async fn claim(path: &Path, new_proposal: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre::eyre!("invalid proposal marker path"))?;
    tokio::fs::create_dir_all(parent).await?;
    if new_proposal {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .wrap_err("submission already started for this call; refusing a duplicate")?;
    file.sync_all().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::propose::test_support::{config, plan};
    use crate::commands::propose::types::ProposalType;
    use alloy::primitives::Address;

    #[tokio::test]
    async fn interrupted_or_concurrent_submission_cannot_be_claimed_twice() {
        let path = std::env::temp_dir().join(format!(
            "axe-proposal-guard-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        check(&path, false).await.unwrap();
        claim(&path, false).await.unwrap();
        assert!(check(&path, false).await.is_err());
        assert!(claim(&path, false).await.is_err());
        // A deliberately new operation is explicit, rather than an automatic retry.
        check(&path, true).await.unwrap();
        claim(&path, true).await.unwrap();
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[test]
    fn marker_depends_on_every_call_but_not_their_order() {
        let call = |edge: &str, byte: u8| {
            let mut cfg = config(String::new());
            cfg.edge_axelar_id = edge.into();
            plan(
                cfg,
                ProposalType::Operator,
                Address::from([byte; 20]),
                vec![byte].into(),
            )
            .calls
            .remove(0)
        };
        let plan_of = |calls| ProposalPlan::new(ProposalType::Operator, "test".into(), calls);
        let one = plan_of(vec![call("a", 1)]).unwrap();
        let ab = plan_of(vec![call("a", 1), call("b", 2)]).unwrap();
        let ba = plan_of(vec![call("b", 2), call("a", 1)]).unwrap();
        let changed = plan_of(vec![call("a", 1), call("b", 9)]).unwrap();
        assert_ne!(path(&one).unwrap(), path(&ab).unwrap());
        assert_eq!(path(&ab).unwrap(), path(&ba).unwrap());
        assert_ne!(path(&ab).unwrap(), path(&changed).unwrap());
    }
}
