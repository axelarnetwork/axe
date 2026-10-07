use crate::commands::deploy::hardened::evm as deployment_evm;
use alloy::{
    hex,
    network::TransactionBuilder,
    primitives::{B256, Bytes, U256, keccak256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::evm::{encode_gateway_setup_params, read_artifact_bytecode};
use crate::state::save_state;
use crate::types::Network;
use crate::ui;
use crate::utils::{compute_domain_separator, update_target_json};

#[cfg(test)]
mod tests;

struct GatewayDeploymentRecord {
    proxy: alloy::primitives::Address,
    implementation: alloy::primitives::Address,
    deployer: alloy::primitives::Address,
    implementation_codehash: alloy::primitives::B256,
    domain_separator: alloy::primitives::B256,
    verifier_set_id: String,
}

async fn write_gateway_config(ctx: &DeployContext, record: &GatewayDeploymentRecord) -> Result<()> {
    let mut data = serde_json::Map::new();
    data.insert("address".into(), json!(format!("{}", record.proxy)));
    data.insert(
        "implementation".into(),
        json!(format!("{}", record.implementation)),
    );
    data.insert("deployer".into(), json!(format!("{}", record.deployer)));
    data.insert("deploymentMethod".into(), json!("create"));
    data.insert(
        "implementationCodehash".into(),
        json!(format!("{}", record.implementation_codehash)),
    );
    data.insert("previousSignersRetention".into(), json!(15));
    data.insert(
        "domainSeparator".into(),
        json!(format!("{}", record.domain_separator)),
    );
    data.insert(
        "minimumRotationDelay".into(),
        json!(ctx.state.env.gateway_rotation_delay_seconds()),
    );
    data.insert(
        "operator".into(),
        json!(
            ctx.state
                .hardened_plan
                .as_ref()
                .ok_or_else(|| eyre::eyre!("missing deployment plan"))?
                .gateway_operator
        ),
    );
    data.insert("owner".into(), json!(format!("{}", record.deployer)));
    data.insert("connectionType".into(), json!("amplifier"));
    data.insert("initialVerifierSetId".into(), json!(record.verifier_set_id));
    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        "AxelarGateway",
        Value::Object(data),
    )
    .await
}

async fn deploy_gateway_proxy<P: Provider>(
    provider: &P,
    implementation: alloy::primitives::Address,
    owner: alloy::primitives::Address,
    setup_params: &Bytes,
    proxy_artifact: &str,
    nonce: Option<u64>,
) -> Result<alloy::primitives::Address> {
    ui::info("deploying AxelarAmplifierGatewayProxy...");
    let mut deploy_code = read_artifact_bytecode(proxy_artifact).await?;
    deploy_code
        .extend_from_slice(&(implementation, owner, setup_params.clone()).abi_encode_params());
    let mut tx = TransactionRequest::default()
        .with_deploy_code(Bytes::from(deploy_code))
        .with_gas_limit(5_000_000);
    tx.nonce = nonce;

    let receipt = deployment_evm::send(provider, tx, "gateway proxy").await?;
    ui::tx_hash("proxy tx hash", &format!("{}", receipt.transaction_hash));
    if !receipt.status() {
        return Err(eyre::eyre!(
            "proxy deployment tx {} reverted on-chain (status=0)",
            receipt.transaction_hash
        ));
    }
    let address = receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("no contract address in proxy receipt"))?;
    ui::address("proxy deployed at", &format!("{address}"));
    Ok(address)
}

pub async fn run(
    ctx: &mut DeployContext,
    step_idx: usize,
    private_key: &str,
    impl_artifact: &str,
    proxy_artifact: &str,
) -> Result<()> {
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_addr = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(ctx.rpc_url.parse()?);

    let domain_separator = compute_domain_separator(&ctx.target_json, &ctx.axelar_id).await?;
    let deployment_nonce = crate::commands::deploy::hardened::verification::gateway_nonce(
        ctx,
        &provider,
        deployer_addr,
    )
    .await?;

    // --- Tx 1: Deploy implementation (recover from the journal if already deployed) ---
    let (impl_addr, impl_codehash) = {
        ui::info("deploying AxelarAmplifierGateway implementation...");
        let code =
            gateway_implementation_code(impl_artifact, domain_separator, ctx.state.env).await?;
        let mut tx = TransactionRequest::default().with_deploy_code(code);
        tx.nonce = deployment_nonce;
        let receipt = deployment_evm::send(&provider, tx, "gateway implementation").await?;
        ui::tx_hash(
            "implementation tx hash",
            &format!("{}", receipt.transaction_hash),
        );
        let addr = receipt
            .contract_address
            .ok_or_else(|| eyre::eyre!("no contract address in implementation receipt"))?;
        ui::address("implementation deployed at", &format!("{addr}"));

        let code = provider.get_code_at(addr).await?;
        let codehash = keccak256(&code);

        // Save implementation address for status. The journal controls transaction recovery
        ctx.state.steps[step_idx].set_implementation_address(addr)?;
        save_state(&ctx.state).await?;

        (addr, codehash)
    };

    // Recheck the prover before building a new proxy transaction. Signed attempts stay pinned.
    crate::commands::deploy::hardened::verification::refresh_before_gateway(ctx).await?;

    // --- Fetch verifier set from Axelar chain ---
    let (signers, threshold, nonce, verifier_set_id) = {
        let initial = crate::commands::deploy::hardened::verification::initial_set(ctx).await?;
        (
            initial.signers,
            initial.threshold,
            initial.nonce,
            initial.set_id,
        )
    };

    // --- Encode setup params ---
    let operator = ctx
        .state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing deployment plan"))?
        .gateway_operator;
    let owner = deployer_addr;
    let setup_params = encode_gateway_setup_params(operator, &signers, threshold, nonce);
    ui::kv(
        "setup params",
        &format!(
            "{} bytes: 0x{}",
            setup_params.len(),
            hex::encode(&setup_params)
        ),
    );

    let proxy_addr = deploy_gateway_proxy(
        &provider,
        impl_addr,
        owner,
        &setup_params,
        proxy_artifact,
        deployment_nonce.map(|nonce| nonce + 1),
    )
    .await?;

    eyre::ensure!(
        Some(proxy_addr) == ctx.state.predicted_gateway_address,
        "gateway address differs from the pinned Cosmos configuration"
    );

    write_gateway_config(
        ctx,
        &GatewayDeploymentRecord {
            proxy: proxy_addr,
            implementation: impl_addr,
            deployer: deployer_addr,
            implementation_codehash: impl_codehash,
            domain_separator,
            verifier_set_id,
        },
    )
    .await
}

async fn gateway_implementation_code(
    artifact: &str,
    domain_separator: B256,
    network: Network,
) -> Result<Bytes> {
    let constructor = (
        U256::from(15),
        domain_separator,
        U256::from(network.gateway_rotation_delay_seconds()),
    );
    let mut code = read_artifact_bytecode(artifact).await?;
    code.extend_from_slice(&constructor.abi_encode());
    Ok(Bytes::from(code))
}
