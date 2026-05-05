//! Low-level RPC plumbing: confirmed-commitment client construction and the
//! retry loop that hides RPC lag past `send_and_confirm`. Higher-level
//! flows in `gateway` and `its` reach for `fetch_tx_details` to enrich
//! `TxMetrics` with compute-units / slot, and for `fetch_confirmed_tx` to
//! read logs + inner instructions for message-id and event extraction.
//!
//! Default commitment is `confirmed` — it's nearly-as-safe as finalized in
//! practice and shaves ~12-20s per send by returning before the finality
//! root. Flows that hand a Solana state proof off to a system reading at
//! finalized (notably the Amplifier verifier set, which votes on `verify_messages`)
//! must call [`wait_for_signature_finalized`] before doing so, otherwise
//! they race the verifiers and produce split polls.

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

/// Max attempts when polling for a confirmed Solana transaction (see
/// `fetch_confirmed_tx`). 15 attempts × exponential backoff capped at 5s
/// per attempt totals ~60s of wait, enough to cover RPC lag past
/// `send_and_confirm`.
const SOL_TX_FETCH_MAX_ATTEMPTS: u32 = 15;

/// Max attempts when polling for finality (see [`wait_for_signature_finalized`]).
/// 60 × 1s = 60s, comfortably above mainnet's typical 12-20s
/// confirmed→finalized window.
const SOL_FINALIZE_MAX_ATTEMPTS: u32 = 60;

/// Construct an `RpcClient` with the canonical "confirmed" commitment level.
/// `send_and_confirm` returns once the tx hits supermajority vote (~1-2s),
/// not after the finality root. Callers that need rollback-proof state
/// before signalling the cosmos hub must additionally call
/// [`wait_for_signature_finalized`].
pub fn rpc_client(rpc_url: &str) -> RpcClient {
    RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed())
}

/// Block until the Solana transaction with `signature` is finalized
/// (rollback-proof). Polls `getSignatureStatuses` at finalized commitment
/// once a second up to a 60s cap. Returns `Ok(())` as soon as the status
/// reports finalized + successful.
///
/// Use this before submitting `verify_messages` to the Axelar hub — the
/// verifier set reads Solana state at finalized commitment, so anything
/// shorter races them and produces split poll votes.
pub fn wait_for_signature_finalized(rpc_url: &str, signature: &Signature) -> Result<()> {
    let client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    for attempt in 0..SOL_FINALIZE_MAX_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        if let Some(Ok(())) =
            client.get_signature_status_with_commitment(signature, CommitmentConfig::finalized())?
        {
            return Ok(());
        }
    }
    Err(eyre::eyre!(
        "tx {signature} did not finalize within {SOL_FINALIZE_MAX_ATTEMPTS}s"
    ))
}

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
/// Used both for best-effort metadata enrichment (compute units, slot — fine
/// to return None) and for the message-id extraction path which actually
/// requires the logs. RPC providers can lag a few seconds past
/// `send_and_confirm`, so we retry generously.
pub(super) fn fetch_confirmed_tx(
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
