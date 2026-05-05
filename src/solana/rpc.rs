//! Low-level RPC plumbing: finalized-commitment client construction and the
//! retry loop that hides RPC lag past `send_and_confirm`. Higher-level flows
//! in `gateway` and `its` reach for `fetch_tx_details` to enrich `TxMetrics`
//! with compute-units / slot, and for `fetch_finalized_tx` to read logs +
//! inner instructions for message-id and event extraction.
//!
//! We use `finalized` (not `confirmed`) commitment everywhere because Axelar
//! verifiers and indexers read at finalized — anything we report as
//! "delivered" must already be rollback-proof from their perspective, or we
//! race verifier voting and produce split polls.

use eyre::Result;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::UiTransactionEncoding;

/// Solana System Program ID — `11111111111111111111111111111111`. Named here
/// so callers don't sprinkle the base58 literal across the codebase.
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

/// Compute-unit limit applied via ComputeBudget instructions before our own
/// gateway/ITS calls. 400k matches the published per-tx cap and gives our
/// `verify_signature` + `approve_message` step plenty of headroom.
pub(super) const DEFAULT_CU_LIMIT: u32 = 400_000;

/// Max attempts when polling for a finalized Solana transaction (see
/// `fetch_finalized_tx`). 15 attempts × exponential backoff capped at 5s
/// per attempt totals ~60s — comfortably more than mainnet's ~12-20s
/// confirmed→finalized window.
const SOL_TX_FETCH_MAX_ATTEMPTS: u32 = 15;

/// Construct an `RpcClient` with the canonical "finalized" commitment level.
/// All Solana reads/writes flow through this — `send_and_confirm` blocks
/// until the tx is finalized (rollback-proof) so downstream Axelar verifier
/// queries see consistent state.
pub fn rpc_client(rpc_url: &str) -> RpcClient {
    RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized())
}

pub(super) fn fetch_tx_details(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<(Option<u64>, Option<u64>)> {
    let tx = fetch_finalized_tx(rpc_client, signature)?;
    match tx {
        Some(tx) => {
            let slot = Some(tx.slot);
            let compute_units = tx
                .transaction
                .meta
                .and_then(|m| Option::from(m.compute_units_consumed));
            Ok((compute_units, slot))
        }
        None => Ok((None, None)),
    }
}

/// Fetch a finalized transaction with retries.
///
/// Used both for best-effort metadata enrichment (compute units, slot — fine
/// to return None) and for the message-id extraction path which actually
/// requires the logs. RPC providers can lag a few seconds past
/// `send_and_confirm`, so we retry generously to cover the
/// confirmed→finalized window.
pub(super) fn fetch_finalized_tx(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<Option<solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta>> {
    for i in 0..SOL_TX_FETCH_MAX_ATTEMPTS {
        match rpc_client.get_transaction(signature, UiTransactionEncoding::Json) {
            Ok(tx) => return Ok(Some(tx)),
            Err(_) => {
                // Exponential backoff: 500ms, 1s, 2s, capped at 5s. Total ~60s.
                let delay = std::cmp::min(500 * (1 << i), 5000);
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
        }
    }
    Ok(None)
}
