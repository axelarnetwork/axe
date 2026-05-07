//! `RpcClient` constructor + retry helpers for fetching confirmed/finalized
//! transactions.
//!
//! All write paths in `axe` use `CommitmentConfig::finalized()`; the
//! constructor keeps that invariant in one place so callers don't sprinkle
//! `RpcClient::new_with_commitment(_, finalized())` literals across the
//! codebase. `fetch_confirmed_tx` lives here because it owns the retry
//! schedule that's tuned to the public devnet RPC's eventual-consistency
//! window between `confirmed` and `getTransaction` indexing.

use eyre::Result;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::signature::Signature;
use solana_transaction_status::UiTransactionEncoding;

/// Construct an `RpcClient` with finalized commitment — single helper so
/// callers don't sprinkle `RpcClient::new_with_commitment(_, finalized())`
/// across the codebase.
pub fn rpc_client(rpc_url: &str) -> RpcClient {
    RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized())
}

/// Fetch tx slot + compute units consumed for an already-confirmed signature.
pub(super) fn fetch_tx_details(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<(Option<u64>, Option<u64>)> {
    let tx = fetch_confirmed_tx(rpc_client, signature)?;
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

/// Fetch a confirmed transaction with retries.
///
/// Public Solana devnet RPC (api.devnet.solana.com) often takes 30+ seconds
/// to index a freshly-confirmed transaction so it's queryable via
/// getTransaction. Use a generous retry budget (~60s wall-clock) before
/// giving up, since the alternative — guessing the message_id — costs the
/// caller a full 5-minute pipeline timeout downstream.
pub(super) fn fetch_confirmed_tx(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<Option<solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta>> {
    // Slight upfront delay — `send_and_confirm_transaction` only guarantees
    // the tx is in `confirmed`, not that it's been backfilled into the
    // history endpoint queried by `getTransaction`.
    std::thread::sleep(std::time::Duration::from_millis(750));
    for i in 0..15 {
        match rpc_client.get_transaction(signature, UiTransactionEncoding::Json) {
            Ok(tx) => return Ok(Some(tx)),
            Err(_) => {
                // Exponential backoff capped at 5s: 500ms, 1s, 2s, 4s, 5s, 5s, …
                let delay = std::cmp::min(500u64 * (1 << i), 5000);
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
        }
    }
    Ok(None)
}
