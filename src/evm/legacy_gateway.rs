//! Destination-side state on the legacy consensus `AxelarGateway`.
//!
//! Consensus chains (e.g. Avalanche) are secured by the EVM-verifier + nexus
//! flow, not Amplifier, so there is no `VotingVerifier`/`MultisigProver` to
//! poll. The destination message status lives entirely on the gateway:
//!
//! - approval emits `ContractCallApproved(commandId, ..., payloadHash, sourceTxHash, ...)`
//! - execution emits `ContractCallExecuted(commandId)` and flips
//!   `isCommandExecuted(commandId)` to true.
//!
//! The emitted `ContractCallApproved` is located by its indexed `payloadHash`
//! and the on-chain `commandId` is taken from it, guaranteed correct, and
//! confirmed to match the canonical GMP-API value.

use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use eyre::Result;

use super::ContractCallApproved;
use crate::retry::retry_all;

/// Max blocks per `eth_getLogs` window. Public RPCs cap the range (polygon Amoy
/// rejects > ~128 blocks). We scan newest to oldest in windows of this size so a
/// fresh approval is found in the first request and we never submit a range an
/// RPC will reject. Kept well under common caps.
pub const LOG_WINDOW_BLOCKS: u64 = 100;

/// A `ContractCallApproved` the gateway emitted for a destination contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedCall {
    pub command_id: B256,
    pub source_chain: String,
    pub source_address: String,
}

/// Scan the gateway for the `ContractCallApproved` matching `payload_hash`
/// (indexed) and `contract`, optionally pinned to the exact `source_tx_hash`.
/// Returns `None` until the approval is observed.
pub async fn find_contract_call_approved<P: Provider>(
    provider: &P,
    gateway: Address,
    contract: Address,
    payload_hash: B256,
    source_tx_hash: Option<B256>,
    from_block: u64,
) -> Result<Option<ApprovedCall>> {
    // Retried: these run against the destination RPC every poll cycle, and a
    // transient blip must not surface as a scan failure (the callers treat a
    // failed scan as "still pending" (see the tolerant poll loops), so an
    // unretried error would silently delay detection by a full cycle).
    let latest = retry_all("legacy get_block_number", || provider.get_block_number()).await?;
    let mut to = latest;
    while to >= from_block {
        let from = to.saturating_sub(LOG_WINDOW_BLOCKS - 1).max(from_block);
        let filter = Filter::new()
            .address(gateway)
            .event_signature(ContractCallApproved::SIGNATURE_HASH)
            .topic3(payload_hash)
            .from_block(from)
            .to_block(to);
        let logs = retry_all("legacy get_logs", || provider.get_logs(&filter)).await?;
        for log in logs {
            let Ok(decoded) = ContractCallApproved::decode_log(&log.inner) else {
                continue;
            };
            if decoded.data.contractAddress == contract
                && source_tx_hash.is_none_or(|h| decoded.data.sourceTxHash == h)
            {
                return Ok(Some(ApprovedCall {
                    command_id: decoded.data.commandId,
                    source_chain: decoded.data.sourceChain,
                    source_address: decoded.data.sourceAddress,
                }));
            }
        }
        if from == from_block {
            break;
        }
        to = from - 1;
    }
    Ok(None)
}
