use alloy::{
    primitives::{Address, B256, Bytes, keccak256},
    providers::Provider,
    rpc::types::TransactionRequest,
};
use eyre::Result;
use solana_axelar_std::{CrossChainId, Message};

use crate::evm::{AxelarAmplifierGateway, SenderReceiver};
use crate::solana::{
    approve_messages_on_gateway, decode_execute_data, execute_on_memo, load_keypair,
};
use crate::ui;

/// Submit the Amplifier-built `execute_data` to the EVM gateway, confirm the
/// message is approved, then call `execute` on the SenderReceiver and read
/// back the stored message. `source_address` is the original sender's
/// address (a SenderReceiver address for the EVM-loopback flow, a Solana
/// keypair pubkey for sol→evm) — the EVM gateway validates the approval
/// against this value, so it must match what was emitted at the source.
#[allow(clippy::too_many_arguments)]
pub async fn approve_and_execute_evm<P: Provider>(
    provider: &P,
    gateway: Address,
    sender_receiver: Address,
    source_chain: &str,
    source_address: &str,
    message_id: &str,
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
    let _approve_receipt = crate::evm::broadcast_and_log(pending_approve, "tx").await?;

    // Derive commandId from (source_chain, message_id) — the canonical
    // Amplifier formula (`keccak256(source_chain || "-" || message_id)`,
    // matching `solana_axelar_std::Message::command_id()`). Reading it
    // back from the approve receipt's `ContractCallApproved`/`MessageApproved`
    // event is fragile across legacy vs modern gateway variants; deriving
    // works regardless of which event the gateway happens to emit.
    let command_id_input = {
        let mut buf = Vec::with_capacity(source_chain.len() + 1 + message_id.len());
        buf.extend_from_slice(source_chain.as_bytes());
        buf.push(b'-');
        buf.extend_from_slice(message_id.as_bytes());
        buf
    };
    let command_id = keccak256(&command_id_input);
    ui::kv("commandId", &format!("{command_id}"));

    let gw_contract = AxelarAmplifierGateway::new(gateway, provider);
    let approved = gw_contract
        .isContractCallApproved(
            command_id,
            source_chain.to_string(),
            source_address.to_string(),
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
        source_address.to_string(),
        Bytes::from(payload_bytes.to_vec()),
    );
    let pending_exec = exec_call.send().await?;
    let _exec_receipt = crate::evm::broadcast_and_log(pending_exec, "tx").await?;

    let stored_message = sr_contract.message().call().await?;
    ui::kv("stored message", &format!("\"{stored_message}\""));

    Ok(())
}

/// Submit the Amplifier-built `execute_data` to the Solana gateway, then
/// call the destination program (memo) with the decoded GMP message and
/// raw payload. Wraps Steps 7-8 of an SVM-destination flow.
#[allow(clippy::too_many_arguments)]
pub fn approve_and_execute_svm(
    dst_rpc: &str,
    source_chain: &str,
    destination_chain: &str,
    source_address: &str,
    destination_address: &str,
    message_id: &str,
    payload_bytes: &[u8],
    payload_hash: B256,
    execute_data_hex: &str,
    step_idx_approve: usize,
    step_idx_execute: usize,
    total_steps: usize,
) -> Result<()> {
    ui::step_header(step_idx_approve, total_steps, "Approve on Solana gateway");
    let keypair = load_keypair(None)?;
    let execute_data = decode_execute_data(execute_data_hex)?;
    approve_messages_on_gateway(dst_rpc, &keypair, &execute_data)?;

    ui::step_header(step_idx_execute, total_steps, "Execute on destination");
    let gmp_message = Message {
        cc_id: CrossChainId {
            chain: source_chain.to_string(),
            id: message_id.to_string(),
        },
        source_address: source_address.to_string(),
        destination_chain: destination_chain.to_string(),
        destination_address: destination_address.to_string(),
        payload_hash: payload_hash.0,
    };

    let memo_sig = execute_on_memo(dst_rpc, &keypair, gmp_message, payload_bytes)?;
    ui::tx_hash("execute", &memo_sig.to_string());

    Ok(())
}
