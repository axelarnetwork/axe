use crate::commands::deploy::hardened::evm as deployment_evm;
use alloy::{
    network::TransactionBuilder,
    primitives::{Bytes, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::config::ChainContract;
use crate::evm::{LegacyProxy, read_artifact_bytecode};
use crate::state::save_state;
use crate::ui;
use crate::utils::{read_contract_address, update_target_json};

async fn write_contract_config(
    ctx: &DeployContext,
    step_name: &str,
    proxy_addr: alloy::primitives::Address,
    impl_addr: alloy::primitives::Address,
    deployer_addr: alloy::primitives::Address,
    gas_collector: alloy::primitives::Address,
) -> Result<()> {
    let mut data = serde_json::Map::new();
    data.insert("address".into(), json!(format!("{proxy_addr}")));
    data.insert("implementation".into(), json!(format!("{impl_addr}")));
    data.insert("deployer".into(), json!(format!("{deployer_addr}")));
    data.insert("deploymentMethod".into(), json!("create"));
    data.insert("collector".into(), json!(format!("{gas_collector}")));
    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        step_name,
        Value::Object(data),
    )
    .await
}

async fn initialize_proxy<P: Provider>(
    provider: &P,
    proxy_addr: alloy::primitives::Address,
    impl_addr: alloy::primitives::Address,
    owner: alloy::primitives::Address,
) -> Result<()> {
    let implementation_slot: U256 =
        "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc".parse()?;
    let stored = provider
        .get_storage_at(proxy_addr, implementation_slot)
        .await?;
    if stored != U256::ZERO {
        let stored_impl = alloy::primitives::Address::from_word(stored.into());
        eyre::ensure!(
            stored_impl == impl_addr,
            "proxy has a conflicting implementation"
        );
        ui::info(&format!(
            "proxy already initialized with implementation: {stored_impl}"
        ));
        return Ok(());
    }
    ui::info(&format!("calling proxy.init({impl_addr}, {owner}, 0x)..."));
    let request = LegacyProxy::new(proxy_addr, provider)
        .init(impl_addr, owner, Bytes::new())
        .into_transaction_request();
    let receipt = deployment_evm::send(provider, request, "gas proxy initialization").await?;
    ui::tx_hash("init tx hash", &format!("{}", receipt.transaction_hash));
    if !receipt.status() {
        return Err(eyre::eyre!(
            "proxy init tx {} reverted on-chain",
            receipt.transaction_hash
        ));
    }
    ui::success("proxy initialized successfully");
    Ok(())
}

pub async fn run(
    ctx: &mut DeployContext,
    step_idx: usize,
    step_name: &str,
    private_key: &str,
    impl_artifact: &str,
    proxy_artifact: &str,
) -> Result<()> {
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_addr = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(ctx.rpc_url.parse()?);

    // Read the gas collector address (= Operators contract)
    let gas_collector =
        read_contract_address(&ctx.target_json, &ctx.axelar_id, ChainContract::Operators).await?;
    ui::address("gas collector (Operators)", &format!("{gas_collector}"));

    // --- Tx 1: Deploy implementation (recover from the journal if already deployed) ---
    let impl_addr = {
        ui::info("deploying AxelarGasService implementation...");
        let impl_bytecode = read_artifact_bytecode(impl_artifact).await?;
        let mut impl_deploy_code = impl_bytecode.clone();
        impl_deploy_code.extend_from_slice(&gas_collector.abi_encode());

        let tx = TransactionRequest::default().with_deploy_code(Bytes::from(impl_deploy_code));
        let receipt = deployment_evm::send(&provider, tx, "gas implementation").await?;
        ui::tx_hash(
            "implementation tx hash",
            &format!("{}", receipt.transaction_hash),
        );

        if !receipt.status() {
            return Err(eyre::eyre!(
                "implementation deployment tx {} reverted on-chain",
                receipt.transaction_hash
            ));
        }

        let addr = receipt
            .contract_address
            .ok_or_else(|| eyre::eyre!("no contract address in implementation receipt"))?;
        ui::address("implementation deployed at", &format!("{addr}"));

        // Save to state for status. The journal controls transaction recovery
        ctx.state.steps[step_idx].set_implementation_address(addr)?;
        save_state(&ctx.state).await?;
        addr
    };

    // --- Tx 2: Deploy proxy (recover from the journal if already deployed) ---
    let proxy_addr = {
        ui::info("deploying AxelarGasServiceProxy...");
        let proxy_bytecode = read_artifact_bytecode(proxy_artifact).await?;

        let tx = TransactionRequest::default().with_deploy_code(Bytes::from(proxy_bytecode));
        let receipt = deployment_evm::send(&provider, tx, "gas proxy").await?;
        ui::tx_hash("proxy tx hash", &format!("{}", receipt.transaction_hash));

        if !receipt.status() {
            return Err(eyre::eyre!(
                "proxy deployment tx {} reverted on-chain",
                receipt.transaction_hash
            ));
        }

        let addr = receipt
            .contract_address
            .ok_or_else(|| eyre::eyre!("no contract address in proxy receipt"))?;
        ui::address("proxy deployed at", &format!("{addr}"));

        // Save to state for status. The journal controls transaction recovery
        ctx.state.steps[step_idx].set_proxy_address(addr)?;
        save_state(&ctx.state).await?;
        addr
    };

    initialize_proxy(&provider, proxy_addr, impl_addr, deployer_addr).await?;

    write_contract_config(
        ctx,
        step_name,
        proxy_addr,
        impl_addr,
        deployer_addr,
        gas_collector,
    )
    .await
}
