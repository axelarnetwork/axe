use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use eyre::{Result, ensure};
use serde_json::json;

use super::config::{self, GOVERNANCE_ADDRESS, GOVERNANCE_CHAIN};
use super::types::{Context, Deployment, Options, Settings, TransactionAction};
use super::{checks, existing, factory, transactions};
use crate::evm::{Create3Deployer, get_salt_from_key};
use crate::ui;

pub fn validate_inputs(settings: &Settings, from: Address, salt: Option<&str>) -> Result<()> {
    ensure!(
        from == settings.deployer,
        "ASG deployment requires --from {} and ASG_DEPLOYER_PRIVATE_KEY for that account, got {from}",
        settings.deployer
    );
    ensure!(
        salt.is_none_or(|salt| salt == settings.salt),
        "ASG deployment requires salt '{}' to preserve the network's deployment recipe",
        settings.salt
    );
    Ok(())
}

pub(super) async fn prepare<P: Provider>(
    provider: &P,
    ctx: &Context,
    from: Address,
) -> Result<Deployment> {
    let deployer = factory::verify(provider, ctx, from).await?;
    let artifact = checks::artifact(ctx)?;
    let init = checks::creation(ctx, &artifact);
    let runtime = checks::expected_runtime(provider, init.clone(), from).await?;
    let salt_bytes = get_salt_from_key(ctx.settings.salt);
    let address = Create3Deployer::new(deployer, provider)
        .deployedAddress(init.clone(), from, salt_bytes)
        .call()
        .await?;
    validate_prediction(ctx, address)?;
    let input = Create3Deployer::deployCall {
        bytecode: init,
        salt: get_salt_from_key(ctx.settings.salt),
    }
    .abi_encode();
    let request = TransactionRequest::default()
        .from(from)
        .to(deployer)
        .with_chain_id(ctx.chain_id)
        .value(U256::ZERO)
        .input(input.into());
    Ok(Deployment {
        address,
        request,
        runtime,
        artifact,
    })
}

pub(super) fn validate_prediction(ctx: &Context, predicted: Address) -> Result<()> {
    ensure!(!predicted.is_zero(), "factory predicted a zero ASG address");
    if let Some(configured) = ctx.configured_asg {
        ensure!(
            configured == predicted,
            "configured ASG {configured} differs from predicted {predicted}, refusing a replacement"
        );
    }
    Ok(())
}

pub async fn run<P: Provider>(provider: &P, ctx: &mut Context, options: &Options) -> Result<()> {
    ui::address(
        "required ASG deployer",
        &ctx.settings.deployer.to_checksum(None),
    );
    ui::kv("deployment signing key", super::signing::DEPLOYER_KEY_ENV);
    ui::kv("salt key", ctx.settings.salt);
    if let Some(asg) = existing::address(provider, ctx).await? {
        return existing::reuse(provider, ctx, options, asg).await;
    }
    ui::address(
        "configured CREATE3 factory",
        &factory::selected(ctx)?.to_checksum(None),
    );
    ui::info(
        "ASG addresses are specific to the factory, network deployer and salt. The predicted address below will be recorded for this chain",
    );
    let deployment = prepare(provider, ctx, options.from).await?;
    checks::report(ctx, deployment.address);
    ui::address(
        "predicted ASG address",
        &deployment.address.to_checksum(None),
    );
    ui::kv("method", "CREATE3");
    ui::kv(
        "creation bytecode hash",
        &keccak256(&deployment.artifact.bytecode).to_string(),
    );
    ui::kv(
        "expected runtime hash",
        &keccak256(&deployment.runtime).to_string(),
    );
    ui::info("Full CREATE3 deploy(bytes,bytes32) payload, including ASG constructor arguments:");
    transactions::preview(&deployment.request)?;
    let exists = !provider.get_code_at(deployment.address).await?.is_empty();
    let recover = transactions::journal_path(&deployment.request)?.exists();
    if exists && (!options.apply || !recover) {
        checks::governance(provider, ctx, deployment.address, &deployment.runtime).await?;
        ui::success("ASG already deployed and verified, no deployment transaction needed");
    }
    if !options.apply {
        ui::info("Preview only. Use --apply to deploy or record verified existing state");
        return Ok(());
    }
    let receipt = if exists && !recover {
        ensure!(
            ui::confirm("Record this verified ASG in the deployments config?").await,
            "declined"
        );
        None
    } else {
        Some(
            transactions::send(
                provider,
                ctx,
                &deployment.request,
                TransactionAction::Deploy(deployment.address),
            )
            .await?,
        )
    };
    checks::governance(provider, ctx, deployment.address, &deployment.runtime).await?;
    let mut record = json!({
        "address": deployment.address.to_checksum(None),
        "gateway": ctx.gateway.to_checksum(None),
        "governanceChain": GOVERNANCE_CHAIN,
        "governanceAddress": GOVERNANCE_ADDRESS,
        "minimumTimeDelay": ctx.settings.delay,
        "operator": ctx.settings.operator.to_checksum(None),
        "deployer": options.from.to_checksum(None),
        "deploymentMethod": "create3", "salt": ctx.settings.salt,
        "create3Deployer": factory::selected(ctx)?.to_checksum(None),
        "predeployCodehash": keccak256(&deployment.artifact.deployed_bytecode),
        "codehash": keccak256(&deployment.runtime),
    });
    if let Some(receipt) = receipt {
        record["deploymentTx"] = json!(receipt.transaction_hash);
    }
    config::save(ctx, &[("AxelarServiceGovernance", record)])
}
