//! Legacy (consensus) Axelar EVM gateway verification helpers.
//!
//! Consensus chains (e.g. Avalanche) are secured by the EVM-verifier + nexus
//! flow, not Amplifier, so there is no `VotingVerifier`/`MultisigProver` to
//! poll. The destination message status lives entirely on the legacy
//! `AxelarGateway`:
//!
//! - approval emits `ContractCallApproved(commandId, …, payloadHash, sourceTxHash, …)`
//! - execution emits `ContractCallExecuted(commandId)` and flips
//!   `isCommandExecuted(commandId)` to true.
//!
//! We locate the approval by reading the **emitted event** (matched by the
//! indexed `payloadHash` plus the exact `sourceTxHash`) and take the on-chain
//! `commandId` from it — guaranteed correct regardless of derivation edge
//! cases. [`derive_command_id`] reproduces axelar-core's offline derivation
//! purely as a logged cross-check.

use alloy::primitives::{Address, FixedBytes, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use eyre::{Result, eyre};

use crate::evm::ContractCallApproved;

/// Parse an EVM source message id (`{0x-tx-hash}-{event_index}`) into its
/// `(tx_hash, event_index)` parts. This is the `message_id` stored on a
/// `PendingTx` for an EVM source (`SourceChainType::Evm`).
pub(super) fn parse_evm_message_id(message_id: &str) -> Result<(FixedBytes<32>, u64)> {
    let (hash_str, idx_str) = message_id
        .rsplit_once('-')
        .ok_or_else(|| eyre!("malformed EVM message id (no '-{{index}}' suffix): {message_id}"))?;
    let hash_hex = hash_str.strip_prefix("0x").unwrap_or(hash_str);
    let bytes =
        hex::decode(hash_hex).map_err(|e| eyre!("bad tx hash in message id {message_id}: {e}"))?;
    if bytes.len() != 32 {
        return Err(eyre!(
            "tx hash in message id {message_id} is {} bytes, expected 32",
            bytes.len()
        ));
    }
    let event_index = idx_str
        .parse::<u64>()
        .map_err(|e| eyre!("bad event index in message id {message_id}: {e}"))?;
    Ok((FixedBytes::<32>::from_slice(&bytes), event_index))
}

/// Reproduce axelar-core's consensus-gateway `commandId` derivation
/// (`x/evm/types`): `keccak256(sourceTxHash ++ u64_LE(eventIndex) ++
/// chainId_big_endian_minimal)[:32]`. Kept as a cross-check against the
/// on-chain event — not the source of truth.
pub(super) fn derive_command_id(
    source_tx_hash: FixedBytes<32>,
    event_index: u64,
    dest_chain_id: u64,
) -> [u8; 32] {
    let mut data = Vec::with_capacity(32 + 8 + 8);
    data.extend_from_slice(source_tx_hash.as_slice());
    data.extend_from_slice(&event_index.to_le_bytes());
    data.extend_from_slice(&chain_id_be_minimal(dest_chain_id));
    keccak256(&data).into()
}

/// Big-endian minimal-byte encoding of a chain id, matching Go's
/// `big.Int.Bytes()` (no leading zero bytes; empty for 0).
fn chain_id_be_minimal(chain_id: u64) -> Vec<u8> {
    let be = chain_id.to_be_bytes();
    let first = be.iter().position(|&b| b != 0).unwrap_or(be.len());
    be[first..].to_vec()
}

/// Scan the destination legacy gateway for the `ContractCallApproved` matching
/// this message and return the on-chain `commandId`. Filters by the indexed
/// `payloadHash`, then pins the match with `contractAddress` + the exact
/// `sourceTxHash`. Returns `None` until the approval is observed.
pub(super) async fn find_contract_call_approved<P: Provider>(
    provider: &P,
    gateway: Address,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
    source_tx_hash: FixedBytes<32>,
    from_block: u64,
) -> Result<Option<[u8; 32]>> {
    let filter = Filter::new()
        .address(gateway)
        .event_signature(ContractCallApproved::SIGNATURE_HASH)
        .topic3(payload_hash)
        .from_block(from_block);
    let logs = provider.get_logs(&filter).await?;
    for log in logs {
        let Ok(decoded) = ContractCallApproved::decode_log(&log.inner) else {
            continue;
        };
        if decoded.data.contractAddress == contract_addr
            && decoded.data.sourceTxHash == source_tx_hash
        {
            return Ok(Some(decoded.data.commandId.into()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_evm_message_id_splits_hash_and_index() {
        let (hash, idx) = parse_evm_message_id(
            "0x3e49bc399a15eb36a555cf207980155753b7154471918a87f8bdad01a4df01ad-3",
        )
        .unwrap();
        assert_eq!(idx, 3);
        assert_eq!(
            hex::encode(hash),
            "3e49bc399a15eb36a555cf207980155753b7154471918a87f8bdad01a4df01ad"
        );
    }

    #[test]
    fn chain_id_be_minimal_strips_leading_zeros() {
        assert_eq!(chain_id_be_minimal(43113), vec![0xa8, 0x69]); // 43113 = 0xA869
        assert_eq!(chain_id_be_minimal(1), vec![1]);
        assert_eq!(chain_id_be_minimal(0), Vec::<u8>::new());
    }

    #[test]
    fn derive_command_id_is_deterministic() {
        let (hash, idx) = parse_evm_message_id(
            "0x3e49bc399a15eb36a555cf207980155753b7154471918a87f8bdad01a4df01ad-3",
        )
        .unwrap();
        let a = derive_command_id(hash, idx, 43113);
        let b = derive_command_id(hash, idx, 43113);
        assert_eq!(a, b);
        // Different destination chain id ⇒ different commandId.
        assert_ne!(a, derive_command_id(hash, idx, 11155111));
    }
}
