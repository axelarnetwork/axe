use alloy::{
    primitives::{Bytes, U256, keccak256},
    rpc::types::TransactionReceipt,
    sol_types::SolValue,
};
use eyre::Result;

use super::super::{
    encoding::encode_receive_from_hub,
    relay::{DestinationRelayRequest, HubRelayRequest, relay_to_destination, relay_to_hub},
};
use super::types::Context;
use crate::commands::event_extractors::extract_contract_call_event;
use crate::types::ChainAxelarId;
use crate::ui;

/// Preserve the actual emitted inner payload, rather than reconstructing token metadata or amounts.
pub(super) fn destination_payload(
    payload: &[u8],
    source: &ChainAxelarId,
    destination: &ChainAxelarId,
) -> Result<Vec<u8>> {
    let (kind, chain, inner) = <(U256, String, Bytes)>::abi_decode_params(payload)?;
    eyre::ensure!(kind == U256::from(3), "expected ITS SendToHub payload");
    eyre::ensure!(
        chain == destination.as_str(),
        "ITS payload targets a different destination chain"
    );
    Ok(encode_receive_from_hub(source, &inner))
}

pub(super) async fn relay(ctx: &Context, receipt: &TransactionReceipt) -> Result<u64> {
    let (index, payload, hash, destination, hub) = extract_contract_call_event(receipt)?;
    eyre::ensure!(
        destination == crate::types::HubChain::NAME && hub == ctx.route.its_hub,
        "source ITS event does not target the configured hub"
    );
    eyre::ensure!(
        keccak256(&payload) == hash,
        "source event payload hash mismatch"
    );
    let forwarded = destination_payload(
        &payload,
        &ctx.route.source_axelar_id,
        &ctx.route.destination_axelar_id,
    )?;
    let message_id = format!("{:#x}-{index}", receipt.transaction_hash);
    ui::kv("source message", &message_id);
    crate::evm::finality::wait_for_finalized_receipt(&ctx.source, receipt).await?;
    ui::section("Manual relay: source → Axelar hub");
    relay_to_hub(HubRelayRequest {
        axelar_id: ctx.route.source_axelar_id.as_str(),
        message_id: &message_id,
        source_address: &ctx.source_its.to_string(),
        destination_chain: &destination,
        destination_address: &hub,
        payload_hash: &hash,
        payload: &payload,
        tx: ctx.relay.tx(),
        cosm_gateway: &ctx.relay.cosm_gateway,
        voting_verifier: &ctx.relay.voting_verifier,
        axelarnet_gateway: &ctx.relay.axelarnet_gateway,
    })
    .await?;
    ui::section("Manual relay: Axelar hub → destination EVM");
    relay_to_destination(DestinationRelayRequest {
        first_leg_message_id: &message_id,
        dest_payload: &forwarded,
        dst_its_proxy: ctx.route.destination_its,
        dst_evm_gateway: ctx.route.destination_gateway,
        dst_provider: &ctx.destination,
        tx: ctx.relay.tx(),
        dst_cosm_gateway: &ctx.route.destination_cosm_gateway,
        dst_multisig_prover: &ctx.route.destination_multisig_prover,
        axelar_rpc: &ctx.axelar_rpc,
        step_base: 1,
        step_total: 6,
    })
    .await
}
