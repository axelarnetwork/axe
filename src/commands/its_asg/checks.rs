use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolValue;
use eyre::{Result, WrapErr, ensure};

use super::config::{GOVERNANCE_ADDRESS, GOVERNANCE_CHAIN};
use super::contracts::{Governance, Managed};
use super::signing::{OPERATOR_KEY_ENV, SENDER_KEY_ENV};
use super::types::{Artifact, Context, Roles};
use crate::evm::AxelarServiceGovernance;
use crate::ui;

pub async fn network<P: Provider>(provider: &P, ctx: &Context) -> Result<()> {
    ensure!(
        provider.get_chain_id().await? == ctx.chain_id,
        "EVM RPC chain ID mismatch"
    );
    for address in [ctx.gateway, ctx.its] {
        ensure!(
            !provider.get_code_at(address).await?.is_empty(),
            "no contract at {address}"
        );
    }
    let its_gateway = Managed::new(ctx.its, provider).gateway().call().await?;
    ensure!(
        its_gateway == ctx.gateway,
        "ITS points to a different gateway"
    );
    Managed::new(ctx.gateway, provider)
        .operator()
        .call()
        .await
        .wrap_err("cannot read amplifier gateway operator")?;
    Ok(())
}

pub fn artifact(ctx: &Context) -> Result<Artifact> {
    let artifact: Artifact = serde_json::from_slice(&std::fs::read(&ctx.artifact_path)?)?;
    ensure!(
        artifact.contract_name == "AxelarServiceGovernance",
        "wrong ASG artifact contractName"
    );
    ensure!(
        !artifact.bytecode.is_empty() && !artifact.deployed_bytecode.is_empty(),
        "ASG artifact has empty bytecode"
    );
    Ok(artifact)
}

pub fn creation(ctx: &Context, artifact: &Artifact) -> Bytes {
    let args = (
        ctx.gateway,
        GOVERNANCE_CHAIN.to_string(),
        GOVERNANCE_ADDRESS.to_string(),
        U256::from(ctx.settings.delay),
        ctx.settings.operator,
    )
        .abi_encode_params();
    [artifact.bytecode.as_ref(), args.as_slice()]
        .concat()
        .into()
}

pub async fn expected_runtime<P: Provider>(
    provider: &P,
    init: Bytes,
    from: Address,
) -> Result<Bytes> {
    // eth_call of contract creation returns runtime with constructor immutables filled.
    let runtime = provider
        .call(TransactionRequest::default().from(from).input(init.into()))
        .await
        .wrap_err("cannot simulate contract constructor to verify runtime")?;
    ensure!(
        !runtime.is_empty(),
        "contract constructor returned empty runtime"
    );
    Ok(runtime)
}

pub async fn governance<P: Provider>(
    provider: &P,
    ctx: &Context,
    address: Address,
    runtime: &Bytes,
) -> Result<()> {
    let code = provider
        .get_code_at(address)
        .block_id(BlockId::finalized())
        .await?;
    ensure!(
        code == *runtime,
        "ASG runtime mismatch at {address}, use the matching deployment artifact"
    );
    let asg = AxelarServiceGovernance::new(address, provider);
    let wiring = Governance::new(address, provider);
    let block = BlockId::finalized();
    ensure!(
        wiring.gateway().block(block).call().await? == ctx.gateway,
        "ASG gateway mismatch"
    );
    ensure!(
        asg.operator().block(block).call().await? == ctx.settings.operator,
        "ASG operator differs from network policy"
    );
    ensure!(
        asg.operator().call().await? == ctx.settings.operator,
        "ASG operator changed in the latest state"
    );
    ensure!(
        asg.minimumTimeLockDelay().block(block).call().await? == U256::from(ctx.settings.delay),
        "ASG timelock differs from network policy"
    );
    ensure!(
        asg.governanceChain().block(block).call().await? == GOVERNANCE_CHAIN,
        "ASG governance chain mismatch"
    );
    ensure!(
        asg.governanceAddress().block(block).call().await? == GOVERNANCE_ADDRESS,
        "ASG governance address mismatch"
    );
    ensure!(
        wiring.governanceChainHash().block(block).call().await? == keccak256(GOVERNANCE_CHAIN),
        "ASG governance chain hash mismatch"
    );
    ensure!(
        wiring.governanceAddressHash().block(block).call().await? == keccak256(GOVERNANCE_ADDRESS),
        "ASG governance address hash mismatch"
    );
    Ok(())
}

pub async fn roles<P: Provider>(
    provider: &P,
    ctx: &Context,
    from: Address,
    asg: Address,
) -> Result<Roles> {
    let gw = Managed::new(ctx.gateway, provider);
    let its = Managed::new(ctx.its, provider);
    let block = BlockId::finalized();
    Ok(Roles {
        gateway_owner: gw.owner().block(block).call().await?,
        gateway_operator: gw.operator().block(block).call().await?,
        its_owner: its.owner().block(block).call().await?,
        sender_is_its_operator: its.isOperator(from).block(block).call().await?,
        asg_is_its_operator: its.isOperator(asg).block(block).call().await?,
    })
}

pub fn report(ctx: &Context, asg: Address) {
    ui::section(&format!(
        "Service governance: {} on {}",
        ctx.key, ctx.network
    ));
    ui::kv("EVM chain ID", &ctx.chain_id.to_string());
    ui::address("ASG", &asg.to_checksum(None));
    ui::address("gateway", &ctx.gateway.to_checksum(None));
    ui::address("ITS", &ctx.its.to_checksum(None));
    ui::address("ASG operator", &ctx.settings.operator.to_checksum(None));
    ui::kv("transaction signing key (--from)", SENDER_KEY_ENV);
    ui::kv("required ASG operator key (--apply)", OPERATOR_KEY_ENV);
    ui::info(
        "ASG is controlled by Axelar governance. Its operator executes governance-approved calls",
    );
    ui::kv("governanceChain", GOVERNANCE_CHAIN);
    ui::kv("governanceAddress", GOVERNANCE_ADDRESS);
    ui::kv(
        "minimumTimeLockDelay (seconds)",
        &ctx.settings.delay.to_string(),
    );
    ui::kv("ASG artifact", &ctx.artifact_path.display().to_string());
}
