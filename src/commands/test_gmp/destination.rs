use alloy::{
    primitives::{Address, B256, Bytes},
    providers::Provider,
    rpc::types::TransactionRequest,
};
use eyre::Result;

use crate::evm::{AxelarAmplifierGateway, SenderReceiver};
use crate::ui;

/// Submit the Amplifier-built `execute_data` to the EVM gateway, confirm the
/// message is approved, then call `execute` on the SenderReceiver and read
/// back the stored message. Wraps Steps 7-8 of the EVM-loopback flow.
#[allow(clippy::too_many_arguments)]
pub async fn approve_and_execute_evm<P: Provider>(
    provider: &P,
    gateway: Address,
    sender_receiver: Address,
    source_chain: &str,
    execute_data_hex: &str,
    payload_bytes: &[u8],
    payload_hash: B256,
    step_idx_approve: usize,
    step_idx_execute: usize,
    total_steps: usize,
) -> Result<()> {
    ui::step_header(step_idx_approve, total_steps, "Submit proof to EVM gateway");
    let execute_data = alloy::hex::decode(execute_data_hex)?;

    let approve_tx = TransactionRequest::default()
        .to(gateway)
        .input(Bytes::from(execute_data).into());
    let pending_approve = provider.send_transaction(approve_tx).await?;
    let approve_receipt = crate::evm::broadcast_and_log(pending_approve, "tx").await?;

    let command_id = approve_receipt
        .inner
        .logs()
        .iter()
        .find_map(|log| {
            if log.topics().len() >= 2 && log.address() == gateway {
                Some(log.topics()[1])
            } else {
                None
            }
        })
        .ok_or_else(|| eyre::eyre!("commandId not found in approve tx logs"))?;
    ui::kv("commandId", &format!("{command_id}"));

    let gw_contract = AxelarAmplifierGateway::new(gateway, provider);
    let approved = gw_contract
        .isContractCallApproved(
            command_id,
            source_chain.to_string(),
            format!("{sender_receiver}"),
            sender_receiver,
            payload_hash,
        )
        .call()
        .await?;
    ui::kv("isContractCallApproved", &format!("{approved}"));

    if !approved {
        return Err(eyre::eyre!("message not approved on EVM gateway"));
    }

    ui::step_header(step_idx_execute, total_steps, "Execute on SenderReceiver");
    let sr_contract = SenderReceiver::new(sender_receiver, provider);
    let exec_call = sr_contract.execute(
        command_id,
        source_chain.to_string(),
        format!("{sender_receiver}"),
        Bytes::from(payload_bytes.to_vec()),
    );
    let pending_exec = exec_call.send().await?;
    let _exec_receipt = crate::evm::broadcast_and_log(pending_exec, "tx").await?;

    let stored_message = sr_contract.message().call().await?;
    ui::kv("stored message", &format!("\"{stored_message}\""));

    Ok(())
}
