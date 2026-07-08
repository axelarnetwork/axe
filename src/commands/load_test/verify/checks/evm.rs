use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use eyre::Result;

use crate::evm::AxelarAmplifierGateway;

/// Check `isMessageApproved` on the EVM gateway (single attempt).
pub(in super::super) async fn check_evm_is_message_approved<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    source_chain: &str,
    message_id: &str,
    source_address: &str,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
) -> Result<bool> {
    let approved = gw_contract
        .isMessageApproved(
            source_chain.to_string(),
            message_id.to_string(),
            source_address.to_string(),
            contract_addr,
            payload_hash,
        )
        .call()
        .await?;
    Ok(approved)
}

/// Check `isMessageExecuted` on the EVM amplifier gateway (single attempt).
/// True once the approved message has been consumed by the destination
/// contract. Unlike `isMessageApproved` — which flips back to false the instant
/// the message executes — this stays true, so it catches an approval that was
/// approved *and* executed between two polls (the fast-route race that left
/// monad-3 / hyperliquid stuck in the Approved phase until the inactivity
/// timeout).
pub(in super::super) async fn check_evm_is_message_executed<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let executed = gw_contract
        .isMessageExecuted(source_chain.to_string(), message_id.to_string())
        .call()
        .await?;
    Ok(executed)
}

/// Check `isCommandExecuted` on a legacy consensus gateway (single attempt).
/// True once the destination contract has consumed the approval command.
pub(in super::super) async fn check_evm_command_executed<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    command_id: FixedBytes<32>,
) -> Result<bool> {
    let executed = gw_contract.isCommandExecuted(command_id).call().await?;
    Ok(executed)
}
