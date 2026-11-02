use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use eyre::{Result, ensure};
use serde_json::json;

use super::config::{self, GOVERNANCE_ADDRESS, GOVERNANCE_CHAIN};
use super::types::{Context, Deployment, Options};
use super::{checks, transactions};
use crate::evm::{Create3Deployer, get_salt_from_key, read_artifact_bytecode};
use crate::{ui, utils};

async fn prepare<P: Provider>(
    provider: &P,
    ctx: &Context,
    from: Address,
    salt: &str,
) -> Result<Deployment> {
    ensure!(!salt.trim().is_empty(), "salt cannot be empty");
    let deployer = ctx
        .create3
        .ok_or_else(|| eyre::eyre!("Create3Deployer missing from config"))?;
    let root = utils::deployments_root(&ctx.path)?;
    let (artifact_path, _) = utils::artifact_paths_for_step("Create3Deployer", &root)
        .ok_or_else(|| eyre::eyre!("missing Create3Deployer artifact path"))?;
    let code = provider
        .get_code_at(deployer)
        .block_id(BlockId::finalized())
        .await?;
    let expected = checks::expected_runtime(
        provider,
        read_artifact_bytecode(&artifact_path).await?.into(),
        from,
    )
    .await?;
    ensure!(
        code == expected,
        "Create3Deployer runtime does not match the checkout artifact"
    );
    let artifact = checks::artifact(ctx)?;
    let init = checks::creation(ctx, &artifact);
    let runtime = checks::expected_runtime(provider, init.clone(), from).await?;
    let salt_bytes = get_salt_from_key(salt);
    let address = Create3Deployer::new(deployer, provider)
        .deployedAddress(init.clone(), from, salt_bytes)
        .call()
        .await?;
    if let Some(configured) = ctx.configured_asg {
        ensure!(
            configured == address,
            "configured ASG {configured} differs from predicted {address}, refusing a replacement"
        );
    }
    let input = Create3Deployer::deployCall {
        bytecode: init,
        salt: salt_bytes,
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

pub async fn run<P: Provider>(
    provider: &P,
    ctx: &mut Context,
    options: &Options,
    salt: &str,
) -> Result<()> {
    let deployment = prepare(provider, ctx, options.from, salt).await?;
    checks::report(ctx, deployment.address);
    ui::kv("method", "CREATE3");
    ui::kv("salt key", salt);
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
    if exists {
        checks::governance(provider, ctx, deployment.address, &deployment.runtime).await?;
        ui::success("ASG already deployed and verified, no deployment transaction needed");
    }
    if !options.apply {
        ui::info("Preview only. Use --apply to deploy or record verified existing state");
        return Ok(());
    }
    let receipt = if exists {
        ensure!(
            ui::confirm("Record this verified ASG in the deployments config?").await,
            "declined"
        );
        None
    } else {
        Some(transactions::send(provider, ctx, &deployment.request, None).await?)
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
        "deploymentMethod": "create3", "salt": salt,
        "predeployCodehash": keccak256(&deployment.artifact.deployed_bytecode),
        "codehash": keccak256(&deployment.runtime),
    });
    if let Some(receipt) = receipt {
        record["deploymentTx"] = json!(receipt.transaction_hash);
    }
    config::save(ctx, &[("AxelarServiceGovernance", record)])
}
