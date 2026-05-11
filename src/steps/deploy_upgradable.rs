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
use crate::evm::{LegacyProxy, read_artifact_bytecode};
use crate::state::{Step, save_state};
use crate::ui;
use crate::utils::{read_contract_address, update_target_json};

pub async fn run(
    ctx: &mut DeployContext,
    step_idx: usize,
    step: &Step,
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
    let gas_collector = read_contract_address(&ctx.target_json, &ctx.axelar_id, "Operators")?;
    ui::address("gas collector (Operators)", &format!("{gas_collector}"));

    // --- Tx 1: Deploy implementation (skip if already deployed) ---
    let impl_addr = if let Some(addr) = step.implementation_address() {
        let code = provider.get_code_at(addr).await?;
        if code.is_empty() {
            return Err(eyre::eyre!(
                "saved implementation {addr} has no code on-chain"
            ));
        }
        ui::info(&format!(
            "reusing previously deployed implementation: {addr}"
        ));
        addr
    } else {
        ui::info("deploying AxelarGasService implementation...");
        let impl_bytecode = read_artifact_bytecode(impl_artifact)?;
        let mut impl_deploy_code = impl_bytecode.clone();
        impl_deploy_code.extend_from_slice(&gas_collector.abi_encode());

        let tx = TransactionRequest::default().with_deploy_code(Bytes::from(impl_deploy_code));
        let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
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

        // Save to state so retries skip re-deployment
        ctx.state.steps[step_idx].set_implementation_address(addr);
        save_state(&ctx.state)?;
        addr
    };

    // --- Tx 2: Deploy proxy (skip if already deployed) ---
    let proxy_addr = if let Some(addr) = step.proxy_address() {
        let code = provider.get_code_at(addr).await?;
        if code.is_empty() {
            return Err(eyre::eyre!("saved proxy {addr} has no code on-chain"));
        }
        ui::info(&format!("reusing previously deployed proxy: {addr}"));
        addr
    } else {
        ui::info("deploying AxelarGasServiceProxy...");
        let proxy_bytecode = read_artifact_bytecode(proxy_artifact)?;

        let tx = TransactionRequest::default().with_deploy_code(Bytes::from(proxy_bytecode));
        let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
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

        // Save to state so retries skip re-deployment
        ctx.state.steps[step_idx].set_proxy_address(addr);
        save_state(&ctx.state)?;
        addr
    };

    // --- Tx 3: Call proxy.init(implementation, owner, setupParams) ---
    let eip1967_impl_slot: U256 =
        "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc".parse()?;
    let impl_slot = provider
        .get_storage_at(proxy_addr, eip1967_impl_slot)
        .await?;

    if impl_slot != U256::ZERO {
        let stored_impl = alloy::primitives::Address::from_word(impl_slot.into());
        ui::info(&format!(
            "proxy already initialized with implementation: {stored_impl}"
        ));
    } else {
        ui::info(&format!(
            "calling proxy.init({impl_addr}, {deployer_addr}, 0x)..."
        ));
        let proxy = LegacyProxy::new(proxy_addr, &provider);
        let init_tx = proxy.init(impl_addr, deployer_addr, Bytes::new());

        let receipt = init_tx.send().await?.get_receipt().await?;
        ui::tx_hash("init tx hash", &format!("{}", receipt.transaction_hash));

        if !receipt.status() {
            return Err(eyre::eyre!(
                "proxy init tx {} reverted on-chain",
                receipt.transaction_hash
            ));
        }
        ui::success("proxy initialized successfully");
    }

    // --- Write to target JSON ---
    let mut contract_data = serde_json::Map::new();
    contract_data.insert("address".into(), json!(format!("{proxy_addr}")));
    contract_data.insert("implementation".into(), json!(format!("{impl_addr}")));
    contract_data.insert("deployer".into(), json!(format!("{deployer_addr}")));
    contract_data.insert("deploymentMethod".into(), json!("create"));
    contract_data.insert("collector".into(), json!(format!("{gas_collector}")));

    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        step_name,
        Value::Object(contract_data),
    )?;

    Ok(())
}
