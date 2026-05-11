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
