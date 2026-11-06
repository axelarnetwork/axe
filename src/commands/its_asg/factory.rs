use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::Provider;
use eyre::{Result, ensure};

use super::types::{Context, RecordedFactory};
use super::{checks, rpc};
use crate::config::ChainConfig;
use crate::evm::read_artifact_bytecode;
use crate::utils;

pub(super) fn configured(cfg: &ChainConfig) -> Result<Option<Address>> {
    let Some(contracts) = &cfg.contracts else {
        return Ok(None);
    };
    let recorded = contracts
        .get("AxelarServiceGovernance")
        .map(|record| {
            serde_json::from_value::<RecordedFactory>(serde_json::to_value(&record.extra)?)
        })
        .transpose()?
        .unwrap_or_default()
        .create3_deployer;
    let selected = match recorded {
        Some(address) => Some(address),
        None => contracts
            .get("Create3Deployer")
            .and_then(|c| c.address.as_deref())
            .map(str::parse)
            .transpose()?,
    };
    ensure!(
        selected.is_none_or(|address| !address.is_zero()),
        "configured CREATE3 factory is zero"
    );
    Ok(selected)
}

pub(super) fn selected(ctx: &Context) -> Result<Address> {
    ctx.create3.ok_or_else(|| eyre::eyre!("no CREATE3 factory configured for {}. Complete the chain's normal factory deployment and record Create3Deployer.address first", ctx.key))
}

pub(super) async fn verify<P: Provider>(
    provider: &P,
    ctx: &Context,
    from: Address,
) -> Result<Address> {
    let factory = selected(ctx)?;
    let latest = provider.get_code_at(factory).await?;
    ensure!(
        !latest.is_empty(),
        "configured CREATE3 factory {factory} is not deployed on {}. Complete the chain's normal factory deployment first",
        ctx.key
    );
    let root = utils::deployments_root(&ctx.path)?;
    let (path, _) = utils::artifact_paths_for_step("Create3Deployer", &root)
        .ok_or_else(|| eyre::eyre!("missing Create3Deployer artifact path"))?;
    let expected =
        checks::expected_runtime(provider, read_artifact_bytecode(&path).await?.into(), from)
            .await?;
    ensure!(
        latest == expected,
        "CREATE3 factory runtime does not match the checkout artifact"
    );
    ensure!(
        rpc::finalized_read("finalized CREATE3 factory runtime", || async {
            Ok(provider
                .get_code_at(factory)
                .block_id(BlockId::finalized())
                .await?)
        })
        .await?
            == expected,
        "finalized CREATE3 factory runtime does not match the checkout artifact"
    );
    Ok(factory)
}
