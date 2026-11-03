use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use eyre::{Result, ensure};
use serde_json::json;

use super::checks;
use super::config::{self, GOVERNANCE_ADDRESS, GOVERNANCE_CHAIN};
use super::types::{Context, Options};
use crate::ui;

pub(super) async fn address<P: Provider>(provider: &P, ctx: &Context) -> Result<Option<Address>> {
    if let Some(configured) = ctx.configured_asg
        && !provider.get_code_at(configured).await?.is_empty()
    {
        return Ok(Some(configured));
    }
    let legacy = ctx.settings.legacy_asg;
    if !provider.get_code_at(legacy).await?.is_empty() {
        ensure!(
            ctx.configured_asg
                .is_none_or(|configured| configured == legacy),
            "an existing shared ASG {legacy} conflicts with the configured ASG. Reconcile the config before deploying"
        );
        return Ok(Some(legacy));
    }
    Ok(None)
}

pub(super) async fn reuse<P: Provider>(
    provider: &P,
    ctx: &mut Context,
    options: &Options,
    asg: Address,
) -> Result<()> {
    let artifact = checks::artifact(ctx)?;
    let runtime =
        checks::expected_runtime(provider, checks::creation(ctx, &artifact), options.from).await?;
    checks::governance(provider, ctx, asg, &runtime).await?;
    checks::report(ctx, asg);
    ui::success("Existing ASG verified, no deployment transaction needed");
    // Only observed contract properties are updated, never inferred deployment provenance.
    let record = json!({
        "address": asg.to_checksum(None), "gateway": ctx.gateway.to_checksum(None),
        "governanceChain": GOVERNANCE_CHAIN, "governanceAddress": GOVERNANCE_ADDRESS,
        "minimumTimeDelay": ctx.settings.delay, "operator": ctx.settings.operator.to_checksum(None),
        "codehash": keccak256(&runtime),
    });
    ui::info("Verified ASG config fields:");
    ui::info(&serde_json::to_string_pretty(&record)?);
    if !options.apply {
        ui::info("Preview only. Use --apply to record verified existing state");
        return Ok(());
    }
    ensure!(
        ui::confirm("Record this verified ASG in the deployments config?").await,
        "declined"
    );
    checks::network(provider, ctx).await?;
    checks::governance(provider, ctx, asg, &runtime).await?;
    config::save(ctx, &[("AxelarServiceGovernance", record)])
}
