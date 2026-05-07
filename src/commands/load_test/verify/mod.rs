use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, FixedBytes, keccak256};
use alloy::providers::Provider;
use eyre::Result;
use futures::StreamExt;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

use super::metrics::{AmplifierTiming, PeakThroughput, TxMetrics, VerificationReport};
use crate::cosmos::{
    lcd_cosmwasm_smart_query, read_axelar_config, read_axelar_contract_field, read_axelar_rpc,
    rpc_tx_search_event,
};
use crate::evm::AxelarAmplifierGateway;
use crate::solana::solana_call_contract_index;
use crate::ui;

/// If no transaction completes a phase for this long, we stop waiting.
/// Resets every time a tx makes progress, so large batches naturally get more time.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(120);
/// Delay between poll attempts.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Interval for recalculating rolling throughput.
const THROUGHPUT_WINDOW: Duration = Duration::from_secs(10);

mod report;
mod state;

use self::report::{compute_peak_throughput, compute_verification_report};
use self::state::{ApprovalResult, Phase, RealTimeStats, phase_counts};

// Re-export `PendingTx` to the parent `load_test` module so the per-pair
// runners can receive it back from the `tx_to_pending_*` constructors and
// forward it through their verifier mpsc channels.
pub(in crate::commands::load_test) use self::state::PendingTx;

/// Parse a hex-encoded 32-byte payload hash, with or without the `0x`
/// prefix. Returns an error rather than silently zero-extending so a
/// truncated hash from upstream code surfaces immediately instead of
/// propagating into a downstream "wrong gateway hash" mismatch.
fn parse_payload_hash(hex_str: &str) -> Result<FixedBytes<32>> {
    let bytes = alloy::hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?;
    if bytes.len() != 32 {
        return Err(eyre::eyre!(
            "payload_hash must be 32 bytes, got {}",
            bytes.len()
        ));
    }
    Ok(FixedBytes::from_slice(&bytes))
}

// ---------------------------------------------------------------------------
// Destination checker abstraction
// ---------------------------------------------------------------------------

enum DestinationChecker<'a, P: Provider> {
    Evm {
        gw_contract: &'a AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&'a P>,
    },
    Solana {
        rpc_client: Arc<solana_client::rpc_client::RpcClient>,
        _phantom: std::marker::PhantomData<&'a P>,
    },
    Stellar {
        client: crate::stellar::StellarClient,
        gateway_contract: String,
        signer_pk: [u8; 32],
        _phantom: std::marker::PhantomData<&'a P>,
    },
    /// Sui destination — query AxelarGateway events `MessageApproved`
    /// (Approved phase) and `MessageExecuted` (Executed phase) by
    /// `(source_chain, message_id)`.
    Sui {
        client: crate::sui::SuiClient,
        gateway_pkg: String,
        _phantom: std::marker::PhantomData<&'a P>,
    },
}

#[allow(dead_code)]
impl<P: Provider> DestinationChecker<'_, P> {
    async fn check_approved(
        &self,
        tx: &PendingTx,
        _idx: usize,
        source_chain: &str,
    ) -> Result<ApprovalResult> {
        match self {
            Self::Evm { gw_contract } => {
                let approved = check_evm_is_message_approved(
                    gw_contract,
                    source_chain,
                    &tx.message_id,
                    &tx.source_address,
                    tx.contract_addr,
                    tx.payload_hash,
                )
                .await?;
                if approved {
                    Ok(ApprovalResult::Approved)
                } else {
                    // tx is already routed, so false = already executed
                    Ok(ApprovalResult::AlreadyExecuted)
                }
            }
            Self::Solana { rpc_client, .. } => {
                let client = rpc_client.clone();
                let cmd_id = tx.command_id.unwrap_or_default();
                let result = tokio::task::spawn_blocking(move || {
                    check_solana_incoming_message(&client, &cmd_id)
                })
                .await??;
                match result {
                    Some(0) => Ok(ApprovalResult::Approved),
                    Some(_) => Ok(ApprovalResult::AlreadyExecuted),
                    None => Ok(ApprovalResult::NotYet),
                }
            }
            Self::Stellar {
                client,
                gateway_contract,
                signer_pk,
                ..
            } => {
                let approved = client
                    .gateway_is_message_approved(
                        signer_pk,
                        gateway_contract,
                        source_chain,
                        &tx.message_id,
                        &tx.source_address,
                        // For GMP, contract_address is the destination C-address;
                        // we stash that in `payload_hash_hex` slot? No — use a
                        // dedicated field. PendingTx.gmp_destination_address
                        // holds the destination contract for non-ITS flows.
                        &tx.gmp_destination_address,
                        tx.payload_hash.0,
                    )
                    .await
                    .unwrap_or(None);
                match approved {
                    Some(true) => Ok(ApprovalResult::Approved),
                    Some(false) => Ok(ApprovalResult::AlreadyExecuted),
                    None => Ok(ApprovalResult::NotYet),
                }
            }
            Self::Sui {
                client,
                gateway_pkg,
                ..
            } => {
                // Sui events are immutable; check Executed first (it
                // implies Approved). Two queries; both are idempotent.
                let executed_event_type = format!("{gateway_pkg}::events::MessageExecuted");
                let approved_event_type = format!("{gateway_pkg}::events::MessageApproved");
                let executed = client
                    .has_message_executed(&executed_event_type, source_chain, &tx.message_id)
                    .await
                    .unwrap_or(false);
                if executed {
                    return Ok(ApprovalResult::AlreadyExecuted);
                }
                let approved = client
                    .has_message_approved(&approved_event_type, source_chain, &tx.message_id)
                    .await
                    .unwrap_or(false);
                if approved {
                    Ok(ApprovalResult::Approved)
                } else {
                    Ok(ApprovalResult::NotYet)
                }
            }
        }
    }

    async fn check_executed(
        &self,
        tx: &PendingTx,
        _idx: usize,
        source_chain: &str,
    ) -> Result<bool> {
        match self {
            Self::Evm { gw_contract } => {
                let approved = check_evm_is_message_approved(
                    gw_contract,
                    source_chain,
                    &tx.message_id,
                    &tx.source_address,
                    tx.contract_addr,
                    tx.payload_hash,
                )
                .await?;
                // false = approval consumed = executed
                Ok(!approved)
            }
            Self::Solana { rpc_client, .. } => {
                let client = rpc_client.clone();
                let cmd_id = tx.command_id.unwrap_or_default();
                let result = tokio::task::spawn_blocking(move || {
                    check_solana_incoming_message(&client, &cmd_id)
                })
                .await??;
                match result {
                    Some(status) if status != 0 => Ok(true),
                    _ => Ok(false),
                }
            }
            Self::Stellar {
                client,
                gateway_contract,
                signer_pk,
                ..
            } => {
                let executed = client
                    .gateway_is_message_executed(
                        signer_pk,
                        gateway_contract,
                        source_chain,
                        &tx.message_id,
                    )
                    .await
                    .unwrap_or(None);
                Ok(matches!(executed, Some(true)))
            }
            Self::Sui {
                client,
                gateway_pkg,
                ..
            } => {
                let event_type = format!("{gateway_pkg}::events::MessageExecuted");
                let executed = client
                    .has_message_executed(&event_type, source_chain, &tx.message_id)
                    .await
                    .unwrap_or(false);
                Ok(executed)
            }
        }
    }

    fn approval_label(&self) -> &str {
        match self {
            Self::Evm { .. } => "EVM approval",
            Self::Solana { .. } => "Solana approval",
            Self::Stellar { .. } => "Stellar approval",
            Self::Sui { .. } => "Sui approval",
        }
    }

    fn execution_label(&self) -> &str {
        match self {
            Self::Evm { .. } => "EVM execution",
            Self::Solana { .. } => "Solana execution",
            Self::Stellar { .. } => "Stellar execution",
            Self::Sui { .. } => "Sui execution",
        }
    }
}

// ---------------------------------------------------------------------------
// Check outcome — returned from parallel checks, applied to txs afterward
// ---------------------------------------------------------------------------

#[allow(dead_code)]
enum CheckOutcome {
    /// No change (not ready yet, or tx was already terminal).
    NotYet,
    /// Phase completed — record timing and advance.
    PhaseComplete { elapsed: f64 },
    /// Voted phase: no VotingVerifier, just advance to Routed.
    SkipVoting,
    /// Approved check found the tx already executed — skip to Done.
    AlreadyExecuted { elapsed: f64 },
    /// Second-leg discovered — carries the extracted info.
    SecondLegDiscovered {
        message_id: String,
        payload_hash: String,
        source_address: String,
        destination_address: String,
    },
    /// Check returned an error.
    Error(String),
}

// ---------------------------------------------------------------------------
// Unified polling pipeline
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
async fn poll_pipeline<P: Provider>(
    txs: &mut Vec<PendingTx>,
    lcd: &str,
    voting_verifier: Option<&str>,
    cosm_gateway: Option<&str>,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    checker: &DestinationChecker<'_, P>,
    axelarnet_gateway: Option<&str>,
    display_chain: Option<&str>,
    mut rx: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
    send_done: Option<&AtomicBool>,
    external_spinner: Option<indicatif::ProgressBar>,
) -> PeakThroughput {
    let spinner =
        external_spinner.unwrap_or_else(|| ui::wait_spinner("verifying pipeline (starting)..."));
    let mut last_progress = Instant::now();
    let mut rt_stats = RealTimeStats::new();
    let mut received_first_tx = false;

    // For EVM destinations, derive the contract_addr from destination_address
    // so streaming PendingTx entries (which may have Address::ZERO) get the right value.
    let default_contract_addr: Address = destination_address.parse().unwrap_or(Address::ZERO);

    loop {
        // Drain any newly-confirmed txs from the streaming channel.
        if let Some(ref mut receiver) = rx {
            while let Ok(mut new_tx) = receiver.try_recv() {
                if new_tx.contract_addr == Address::ZERO && default_contract_addr != Address::ZERO {
                    new_tx.contract_addr = default_contract_addr;
                }
                txs.push(new_tx);
            }
        }

        let sending_complete = send_done.is_none_or(|f| f.load(Ordering::Relaxed));

        let total = txs.len();
        if total == 0 {
            if sending_complete {
                break;
            }
            // Still sending — wait for txs to arrive.
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        if !received_first_tx {
            received_first_tx = true;
            spinner.set_message(format!("verifying pipeline: 0/{total} confirmed..."));
        }

        // Collect indices of non-terminal txs
        let active: Vec<usize> = (0..txs.len())
            .filter(|&i| !txs[i].failed && txs[i].phase != Phase::Done)
            .collect();

        if active.is_empty() && sending_complete {
            break;
        }
        if active.is_empty() {
            // All current txs done but send still in progress — wait for more.
            tokio::time::sleep(POLL_INTERVAL).await;
            last_progress = Instant::now();
            continue;
        }

        // --- Phase-grouped batch polling (all phases in parallel) ---
        // Each cycle: collect ALL txs per phase, fire batch queries for all
        // phases concurrently. Within each phase, HTTP chunks of COSMOS_BATCH_SIZE
        // run concurrently too. This keeps poll cycles fast even at 600+ txs.
        let error_msg: Option<String> = None;

        // Snapshot indices by phase (no borrows held after this).
        let voted_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Voted)
            .copied()
            .collect();
        let routed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Routed)
            .copied()
            .collect();
        let hub_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::HubApproved)
            .copied()
            .collect();
        let approved_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Approved)
            .copied()
            .collect();
        let executed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Executed)
            .copied()
            .collect();

        // Build owned data for batch queries (avoids holding borrows on txs).
        let voted_data: Vec<(usize, String, String, String)> = voted_indices
            .iter()
            .map(|&i| {
                (
                    i,
                    txs[i].message_id.clone(),
                    txs[i].source_address.clone(),
                    txs[i].payload_hash_hex.clone(),
                )
            })
            .collect();
        let routed_data: Vec<(usize, String)> = routed_indices
            .iter()
            .map(|&i| (i, txs[i].message_id.clone()))
            .collect();
        let hub_data: Vec<(usize, String)> = hub_indices
            .iter()
            .map(|&i| (i, txs[i].message_id.clone()))
            .collect();
        let dest_indices: Vec<usize> = approved_indices
            .iter()
            .chain(executed_indices.iter())
            .copied()
            .collect();
        let dest_data: Vec<(usize, [u8; 32])> = dest_indices
            .iter()
            .map(|&i| (i, txs[i].command_id.unwrap_or_default()))
            .collect();

        // Fire Cosmos phases concurrently (each internally chunks into COSMOS_BATCH_SIZE).
        let (voted_results, routed_results, hub_results) = tokio::join!(
            // Voted
            async {
                if voted_data.is_empty() {
                    return Vec::new();
                }
                if let Some(vv) = voting_verifier {
                    batch_check_voting_verifier_owned(
                        lcd,
                        vv,
                        source_chain,
                        destination_chain,
                        destination_address,
                        &voted_data,
                    )
                    .await
                } else {
                    // No VotingVerifier — all pass immediately
                    voted_data.iter().map(|(i, ..)| (*i, true)).collect()
                }
            },
            // Routed
            async {
                if routed_data.is_empty() {
                    return Vec::new();
                }
                if let Some(gw) = cosm_gateway {
                    batch_check_cosmos_routed_owned(lcd, gw, source_chain, &routed_data).await
                } else {
                    routed_data.iter().map(|(i, _)| (*i, true)).collect()
                }
            },
            // HubApproved
            async {
                if hub_data.is_empty() {
                    return Vec::new();
                }
                if let Some(gw) = axelarnet_gateway {
                    batch_check_hub_approved_owned(lcd, gw, source_chain, &hub_data).await
                } else {
                    hub_data.iter().map(|(i, _)| (*i, true)).collect()
                }
            },
        );

        // Apply Cosmos results
        for (i, ok) in voted_results {
            if ok {
                txs[i].timing.voted_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::Routed;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in routed_results {
            if ok {
                txs[i].timing.routed_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = if axelarnet_gateway.is_some() {
                    Phase::HubApproved
                } else {
                    Phase::Approved
                };
                last_progress = Instant::now();
            }
        }
        for (i, ok) in hub_results {
            if ok {
                txs[i].timing.hub_approved_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::Approved;
                last_progress = Instant::now();
            }
        }

        // Destination checks (Solana batch / EVM individual)
        if !dest_data.is_empty() {
            match checker {
                DestinationChecker::Solana { rpc_client, .. } => {
                    let client = rpc_client.clone();
                    let data = dest_data;
                    let results = tokio::task::spawn_blocking(move || {
                        batch_check_solana_incoming_messages(&client, &data)
                    })
                    .await
                    .unwrap_or_default();

                    for (i, status) in results {
                        match (txs[i].phase, status) {
                            (Phase::Approved, Some(0)) => {
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].phase = Phase::Executed;
                                last_progress = Instant::now();
                            }
                            (Phase::Approved, Some(_)) => {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            (Phase::Executed, Some(s)) if s > 0 => {
                                txs[i].timing.executed_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
                DestinationChecker::Evm { gw_contract } => {
                    let futs: Vec<_> = dest_indices
                        .iter()
                        .map(|&i| {
                            let phase = txs[i].phase;
                            let msg_id = txs[i].message_id.clone();
                            let src_addr = txs[i].source_address.clone();
                            let c_addr = txs[i].contract_addr;
                            let p_hash = txs[i].payload_hash;
                            async move {
                                let approved = check_evm_is_message_approved(
                                    gw_contract,
                                    source_chain,
                                    &msg_id,
                                    &src_addr,
                                    c_addr,
                                    p_hash,
                                )
                                .await
                                .unwrap_or(false);
                                (i, phase, approved)
                            }
                        })
                        .collect();
                    let results: Vec<_> = futures::stream::iter(futs)
                        .buffer_unordered(20)
                        .collect()
                        .await;
                    for (i, phase, approved) in results {
                        match phase {
                            Phase::Approved if approved => {
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].phase = Phase::Executed;
                                last_progress = Instant::now();
                            }
                            Phase::Approved => {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            Phase::Executed if !approved => {
                                txs[i].timing.executed_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
                DestinationChecker::Stellar {
                    client,
                    gateway_contract,
                    signer_pk,
                    ..
                } => {
                    for &i in &dest_indices {
                        let phase = txs[i].phase;
                        let approved = client
                            .gateway_is_message_approved(
                                signer_pk,
                                gateway_contract,
                                source_chain,
                                &txs[i].message_id,
                                &txs[i].source_address,
                                &txs[i].gmp_destination_address,
                                txs[i].payload_hash.0,
                            )
                            .await
                            .ok()
                            .flatten();
                        let executed = if matches!(phase, Phase::Executed) {
                            client
                                .gateway_is_message_executed(
                                    signer_pk,
                                    gateway_contract,
                                    source_chain,
                                    &txs[i].message_id,
                                )
                                .await
                                .ok()
                                .flatten()
                        } else {
                            None
                        };
                        match phase {
                            Phase::Approved if matches!(approved, Some(true)) => {
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].phase = Phase::Executed;
                                last_progress = Instant::now();
                            }
                            Phase::Executed if matches!(executed, Some(true)) => {
                                txs[i].timing.executed_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
                DestinationChecker::Sui {
                    client,
                    gateway_pkg,
                    ..
                } => {
                    let approved_event_type = format!("{gateway_pkg}::events::MessageApproved");
                    let executed_event_type = format!("{gateway_pkg}::events::MessageExecuted");
                    for &i in &dest_indices {
                        let phase = txs[i].phase;
                        let approved = client
                            .has_message_approved(
                                &approved_event_type,
                                source_chain,
                                &txs[i].message_id,
                            )
                            .await
                            .unwrap_or(false);
                        let executed = client
                            .has_message_executed(
                                &executed_event_type,
                                source_chain,
                                &txs[i].message_id,
                            )
                            .await
                            .unwrap_or(false);
                        match phase {
                            Phase::Approved if executed => {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            Phase::Approved if approved => {
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].phase = Phase::Executed;
                                last_progress = Instant::now();
                            }
                            Phase::Executed if executed => {
                                txs[i].timing.executed_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // Update spinner with multi-phase progress + real-time throughput/latency
        let (voted, routed, hub_approved, approved, executed) = phase_counts(txs);
        let counts = [voted, routed, hub_approved, approved, executed];
        rt_stats.update(counts, txs);
        if voted + routed + approved + executed > 0 || error_msg.is_some() {
            let msg = if axelarnet_gateway.is_some() {
                rt_stats.spinner_msg_its(counts, total, error_msg.as_deref())
            } else {
                rt_stats.spinner_msg_gmp(
                    counts,
                    total,
                    error_msg.as_deref(),
                    voting_verifier.is_some(),
                )
            };
            spinner.set_message(msg);
        }

        // If no tx has made progress for INACTIVITY_TIMEOUT, stop waiting.
        // During streaming (send still in progress), use 2× timeout to allow for
        // slow send phases, but still break to avoid hanging indefinitely.
        let timeout = if sending_complete {
            INACTIVITY_TIMEOUT
        } else {
            INACTIVITY_TIMEOUT * 2
        };
        if last_progress.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed
    for tx in txs.iter_mut() {
        if tx.failed || tx.phase == Phase::Done {
            continue;
        }
        tx.failed = true;
        let label = match tx.phase {
            Phase::Voted => "VotingVerifier",
            Phase::Routed => "cosmos routing",
            Phase::HubApproved => "hub approval",
            Phase::DiscoverSecondLeg => "second-leg discovery",
            Phase::Approved => checker.approval_label(),
            Phase::Executed => checker.execution_label(),
            Phase::Done => unreachable!(),
        };
        if tx.phase == Phase::Executed {
            tx.timing.executed_ok = Some(false);
        }
        tx.fail_reason = Some(format!("{label}: timed out"));
    }

    let total = txs.len();
    let (voted, routed, hub_approved, approved, executed) = phase_counts(txs);
    let hub_str = if axelarnet_gateway.is_some() {
        format!("  hub: {hub_approved}/{total}")
    } else {
        String::new()
    };
    spinner.finish_and_clear();
    let label = display_chain.unwrap_or(destination_chain);
    ui::success_annotated(
        &format!(
            "voted: {voted}/{total}  routed: {routed}/{total}{hub_str}  approved: {approved}/{total}  executed: {executed}/{total}"
        ),
        label,
    );

    compute_peak_throughput(txs)
}

// ---------------------------------------------------------------------------
// ITS hub-only pipeline (Voted → HubApproved)
// ---------------------------------------------------------------------------

/// Second-leg message info extracted from hub execution tx.
struct SecondLegInfo {
    message_id: String,
    #[allow(dead_code)]
    source_chain: String,
    #[allow(dead_code)]
    destination_chain: String,
    payload_hash: String,
    source_address: String,
    destination_address: String,
}

/// Discover the second-leg message_id by searching for the hub execution tx
/// that consumed the first-leg message, then extracting routing event attributes.
async fn discover_second_leg(
    rpc: &str,
    first_leg_message_id: &str,
) -> Result<Option<SecondLegInfo>> {
    let resp = rpc_tx_search_event(
        rpc,
        "wasm-message_executed.message_id",
        first_leg_message_id,
    )
    .await?;

    let txs = resp
        .pointer("/result/txs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if txs.is_empty() {
        return Ok(None);
    }

    // Search through events for wasm-routing attributes
    let events = txs[0]
        .pointer("/tx_result/events")
        .and_then(|v| v.as_array());

    let events = match events {
        Some(e) => e,
        None => return Ok(None),
    };

    for event in events {
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if event_type != "wasm-routing" {
            continue;
        }

        let attrs = match event.get("attributes").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => continue,
        };

        let get_attr = |key: &str| -> Option<String> {
            attrs.iter().find_map(|a| {
                let k = a.get("key").and_then(|v| v.as_str())?;
                if k == key {
                    a.get("value")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
        };

        if let (Some(msg_id), Some(src), Some(dst), Some(ph)) = (
            get_attr("message_id"),
            get_attr("source_chain"),
            get_attr("destination_chain"),
            get_attr("payload_hash"),
        ) {
            return Ok(Some(SecondLegInfo {
                message_id: msg_id,
                source_chain: src,
                destination_chain: dst,
                payload_hash: ph,
                source_address: get_attr("source_address").unwrap_or_default(),
                destination_address: get_attr("destination_address").unwrap_or_default(),
            }));
        }
    }

    Ok(None)
}

/// Destination chain kind for `poll_pipeline_its_hub` to know how to query
/// the final approval/execution stage.
#[derive(Clone)]
pub enum ItsHubDest {
    /// Solana destination — uses `incoming_message` PDA + `command_id`.
    Solana { rpc_url: String },
    /// Stellar destination — uses Soroban `is_message_approved` /
    /// `is_message_executed` view calls on the gateway contract.
    Stellar {
        rpc_url: String,
        network_type: String,
        gateway_contract: String,
        signer_pk: [u8; 32],
    },
    /// XRPL destination — polls the recipient account's `account_tx` for an
    /// incoming `Payment` whose `message_id` memo matches the second-leg id.
    /// The XRPL relayer attaches that memo when broadcasting proofs.
    Xrpl {
        rpc_url: String,
        recipient_address: String,
    },
}

/// Full ITS polling pipeline: Voted → HubApproved → DiscoverSecondLeg → Routed → Approved → Executed.
#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
async fn poll_pipeline_its_hub(
    txs: &mut Vec<PendingTx>,
    lcd: &str,
    voting_verifier: Option<&str>,
    source_chain: &str,
    axelarnet_gateway: &str,
    rpc: &str,
    cosm_gateway_dest: &str,
    dest: ItsHubDest,
    mut rx: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
    send_done: Option<&AtomicBool>,
    external_spinner: Option<indicatif::ProgressBar>,
) -> PeakThroughput {
    let spinner = external_spinner
        .unwrap_or_else(|| ui::wait_spinner("verifying ITS pipeline (starting)..."));
    let mut last_progress = Instant::now();
    let mut rt_stats = RealTimeStats::new();
    let mut received_first_tx = false;

    let sol_rpc_client = match &dest {
        ItsHubDest::Solana { rpc_url } => Some(Arc::new(
            solana_client::rpc_client::RpcClient::new_with_commitment(
                rpc_url.clone(),
                solana_commitment_config::CommitmentConfig::finalized(),
            ),
        )),
        _ => None,
    };
    let stellar_client = match &dest {
        ItsHubDest::Stellar {
            rpc_url,
            network_type,
            ..
        } => Some(crate::stellar::StellarClient::new(rpc_url, network_type).ok()),
        _ => None,
    }
    .flatten();
    let xrpl_client = match &dest {
        ItsHubDest::Xrpl { rpc_url, .. } => Some(crate::xrpl::XrplClient::new(rpc_url)),
        _ => None,
    };

    // Skip voting phase entirely if no VotingVerifier
    if voting_verifier.is_none() {
        for tx in txs.iter_mut() {
            if tx.phase == Phase::Voted {
                tx.phase = Phase::HubApproved;
            }
        }
    }

    loop {
        // Drain any newly-confirmed txs from the streaming channel.
        if let Some(ref mut receiver) = rx {
            while let Ok(mut new_tx) = receiver.try_recv() {
                // Skip voting if no verifier
                if voting_verifier.is_none() && new_tx.phase == Phase::Voted {
                    new_tx.phase = Phase::HubApproved;
                }
                txs.push(new_tx);
            }
        }

        let sending_complete = send_done.is_none_or(|f| f.load(Ordering::Relaxed));

        let total = txs.len();
        if total == 0 {
            if sending_complete {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        if !received_first_tx {
            received_first_tx = true;
        }

        let active: Vec<usize> = (0..txs.len())
            .filter(|&i| !txs[i].failed && txs[i].phase != Phase::Done)
            .collect();

        if active.is_empty() && sending_complete {
            break;
        }
        if active.is_empty() {
            tokio::time::sleep(POLL_INTERVAL).await;
            last_progress = Instant::now();
            continue;
        }

        let error_msg: Option<String> = None;

        // Snapshot indices by phase
        let voted_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Voted)
            .copied()
            .collect();
        let hub_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::HubApproved)
            .copied()
            .collect();
        let discover_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::DiscoverSecondLeg)
            .copied()
            .collect();
        let routed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Routed)
            .copied()
            .collect();
        let approved_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Approved)
            .copied()
            .collect();
        let executed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Executed)
            .copied()
            .collect();

        // Build owned data for batch queries
        let voted_data: Vec<(usize, String, String, String)> = voted_indices
            .iter()
            .map(|&i| {
                (
                    i,
                    txs[i].message_id.clone(),
                    txs[i].source_address.clone(),
                    txs[i].payload_hash_hex.clone(),
                )
            })
            .collect();
        let hub_data: Vec<(usize, String)> = hub_indices
            .iter()
            .map(|&i| (i, txs[i].message_id.clone()))
            .collect();
        let routed_data: Vec<(usize, String)> = routed_indices
            .iter()
            .map(|&i| (i, txs[i].second_leg_message_id.clone().unwrap_or_default()))
            .collect();

        // Solana destination checks: need command_id from second_leg_message_id
        let sol_dest_indices: Vec<usize> = approved_indices
            .iter()
            .chain(executed_indices.iter())
            .copied()
            .collect();
        let sol_dest_data: Vec<(usize, [u8; 32])> = sol_dest_indices
            .iter()
            .map(|&i| {
                let sl_id = txs[i].second_leg_message_id.as_deref().unwrap_or("");
                let input = [b"axelar-".as_slice(), sl_id.as_bytes()].concat();
                (i, keccak256(&input).into())
            })
            .collect();

        // --- Fire Cosmos batch phases concurrently ---
        let dest_chain_for_vv = txs
            .first()
            .map(|t| t.gmp_destination_chain.clone())
            .unwrap_or_default();
        let dest_addr_for_vv = txs
            .first()
            .map(|t| t.gmp_destination_address.clone())
            .unwrap_or_default();

        let (voted_results, hub_results, routed_results) = tokio::join!(
            async {
                if voted_data.is_empty() {
                    return Vec::new();
                }
                if let Some(vv) = voting_verifier {
                    batch_check_voting_verifier_owned(
                        lcd,
                        vv,
                        source_chain,
                        &dest_chain_for_vv,
                        &dest_addr_for_vv,
                        &voted_data,
                    )
                    .await
                } else {
                    voted_data.iter().map(|(i, ..)| (*i, true)).collect()
                }
            },
            async {
                if hub_data.is_empty() {
                    return Vec::new();
                }
                batch_check_hub_approved_owned(lcd, axelarnet_gateway, source_chain, &hub_data)
                    .await
            },
            async {
                if routed_data.is_empty() {
                    return Vec::new();
                }
                batch_check_cosmos_routed_owned(lcd, cosm_gateway_dest, "axelar", &routed_data)
                    .await
            },
        );

        // Apply Cosmos results
        for (i, ok) in voted_results {
            if ok {
                txs[i].timing.voted_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::HubApproved;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in hub_results {
            if ok {
                txs[i].timing.hub_approved_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::DiscoverSecondLeg;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in routed_results {
            if ok {
                txs[i].timing.routed_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::Approved;
                last_progress = Instant::now();
            }
        }

        // --- DiscoverSecondLeg: individual RPC tx_search (can't batch) ---
        if !discover_indices.is_empty() {
            let discover_futs: Vec<_> = discover_indices
                .iter()
                .map(|&i| {
                    let msg_id = txs[i].message_id.clone();
                    async move {
                        match discover_second_leg(rpc, &msg_id).await {
                            Ok(Some(info)) => (i, Some(info)),
                            _ => (i, None),
                        }
                    }
                })
                .collect();
            let discover_results: Vec<_> = futures::stream::iter(discover_futs)
                .buffer_unordered(20)
                .collect()
                .await;
            for (i, info) in discover_results {
                if let Some(info) = info {
                    txs[i].second_leg_message_id = Some(info.message_id);
                    txs[i].second_leg_payload_hash = Some(info.payload_hash);
                    txs[i].second_leg_source_address = Some(info.source_address);
                    txs[i].second_leg_destination_address = Some(info.destination_address);
                    txs[i].phase = Phase::Routed;
                    last_progress = Instant::now();
                }
            }
        }

        // --- Destination checks (per-chain) ---
        if !sol_dest_data.is_empty() || !approved_indices.is_empty() || !executed_indices.is_empty()
        {
            match &dest {
                ItsHubDest::Solana { .. } => {
                    if let Some(client) = sol_rpc_client.as_ref()
                        && !sol_dest_data.is_empty()
                    {
                        let client = client.clone();
                        let data = sol_dest_data;
                        let results = tokio::task::spawn_blocking(move || {
                            batch_check_solana_incoming_messages(&client, &data)
                        })
                        .await
                        .unwrap_or_default();

                        for (i, status) in results {
                            match (txs[i].phase, status) {
                                (Phase::Approved, Some(0)) => {
                                    txs[i].timing.approved_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].phase = Phase::Executed;
                                    last_progress = Instant::now();
                                }
                                (Phase::Approved, Some(_)) => {
                                    let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                    if txs[i].timing.approved_secs.is_none() {
                                        txs[i].timing.approved_secs = Some(elapsed);
                                    }
                                    txs[i].timing.executed_secs = Some(elapsed);
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                                (Phase::Executed, Some(s)) if s > 0 => {
                                    txs[i].timing.executed_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                ItsHubDest::Stellar {
                    gateway_contract,
                    signer_pk,
                    ..
                } => {
                    if let Some(client) = stellar_client.as_ref() {
                        let pending: Vec<usize> = approved_indices
                            .iter()
                            .chain(executed_indices.iter())
                            .copied()
                            .collect();
                        for i in pending {
                            let second_leg = match txs[i].second_leg_message_id.as_deref() {
                                Some(s) => s,
                                None => continue,
                            };
                            // For ITS, the destination contract on Stellar is the
                            // ITS proxy (not the example), and the second-leg
                            // source is "axelar". Ignore parse failures.
                            let dest_contract = txs[i]
                                .second_leg_destination_address
                                .clone()
                                .unwrap_or_default();
                            let src_addr =
                                txs[i].second_leg_source_address.clone().unwrap_or_default();
                            let payload_hash = txs[i]
                                .second_leg_payload_hash
                                .as_deref()
                                .and_then(|s| parse_payload_hash(s).ok())
                                .unwrap_or_default();
                            let payload_hash_arr: [u8; 32] = payload_hash.0;

                            let phase = txs[i].phase;
                            // For approved phase, query is_message_approved.
                            // For executed phase, query is_message_executed.
                            let result = if phase == Phase::Approved {
                                client
                                    .gateway_is_message_approved(
                                        signer_pk,
                                        gateway_contract,
                                        "axelar",
                                        second_leg,
                                        &src_addr,
                                        &dest_contract,
                                        payload_hash_arr,
                                    )
                                    .await
                                    .ok()
                                    .flatten()
                                    .map(|approved| if approved { 0u8 } else { 1u8 })
                            } else {
                                client
                                    .gateway_is_message_executed(
                                        signer_pk,
                                        gateway_contract,
                                        "axelar",
                                        second_leg,
                                    )
                                    .await
                                    .ok()
                                    .flatten()
                                    .map(|executed| if executed { 1u8 } else { 0u8 })
                            };

                            match (phase, result) {
                                (Phase::Approved, Some(0)) => {
                                    // approved=true → advance
                                    txs[i].timing.approved_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].phase = Phase::Executed;
                                    last_progress = Instant::now();
                                }
                                (Phase::Executed, Some(1)) => {
                                    // executed=true → done
                                    txs[i].timing.executed_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                ItsHubDest::Xrpl {
                    recipient_address, ..
                } => {
                    if let Some(client) = xrpl_client.as_ref() {
                        // For XRPL we collapse approved + executed into a
                        // single "delivered" check: a Payment from the
                        // multisig with the matching `message_id` memo means
                        // the relayer's proof was broadcast and validated.
                        let pending: Vec<usize> = approved_indices
                            .iter()
                            .chain(executed_indices.iter())
                            .copied()
                            .collect();
                        for i in pending {
                            let second_leg = match txs[i].second_leg_message_id.as_deref() {
                                Some(s) => s,
                                None => continue,
                            };
                            let found = client
                                .find_inbound_with_message_id(recipient_address, second_leg, None)
                                .await
                                .ok()
                                .flatten();
                            if found.is_some() {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                        }
                    }
                }
            }
        }

        let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
        let routed = txs
            .iter()
            .filter(|t| t.timing.routed_secs.is_some())
            .count();
        let counts = [voted, routed, hub_approved, approved, executed];
        rt_stats.update(counts, txs);

        if voted + hub_approved + routed + approved + executed > 0 || error_msg.is_some() {
            spinner.set_message(rt_stats.spinner_msg_its(counts, total, error_msg.as_deref()));
        }

        let timeout = if sending_complete {
            INACTIVITY_TIMEOUT
        } else {
            INACTIVITY_TIMEOUT * 2
        };
        if last_progress.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed
    let total = txs.len();
    for tx in txs.iter_mut() {
        if tx.failed || tx.phase == Phase::Done {
            continue;
        }
        tx.failed = true;
        let label = match tx.phase {
            Phase::Voted => "VotingVerifier",
            Phase::HubApproved => "hub approval",
            Phase::DiscoverSecondLeg => "second-leg discovery",
            Phase::Routed => "cosmos routing",
            Phase::Approved => "Solana approval",
            Phase::Executed => "Solana execution",
            Phase::Done => unreachable!(),
        };
        if tx.phase == Phase::Executed {
            tx.timing.executed_ok = Some(false);
        }
        tx.fail_reason = Some(format!("{label}: timed out"));
    }

    let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
    let routed = txs
        .iter()
        .filter(|t| t.timing.routed_secs.is_some())
        .count();

    spinner.finish_and_clear();
    ui::success(&format!(
        "ITS pipeline: voted: {voted}/{total}  hub: {hub_approved}/{total}  routed: {routed}/{total}  approved: {approved}/{total}  executed: {executed}/{total}"
    ));

    compute_peak_throughput(txs)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Source chain type — determines how message IDs are constructed.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum SourceChainType {
    /// Solana source: message ID = `{signature}-{group}.{index}`
    Svm,
    /// EVM source: message ID = `{tx_hash}-{event_index}` (already in tx.signature)
    Evm,
    /// XRPL source: message ID = `0x{lowercase_tx_hash}` (already in tx.signature)
    Xrpl,
    /// Stellar source: message ID = `0x{lowercase_tx_hash}-{event_index}`
    Stellar,
    /// Sui source: message ID = `{base58_tx_digest}-{event_index}`
    Sui,
}

/// Verify transactions on-chain through 4 Amplifier pipeline checkpoints:
///
/// 1. **Voted** — VotingVerifier verification (source chain)
/// 2. **Routed** — Destination Gateway outgoing_messages
/// 3. **Approved** — EVM gateway isMessageApproved
/// 4. **Executed** — EVM approval consumed
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain<P: Provider>(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    gateway_addr: Address,
    provider: &P,
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();

    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let (lcd, _, _, _) = read_axelar_config(config)?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, provider);
    let contract_addr: Address = destination_address.parse()?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            PendingTx {
                idx,
                message_id: match source_type {
                    SourceChainType::Evm
                    | SourceChainType::Xrpl
                    | SourceChainType::Stellar
                    | SourceChainType::Sui => tx.signature.clone(),
                    SourceChainType::Svm => {
                        format!("{}-{}.1", tx.signature, solana_call_contract_index())
                    }
                },
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                command_id: None, // EVM destination, not needed
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
                phase: initial_phase,
                second_leg_message_id: None,
                second_leg_payload_hash: None,
                second_leg_source_address: None,
                second_leg_destination_address: None,
            }
        })
        .collect();

    let checker = DestinationChecker::Evm {
        gw_contract: &gw_contract,
    };

    let peaks = poll_pipeline(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        Some(&cosm_gateway),
        source_chain,
        destination_chain,
        destination_address,
        &checker,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
}

/// Streaming version of `verify_onchain` for EVM destinations — runs
/// concurrently with the send phase, receiving confirmed txs via the channel.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_evm_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    gateway_addr: Address,
    evm_rpc_url: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let (lcd, _, _, _) = read_axelar_config(config)?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, &provider);

    let checker = DestinationChecker::Evm {
        gw_contract: &gw_contract,
    };

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = poll_pipeline(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        Some(&cosm_gateway),
        source_chain,
        destination_chain,
        destination_address,
        &checker,
        None,
        None,
        Some(&mut rx),
        Some(&send_done),
        Some(spinner),
    )
    .await;

    let report = compute_verification_report(&txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    Ok((report, timings))
}

/// GMP verification with a Stellar destination — uses Stellar's
/// `is_message_approved` / `is_message_executed` Soroban view calls
/// instead of an EVM gateway or Solana PDA.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_stellar_gmp(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_contract: &str,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway: &str,
    signer_pk: [u8; 32],
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let (lcd, _, _, _) = read_axelar_config(config)?;
    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;
    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            let message_id = match source_type {
                SourceChainType::Evm
                | SourceChainType::Xrpl
                | SourceChainType::Stellar
                | SourceChainType::Sui => tx.signature.clone(),
                SourceChainType::Svm => {
                    format!("{}-{}.1", tx.signature, solana_call_contract_index())
                }
            };
            PendingTx {
                idx,
                message_id,
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr: Address::ZERO,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                command_id: None,
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: destination_contract.to_string(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
                phase: initial_phase,
                second_leg_message_id: None,
                second_leg_payload_hash: None,
                second_leg_source_address: None,
                second_leg_destination_address: None,
            }
        })
        .collect();

    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, stellar_network_type)?;
    let checker: DestinationChecker<alloy::providers::RootProvider> = DestinationChecker::Stellar {
        client: stellar_client,
        gateway_contract: stellar_gateway.to_string(),
        signer_pk,
        _phantom: std::marker::PhantomData,
    };

    let peaks = poll_pipeline(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        Some(&cosm_gateway),
        source_chain,
        destination_chain,
        destination_contract,
        &checker,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
}

/// Streaming variant of `verify_onchain_stellar_gmp`.
/// Reserved for future sustained-mode flows (the burst-mode runners use
/// `verify_onchain_stellar_gmp` above today).
#[allow(clippy::too_many_arguments, dead_code)]
pub async fn verify_onchain_stellar_gmp_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_contract: &str,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway: &str,
    signer_pk: [u8; 32],
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let (lcd, _, _, _) = read_axelar_config(config)?;
    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, stellar_network_type)?;
    let checker: DestinationChecker<alloy::providers::RootProvider> = DestinationChecker::Stellar {
        client: stellar_client,
        gateway_contract: stellar_gateway.to_string(),
        signer_pk,
        _phantom: std::marker::PhantomData,
    };

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = poll_pipeline(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        Some(&cosm_gateway),
        source_chain,
        destination_chain,
        destination_contract,
        &checker,
        None,
        None,
        Some(&mut rx),
        Some(&send_done),
        Some(spinner),
    )
    .await;

    let report = compute_verification_report(&txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    Ok((report, timings))
}

/// Build a PendingTx for streaming GMP verification when the destination is
/// Stellar — populates `gmp_destination_address` so the Stellar checker
/// has the contract C-address to query.
#[allow(dead_code)]
pub(super) fn tx_to_pending_stellar_gmp(
    tx: &TxMetrics,
    has_voting_verifier: bool,
    destination_contract: &str,
    source_type: SourceChainType,
) -> PendingTx {
    let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
    let message_id = match source_type {
        SourceChainType::Evm
        | SourceChainType::Xrpl
        | SourceChainType::Stellar
        | SourceChainType::Sui => tx.signature.clone(),
        SourceChainType::Svm => {
            format!("{}-{}.1", tx.signature, solana_call_contract_index())
        }
    };
    PendingTx {
        idx: 0,
        message_id,
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: destination_contract.to_string(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::Routed
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
    }
}

/// Convert a confirmed TxMetrics into a PendingTx for Solana verification.
pub(super) fn tx_to_pending_solana(
    tx: &TxMetrics,
    idx: usize,
    source_chain: &str,
    has_voting_verifier: bool,
    source_type: SourceChainType,
) -> PendingTx {
    let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
    let message_id = match source_type {
        SourceChainType::Evm
        | SourceChainType::Xrpl
        | SourceChainType::Stellar
        | SourceChainType::Sui => tx.signature.clone(),
        SourceChainType::Svm => {
            format!("{}-{}.1", tx.signature, solana_call_contract_index())
        }
    };
    let cmd_input = [source_chain.as_bytes(), b"-", message_id.as_bytes()].concat();
    PendingTx {
        idx,
        message_id,
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: Some(keccak256(&cmd_input).into()),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::Routed
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
    }
}

/// Convert a confirmed TxMetrics into a PendingTx for EVM GMP verification.
#[allow(dead_code)]
pub(super) fn tx_to_pending_evm(
    tx: &TxMetrics,
    _source_chain: &str,
    contract_addr: Address,
    has_voting_verifier: bool,
    source_type: SourceChainType,
) -> PendingTx {
    let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
    let message_id = match source_type {
        SourceChainType::Evm
        | SourceChainType::Xrpl
        | SourceChainType::Stellar
        | SourceChainType::Sui => tx.signature.clone(),
        SourceChainType::Svm => {
            format!("{}-{}.1", tx.signature, solana_call_contract_index())
        }
    };
    PendingTx {
        idx: 0,
        message_id,
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::Routed
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
    }
}

/// Convert a confirmed TxMetrics into a PendingTx for Stellar-sourced GMP.
/// The `signature` field on the input is the pre-formatted message id
/// (`0x{lowercase_hex_tx_hash}-{event_index}`) per the `hex_tx_hash_and_event_index`
/// format of the Stellar `VotingVerifier`.
pub(super) fn tx_to_pending_stellar(
    tx: &TxMetrics,
    has_voting_verifier: bool,
    contract_addr: Address,
) -> PendingTx {
    let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
    PendingTx {
        idx: 0,
        message_id: tx.signature.clone(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::Routed
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
    }
}

/// Convert a confirmed TxMetrics into a PendingTx for XRPL-sourced ITS
/// verification. The `signature` field on the input is the already-formatted
/// XRPL message id (`0x{lowercase_hex_tx_hash}`), which is what the
/// `XrplVotingVerifier` / `XrplGateway` expect.
pub(super) fn tx_to_pending_xrpl(tx: &TxMetrics, has_voting_verifier: bool) -> PendingTx {
    let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
    PendingTx {
        idx: 0,
        message_id: tx.signature.clone(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: tx.gmp_destination_address.clone(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::HubApproved
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
    }
}

/// Convert a confirmed TxMetrics into a PendingTx for ITS hub verification.
/// ITS messages route through the hub, so gmp_destination_chain/address are
/// set from the TxMetrics (typically "axelar" / AxelarnetGateway).
pub(super) fn tx_to_pending_its(tx: &TxMetrics, has_voting_verifier: bool) -> PendingTx {
    let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
    PendingTx {
        idx: 0,
        message_id: tx.signature.clone(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: tx.gmp_destination_address.clone(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::HubApproved
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
    }
}

// ---------------------------------------------------------------------------
// Sui destination verifier (GMP)
// ---------------------------------------------------------------------------

/// Verify *->Sui GMP transactions through Amplifier. Uses Sui events polling
/// (`MessageApproved` / `MessageExecuted` on the AxelarGateway events module)
/// for the destination-side phases.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub async fn verify_onchain_sui_gmp_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    sui_rpc: &str,
    mut rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let (lcd, _, _, _) = read_axelar_config(config)?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let gateway_pkg = crate::sui::read_sui_gateway_pkg(config, destination_chain)?;
    let sui_client = crate::sui::SuiClient::new(sui_rpc);

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> = DestinationChecker::Sui {
        client: sui_client,
        gateway_pkg,
        _phantom: std::marker::PhantomData,
    };

    let mut txs: Vec<PendingTx> = Vec::new();

    let peaks = poll_pipeline(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        Some(&cosm_gateway),
        source_chain,
        destination_chain,
        destination_address,
        &checker,
        None,
        None,
        Some(&mut rx),
        Some(&send_done),
        Some(spinner),
    )
    .await;

    let report = compute_verification_report(&txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    Ok((report, timings))
}

/// Burst-mode Sui destination verifier — block on confirmed metrics array.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_sui_gmp(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    sui_rpc: &str,
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();

    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let (lcd, _, _, _) = read_axelar_config(config)?;
    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;
    let gateway_pkg = crate::sui::read_sui_gateway_pkg(config, destination_chain)?;
    let sui_client = crate::sui::SuiClient::new(sui_rpc);

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            let message_id = match source_type {
                SourceChainType::Evm
                | SourceChainType::Xrpl
                | SourceChainType::Stellar
                | SourceChainType::Sui => tx.signature.clone(),
                SourceChainType::Svm => {
                    format!("{}-{}.1", tx.signature, solana_call_contract_index())
                }
            };
            PendingTx {
                idx,
                message_id,
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr: Address::ZERO,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                command_id: None,
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: destination_address.to_string(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
                phase: initial_phase,
                second_leg_message_id: None,
                second_leg_payload_hash: None,
                second_leg_source_address: None,
                second_leg_destination_address: None,
            }
        })
        .collect();

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> = DestinationChecker::Sui {
        client: sui_client,
        gateway_pkg,
        _phantom: std::marker::PhantomData,
    };

    let peaks = poll_pipeline(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        Some(&cosm_gateway),
        source_chain,
        destination_chain,
        destination_address,
        &checker,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Streaming verification for EVM→Solana in sustained mode.
///
/// Runs verification concurrently with the send phase. Receives confirmed
/// transactions via the channel and starts polling them immediately.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_solana_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    solana_rpc: &str,
    mut rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let (lcd, _, _, _) = read_axelar_config(config)?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    ));

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> =
        DestinationChecker::Solana {
            rpc_client,
            _phantom: std::marker::PhantomData,
        };

    let mut txs: Vec<PendingTx> = Vec::new();

    let peaks = poll_pipeline(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        Some(&cosm_gateway),
        source_chain,
        destination_chain,
        destination_address,
        &checker,
        None,
        None,
        Some(&mut rx),
        Some(&send_done),
        Some(spinner),
    )
    .await;

    let report = compute_verification_report(&txs, &mut [], peaks);
    // Key by message_id (signature) since streaming PendingTx idx is always 0.
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    Ok((report, timings))
}

/// Verify EVM->Solana transactions through the Amplifier pipeline:
///
/// 1. **Voted** — VotingVerifier verification (source EVM chain)
/// 2. **Routed** — Cosmos Gateway outgoing_messages (dest Solana chain)
/// 3. **Approved** — Solana IncomingMessage PDA exists
/// 4. **Executed** — Solana IncomingMessage PDA status = executed
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_solana(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    solana_rpc: &str,
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();

    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let (lcd, _, _, _) = read_axelar_config(config)?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            let message_id = match source_type {
                SourceChainType::Evm
                | SourceChainType::Xrpl
                | SourceChainType::Stellar
                | SourceChainType::Sui => tx.signature.clone(),
                SourceChainType::Svm => {
                    format!("{}-{}.1", tx.signature, solana_call_contract_index())
                }
            };
            let cmd_input = [source_chain.as_bytes(), b"-", message_id.as_bytes()].concat();
            PendingTx {
                idx,
                message_id,
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr: Address::ZERO,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                command_id: Some(keccak256(&cmd_input).into()),
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
                phase: initial_phase,
                second_leg_message_id: None,
                second_leg_payload_hash: None,
                second_leg_source_address: None,
                second_leg_destination_address: None,
            }
        })
        .collect();

    let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    ));

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> =
        DestinationChecker::Solana {
            rpc_client,
            _phantom: std::marker::PhantomData,
        };

    let peaks = poll_pipeline(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        Some(&cosm_gateway),
        source_chain,
        destination_chain,
        destination_address,
        &checker,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
}

/// Verify EVM->Solana ITS transactions through the Amplifier pipeline.
///
/// ITS messages route via the Axelar hub: the ContractCall event has
/// `destination_chain = "axelar"` and `destination_address = AxelarnetGateway`.
/// The VotingVerifier query must match these event values, not the final
/// destination (solana-18).
///
/// Phases tracked:
/// 1. **Voted** — VotingVerifier (dest = "axelar" / AxelarnetGateway)
/// 2. **Hub Approved** — AxelarnetGateway executable_messages
/// 3. **Discover Second Leg** — find second-leg message_id from hub execution tx
/// 4. **Routed** — Cosmos Gateway outgoing_messages (second-leg)
/// 5. **Approved** — Solana IncomingMessage PDA exists
/// 6. **Executed** — Solana IncomingMessage PDA status = executed
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_solana_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    _destination_address: &str,
    solana_rpc: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();

    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let (lcd, _, _, _) = read_axelar_config(config)?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();

    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            PendingTx {
                idx,
                message_id: tx.signature.clone(),
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr: Address::ZERO,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                command_id: None,
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: tx.gmp_destination_address.clone(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
                phase: initial_phase,
                second_leg_message_id: None,
                second_leg_payload_hash: None,
                second_leg_source_address: None,
                second_leg_destination_address: None,
            }
        })
        .collect();

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let peaks = poll_pipeline_its_hub(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        ItsHubDest::Solana {
            rpc_url: solana_rpc.to_string(),
        },
        None,
        None,
        None,
    )
    .await;

    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
}

/// Streaming version of `verify_onchain_solana_its` — runs concurrently with
/// the send phase, receiving confirmed txs via the channel.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_solana_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    solana_rpc: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let (lcd, _, _, _) = read_axelar_config(config)?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();

    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = poll_pipeline_its_hub(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        ItsHubDest::Solana {
            rpc_url: solana_rpc.to_string(),
        },
        Some(&mut rx),
        Some(&send_done),
        Some(spinner),
    )
    .await;

    let report = compute_verification_report(&txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    Ok((report, timings))
}

/// Verify EVM/Solana → Stellar ITS transactions. Mirrors
/// `verify_onchain_solana_its` but uses Stellar's `is_message_approved` /
/// `is_message_executed` view calls to detect destination-side approval and
/// execution. The `signer_pk` is just the source account for simulate
/// envelopes — read-only, no real authorization needed.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_stellar_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    _destination_address: &str,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway_contract: &str,
    signer_pk: [u8; 32],
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let (lcd, _, _, _) = read_axelar_config(config)?;
    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            PendingTx {
                idx,
                message_id: tx.signature.clone(),
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr: Address::ZERO,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                command_id: None,
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: tx.gmp_destination_address.clone(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
                phase: initial_phase,
                second_leg_message_id: None,
                second_leg_payload_hash: None,
                second_leg_source_address: None,
                second_leg_destination_address: None,
            }
        })
        .collect();

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let peaks = poll_pipeline_its_hub(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        ItsHubDest::Stellar {
            rpc_url: stellar_rpc.to_string(),
            network_type: stellar_network_type.to_string(),
            gateway_contract: stellar_gateway_contract.to_string(),
            signer_pk,
        },
        None,
        None,
        None,
    )
    .await;

    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
}

/// Streaming variant of `verify_onchain_stellar_its`.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_stellar_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway_contract: &str,
    signer_pk: [u8; 32],
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let (lcd, _, _, _) = read_axelar_config(config)?;
    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = poll_pipeline_its_hub(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        ItsHubDest::Stellar {
            rpc_url: stellar_rpc.to_string(),
            network_type: stellar_network_type.to_string(),
            gateway_contract: stellar_gateway_contract.to_string(),
            signer_pk,
        },
        Some(&mut rx),
        Some(&send_done),
        Some(spinner),
    )
    .await;

    let report = compute_verification_report(&txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    Ok((report, timings))
}

/// Verify EVM/Solana → XRPL ITS transactions. Polls the recipient XRPL
/// account's `account_tx` for an inbound `Payment` whose `message_id` memo
/// matches the second-leg message id (the XRPL relayer attaches that memo).
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_xrpl_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    xrpl_rpc: &str,
    xrpl_recipient: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let (lcd, _, _, _) = read_axelar_config(config)?;
    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            PendingTx {
                idx,
                message_id: tx.signature.clone(),
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr: Address::ZERO,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                command_id: None,
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: tx.gmp_destination_address.clone(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
                phase: initial_phase,
                second_leg_message_id: None,
                second_leg_payload_hash: None,
                second_leg_source_address: None,
                second_leg_destination_address: None,
            }
        })
        .collect();

    let rpc = read_axelar_rpc(config)?;
    // XRPL's destination cosmos gateway is `XrplGateway/{chain}`, not the
    // standard `Gateway/{chain}`. Try both so the same verifier works
    // regardless of which contract name the deployment uses.
    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )
    .or_else(|_| {
        read_axelar_contract_field(
            config,
            &format!("/axelar/contracts/XrplGateway/{destination_chain}/address"),
        )
    })?;

    let peaks = poll_pipeline_its_hub(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        ItsHubDest::Xrpl {
            rpc_url: xrpl_rpc.to_string(),
            recipient_address: xrpl_recipient.to_string(),
        },
        None,
        None,
        None,
    )
    .await;

    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
}

/// Streaming variant of `verify_onchain_xrpl_its`.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_xrpl_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    xrpl_rpc: &str,
    xrpl_recipient: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let (lcd, _, _, _) = read_axelar_config(config)?;
    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )
    .or_else(|_| {
        read_axelar_contract_field(
            config,
            &format!("/axelar/contracts/XrplGateway/{destination_chain}/address"),
        )
    })?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = poll_pipeline_its_hub(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        ItsHubDest::Xrpl {
            rpc_url: xrpl_rpc.to_string(),
            recipient_address: xrpl_recipient.to_string(),
        },
        Some(&mut rx),
        Some(&send_done),
        Some(spinner),
    )
    .await;

    let report = compute_verification_report(&txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    Ok((report, timings))
}

/// Verify Solana->EVM ITS transactions through the Amplifier pipeline.
///
/// ITS messages route via the Axelar hub: the Solana ITS program CPI's
/// `call_contract` with `destination_chain = "axelar"`.
///
/// Phases tracked:
/// 1. **Voted** — VotingVerifier (dest = "axelar" / AxelarnetGateway)
/// 2. **Hub Approved** — AxelarnetGateway executable_messages
/// 3. **Discover Second Leg** — find second-leg message_id from hub execution tx
/// 4. **Routed** — Cosmos Gateway outgoing_messages (second-leg, dest EVM chain)
/// 5. **Approved** — EVM gateway isMessageApproved (second-leg)
/// 6. **Executed** — EVM approval consumed
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_evm_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    _destination_address: &str,
    evm_gateway_addr: Address,
    evm_rpc_url: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();

    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let (lcd, _, _, _) = read_axelar_config(config)?;

    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    // For Solana ITS, we don't have the payload_hash (the ITS program constructs
    // the payload internally via CPI). Skip VotingVerifier and start at HubApproved,
    // which only needs source_chain + message_id. HubApproved implies voted.
    let initial_phase = Phase::HubApproved;
    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            PendingTx {
                idx,
                message_id: tx.signature.clone(),
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr: Address::ZERO,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                command_id: None,
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: tx.gmp_destination_address.clone(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
                phase: initial_phase,
                second_leg_message_id: None,
                second_leg_payload_hash: None,
                second_leg_source_address: None,
                second_leg_destination_address: None,
            }
        })
        .collect();

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(evm_gateway_addr, &provider);

    let peaks = poll_pipeline_its_hub_evm(
        &mut txs,
        &lcd,
        None, // skip VotingVerifier — no payload_hash for Solana ITS
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        &gw_contract,
        destination_chain,
        None,
        None,
        None,
    )
    .await;

    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
}

/// Streaming version of `verify_onchain_evm_its` — runs concurrently with
/// the send phase, receiving confirmed txs via the channel.
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_evm_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    evm_gateway_addr: Address,
    evm_rpc_url: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let (lcd, _, _, _) = read_axelar_config(config)?;

    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(evm_gateway_addr, &provider);

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = poll_pipeline_its_hub_evm(
        &mut txs,
        &lcd,
        None, // skip VotingVerifier — Solana ITS has no payload_hash
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        &gw_contract,
        destination_chain,
        Some(&mut rx),
        Some(&send_done),
        Some(spinner),
    )
    .await;

    let report = compute_verification_report(&txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    Ok((report, timings))
}

/// Full ITS polling pipeline with EVM destination (batch + streaming):
/// Voted → HubApproved → DiscoverSecondLeg → Routed → Approved(EVM) → Executed(EVM).
#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
async fn poll_pipeline_its_hub_evm<P: Provider>(
    txs: &mut Vec<PendingTx>,
    lcd: &str,
    voting_verifier: Option<&str>,
    source_chain: &str,
    axelarnet_gateway: &str,
    rpc: &str,
    cosm_gateway_dest: &str,
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    _destination_chain: &str,
    mut rx: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
    send_done: Option<&AtomicBool>,
    external_spinner: Option<indicatif::ProgressBar>,
) -> PeakThroughput {
    let spinner = external_spinner
        .unwrap_or_else(|| ui::wait_spinner("verifying ITS pipeline (starting)..."));
    let mut last_progress = Instant::now();
    let mut rt_stats = RealTimeStats::new();

    // Skip voting phase if no VotingVerifier
    if voting_verifier.is_none() {
        for tx in txs.iter_mut() {
            if tx.phase == Phase::Voted {
                tx.phase = Phase::HubApproved;
            }
        }
    }

    loop {
        // Drain streaming channel
        if let Some(ref mut receiver) = rx {
            while let Ok(mut new_tx) = receiver.try_recv() {
                if voting_verifier.is_none() && new_tx.phase == Phase::Voted {
                    new_tx.phase = Phase::HubApproved;
                }
                txs.push(new_tx);
            }
        }

        let sending_complete = send_done.is_none_or(|f| f.load(Ordering::Relaxed));
        let total = txs.len();

        if total == 0 {
            if sending_complete {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let active: Vec<usize> = (0..total)
            .filter(|&i| !txs[i].failed && txs[i].phase != Phase::Done)
            .collect();

        if active.is_empty() && sending_complete {
            break;
        }
        if active.is_empty() {
            tokio::time::sleep(POLL_INTERVAL).await;
            last_progress = Instant::now();
            continue;
        }

        let error_msg: Option<String> = None;

        // Snapshot indices by phase
        let voted_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Voted)
            .copied()
            .collect();
        let hub_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::HubApproved)
            .copied()
            .collect();
        let discover_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::DiscoverSecondLeg)
            .copied()
            .collect();
        let routed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Routed)
            .copied()
            .collect();
        let approved_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Approved)
            .copied()
            .collect();
        let executed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Executed)
            .copied()
            .collect();

        // Build owned data for batch queries
        let voted_data: Vec<(usize, String, String, String)> = voted_indices
            .iter()
            .map(|&i| {
                (
                    i,
                    txs[i].message_id.clone(),
                    txs[i].source_address.clone(),
                    txs[i].payload_hash_hex.clone(),
                )
            })
            .collect();
        let hub_data: Vec<(usize, String)> = hub_indices
            .iter()
            .map(|&i| (i, txs[i].message_id.clone()))
            .collect();
        let routed_data: Vec<(usize, String)> = routed_indices
            .iter()
            .map(|&i| (i, txs[i].second_leg_message_id.clone().unwrap_or_default()))
            .collect();

        let dest_chain_for_vv = txs
            .first()
            .map(|t| t.gmp_destination_chain.clone())
            .unwrap_or_default();
        let dest_addr_for_vv = txs
            .first()
            .map(|t| t.gmp_destination_address.clone())
            .unwrap_or_default();

        // --- Batch Cosmos phases concurrently ---
        let (voted_results, hub_results, routed_results) = tokio::join!(
            async {
                if voted_data.is_empty() {
                    return Vec::new();
                }
                if let Some(vv) = voting_verifier {
                    batch_check_voting_verifier_owned(
                        lcd,
                        vv,
                        source_chain,
                        &dest_chain_for_vv,
                        &dest_addr_for_vv,
                        &voted_data,
                    )
                    .await
                } else {
                    voted_data.iter().map(|(i, ..)| (*i, true)).collect()
                }
            },
            async {
                if hub_data.is_empty() {
                    return Vec::new();
                }
                batch_check_hub_approved_owned(lcd, axelarnet_gateway, source_chain, &hub_data)
                    .await
            },
            async {
                if routed_data.is_empty() {
                    return Vec::new();
                }
                batch_check_cosmos_routed_owned(lcd, cosm_gateway_dest, "axelar", &routed_data)
                    .await
            },
        );

        // Apply Cosmos results
        for (i, ok) in voted_results {
            if ok {
                txs[i].timing.voted_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::HubApproved;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in hub_results {
            if ok {
                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                if txs[i].timing.voted_secs.is_none() {
                    txs[i].timing.voted_secs = Some(elapsed);
                }
                txs[i].timing.hub_approved_secs = Some(elapsed);
                txs[i].phase = Phase::DiscoverSecondLeg;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in routed_results {
            if ok {
                txs[i].timing.routed_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::Approved;
                last_progress = Instant::now();
            }
        }

        // --- DiscoverSecondLeg: individual RPC tx_search ---
        if !discover_indices.is_empty() {
            let discover_futs: Vec<_> = discover_indices
                .iter()
                .map(|&i| {
                    let msg_id = txs[i].message_id.clone();
                    async move {
                        match discover_second_leg(rpc, &msg_id).await {
                            Ok(Some(info)) => (i, Some(info)),
                            _ => (i, None),
                        }
                    }
                })
                .collect();
            let discover_results: Vec<_> = futures::stream::iter(discover_futs)
                .buffer_unordered(20)
                .collect()
                .await;
            for (i, info) in discover_results {
                if let Some(info) = info {
                    txs[i].second_leg_message_id = Some(info.message_id);
                    txs[i].second_leg_payload_hash = Some(info.payload_hash);
                    txs[i].second_leg_source_address = Some(info.source_address);
                    txs[i].second_leg_destination_address = Some(info.destination_address);
                    txs[i].phase = Phase::Routed;
                    last_progress = Instant::now();
                }
            }
        }

        // --- EVM Approved/Executed checks (individual, buffer_unordered) ---
        let evm_check_indices: Vec<usize> = approved_indices
            .iter()
            .chain(executed_indices.iter())
            .copied()
            .collect();
        if !evm_check_indices.is_empty() {
            let evm_futs: Vec<_> = evm_check_indices
                .iter()
                .map(|&i| {
                    let phase = txs[i].phase;
                    let sl_id = txs[i].second_leg_message_id.clone().unwrap_or_default();
                    let sl_ph = txs[i].second_leg_payload_hash.clone().unwrap_or_default();
                    let sl_src = txs[i].second_leg_source_address.clone().unwrap_or_default();
                    let sl_dst = txs[i]
                        .second_leg_destination_address
                        .clone()
                        .unwrap_or_default();
                    async move {
                        let ph = parse_payload_hash(&sl_ph).unwrap_or_default();
                        let dst_addr: Address = sl_dst.parse().unwrap_or(Address::ZERO);
                        let approved = check_evm_is_message_approved(
                            gw_contract,
                            "axelar",
                            &sl_id,
                            &sl_src,
                            dst_addr,
                            ph,
                        )
                        .await
                        .unwrap_or(false);
                        (i, phase, approved)
                    }
                })
                .collect();
            let evm_results: Vec<_> = futures::stream::iter(evm_futs)
                .buffer_unordered(20)
                .collect()
                .await;
            for (i, phase, approved) in evm_results {
                match phase {
                    Phase::Approved if approved => {
                        txs[i].timing.approved_secs =
                            Some(txs[i].send_instant.elapsed().as_secs_f64());
                        txs[i].phase = Phase::Executed;
                        last_progress = Instant::now();
                    }
                    Phase::Executed if !approved => {
                        // false = approval consumed = executed
                        txs[i].timing.executed_secs =
                            Some(txs[i].send_instant.elapsed().as_secs_f64());
                        txs[i].timing.executed_ok = Some(true);
                        txs[i].phase = Phase::Done;
                        last_progress = Instant::now();
                    }
                    _ => {}
                }
            }
        }

        let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
        let routed = txs
            .iter()
            .filter(|t| t.timing.routed_secs.is_some())
            .count();
        let counts = [voted, routed, hub_approved, approved, executed];
        rt_stats.update(counts, txs);

        if voted + hub_approved + routed + approved + executed > 0 || error_msg.is_some() {
            spinner.set_message(rt_stats.spinner_msg_its(counts, total, error_msg.as_deref()));
        }

        let timeout = if sending_complete {
            INACTIVITY_TIMEOUT
        } else {
            INACTIVITY_TIMEOUT * 2
        };
        if last_progress.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed
    let total = txs.len();
    for tx in txs.iter_mut() {
        if tx.failed || tx.phase == Phase::Done {
            continue;
        }
        tx.failed = true;
        let label = match tx.phase {
            Phase::Voted => "VotingVerifier",
            Phase::HubApproved => "hub approval",
            Phase::DiscoverSecondLeg => "second-leg discovery",
            Phase::Routed => "cosmos routing",
            Phase::Approved => "EVM approval",
            Phase::Executed => "EVM execution",
            Phase::Done => unreachable!(),
        };
        if tx.phase == Phase::Executed {
            tx.timing.executed_ok = Some(false);
        }
        tx.fail_reason = Some(format!("{label}: timed out"));
    }

    let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
    let routed = txs
        .iter()
        .filter(|t| t.timing.routed_secs.is_some())
        .count();

    spinner.finish_and_clear();
    ui::success(&format!(
        "ITS pipeline: voted: {voted}/{total}  hub: {hub_approved}/{total}  routed: {routed}/{total}  approved: {approved}/{total}  executed: {executed}/{total}"
    ));

    compute_peak_throughput(txs)
}

/// Wait for an ITS remote deploy message to propagate through the hub pipeline
/// and execute on the EVM destination. The deploy message ID is `{sig}-1.3`.
///
/// Polls: Voted → HubApproved → DiscoverSecondLeg → Routed → Executed(EVM)
pub async fn wait_for_its_remote_deploy(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    deploy_message_id: &str,
    evm_gateway_addr: Address,
    evm_rpc_url: &str,
) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(config)?;
    let rpc = read_axelar_rpc(config)?;

    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();

    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(evm_gateway_addr, &provider);

    ui::kv("deploy message ID", deploy_message_id);
    let spinner = ui::wait_spinner("waiting for remote deploy to propagate through hub...");
    let start = Instant::now();
    let timeout = Duration::from_secs(300);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DeployPhase {
        Voted,
        HubApproved,
        DiscoverSecondLeg,
        Routed,
        Approved,
        Executed,
        Done,
    }

    let mut phase = if voting_verifier.is_some() {
        DeployPhase::Voted
    } else {
        DeployPhase::HubApproved
    };
    let mut second_leg_id: Option<String> = None;
    let mut second_leg_ph: Option<String> = None;

    loop {
        if start.elapsed() >= timeout {
            spinner.finish_and_clear();
            eyre::bail!(
                "remote deploy timed out after {}s at phase {phase:?}",
                timeout.as_secs()
            );
        }

        match phase {
            DeployPhase::Voted => {
                if let Some(ref vv) = voting_verifier {
                    // For deploy, we don't have payload_hash — use empty string
                    // VotingVerifier just needs the message to exist
                    if check_hub_approved(&lcd, &axelarnet_gateway, source_chain, deploy_message_id)
                        .await
                        .unwrap_or(false)
                    {
                        spinner.set_message("remote deploy: hub approved");
                        phase = DeployPhase::DiscoverSecondLeg;
                        continue;
                    }
                    // Also try voting verifier directly — but we'd need payload_hash.
                    // Skip directly to hub_approved check since it implies voted.
                    let _ = vv; // suppress unused warning
                }
                spinner.set_message("remote deploy: waiting for voting...");
            }
            DeployPhase::HubApproved => {
                if check_hub_approved(&lcd, &axelarnet_gateway, source_chain, deploy_message_id)
                    .await
                    .unwrap_or(false)
                {
                    spinner.set_message("remote deploy: hub approved");
                    phase = DeployPhase::DiscoverSecondLeg;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for hub approval...");
            }
            DeployPhase::DiscoverSecondLeg => {
                match discover_second_leg(&rpc, deploy_message_id).await {
                    Ok(Some(info)) => {
                        spinner.set_message(format!(
                            "remote deploy: second leg discovered ({})",
                            info.message_id
                        ));
                        second_leg_id = Some(info.message_id);
                        second_leg_ph = Some(info.payload_hash);
                        phase = DeployPhase::Routed;
                        continue;
                    }
                    Ok(None) => {
                        spinner.set_message("remote deploy: discovering second leg...");
                    }
                    Err(e) => {
                        spinner.set_message(format!("remote deploy: second leg error: {e}"));
                    }
                }
            }
            DeployPhase::Routed => {
                let sl_id = second_leg_id.as_deref().unwrap_or("");
                if check_cosmos_routed(&lcd, &cosm_gateway_dest, "axelar", sl_id)
                    .await
                    .unwrap_or(false)
                {
                    spinner.set_message("remote deploy: routed to destination");
                    phase = DeployPhase::Approved;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for routing...");
            }
            DeployPhase::Approved => {
                let sl_id = second_leg_id.as_deref().unwrap_or("");
                let sl_ph_str = second_leg_ph.as_deref().unwrap_or("");
                let ph = parse_payload_hash(sl_ph_str).unwrap_or_default();
                match check_evm_is_message_approved(
                    &gw_contract,
                    "axelar",
                    sl_id,
                    "",
                    Address::ZERO,
                    ph,
                )
                .await
                {
                    Ok(true) => {
                        spinner.set_message("remote deploy: approved on EVM");
                        phase = DeployPhase::Executed;
                        continue;
                    }
                    Ok(false) => {
                        // Could be already executed — check by trying executed phase
                        phase = DeployPhase::Executed;
                        continue;
                    }
                    Err(_) => {
                        spinner.set_message("remote deploy: waiting for EVM approval...");
                    }
                }
            }
            DeployPhase::Executed => {
                let sl_id = second_leg_id.as_deref().unwrap_or("");
                let sl_ph_str = second_leg_ph.as_deref().unwrap_or("");
                let ph = parse_payload_hash(sl_ph_str).unwrap_or_default();
                match check_evm_is_message_approved(
                    &gw_contract,
                    "axelar",
                    sl_id,
                    "",
                    Address::ZERO,
                    ph,
                )
                .await
                {
                    Ok(false) => {
                        // false = approval consumed = executed
                        phase = DeployPhase::Done;
                        continue;
                    }
                    Ok(true) => {
                        spinner.set_message("remote deploy: waiting for EVM execution...");
                    }
                    Err(_) => {
                        spinner.set_message("remote deploy: waiting for EVM execution...");
                    }
                }
            }
            DeployPhase::Done => break,
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    spinner.finish_and_clear();
    ui::success("remote token deployed on destination chain");
    Ok(())
}

/// Wait for a remote ITS token deploy to propagate through the hub and reach Solana.
///
/// Similar to `wait_for_its_remote_deploy` but for EVM→Solana direction.
/// Polls: Voted → HubApproved → DiscoverSecondLeg → Routed → Done
/// (We don't check Solana approval/execution — once routed, the Solana relayer
/// handles it. We just need the token to exist before sending transfers.)
pub async fn wait_for_its_remote_deploy_to_solana(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    deploy_message_id: &str,
    solana_rpc: &str,
) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(config)?;
    let rpc = read_axelar_rpc(config)?;

    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let sol_rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    );

    ui::kv("deploy message ID", deploy_message_id);
    let spinner =
        ui::wait_spinner("waiting for remote deploy to propagate through hub to Solana...");
    let start = Instant::now();
    let timeout = Duration::from_secs(300);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DeployPhase {
        HubApproved,
        DiscoverSecondLeg,
        Routed,
        Approved,
        Done,
    }

    let mut phase = DeployPhase::HubApproved;
    let mut second_leg_id: Option<String> = None;
    let mut approved_not_found_count: u32 = 0;

    loop {
        if start.elapsed() >= timeout {
            spinner.finish_and_clear();
            eyre::bail!(
                "remote deploy timed out after {}s at phase {phase:?}",
                timeout.as_secs()
            );
        }

        match phase {
            DeployPhase::HubApproved => {
                if check_hub_approved(&lcd, &axelarnet_gateway, source_chain, deploy_message_id)
                    .await
                    .unwrap_or(false)
                {
                    spinner.set_message("remote deploy: hub approved");
                    phase = DeployPhase::DiscoverSecondLeg;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for hub approval...");
            }
            DeployPhase::DiscoverSecondLeg => {
                match discover_second_leg(&rpc, deploy_message_id).await {
                    Ok(Some(info)) => {
                        spinner.set_message(format!(
                            "remote deploy: second leg discovered ({})",
                            info.message_id
                        ));
                        second_leg_id = Some(info.message_id);
                        phase = DeployPhase::Routed;
                        continue;
                    }
                    Ok(None) => {
                        spinner.set_message("remote deploy: discovering second leg...");
                    }
                    Err(e) => {
                        spinner.set_message(format!("remote deploy: second leg error: {e}"));
                    }
                }
            }
            DeployPhase::Routed => {
                let sl_id = second_leg_id.as_deref().unwrap_or("");
                if check_cosmos_routed(&lcd, &cosm_gateway_dest, "axelar", sl_id)
                    .await
                    .unwrap_or(false)
                {
                    spinner.set_message("remote deploy: routed to Solana");
                    phase = DeployPhase::Approved;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for routing...");
            }
            DeployPhase::Approved => {
                // Check if the Solana gateway has the incoming message.
                // The PDA may be absent if the message was already executed and
                // the account was closed, so after enough retries we assume done.
                let sl_id = second_leg_id.as_deref().unwrap_or("");
                let input = [b"axelar-".as_slice(), sl_id.as_bytes()].concat();
                let cmd_id: [u8; 32] = keccak256(&input).into();
                match check_solana_incoming_message(&sol_rpc_client, &cmd_id) {
                    Ok(Some(_)) => {
                        phase = DeployPhase::Done;
                        continue;
                    }
                    Ok(None) => {
                        approved_not_found_count += 1;
                        if approved_not_found_count >= 10 {
                            // PDA never appeared — likely already executed and closed
                            spinner.set_message(
                                "remote deploy: PDA not found, assuming already executed",
                            );
                            phase = DeployPhase::Done;
                            continue;
                        }
                        spinner.set_message("remote deploy: waiting for Solana approval...");
                    }
                    Err(_) => {
                        spinner.set_message("remote deploy: waiting for Solana approval...");
                    }
                }
            }
            DeployPhase::Done => break,
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    spinner.finish_and_clear();
    ui::success("remote token deployed on Solana");
    Ok(())
}

/// Wait for an ITS `deploy_remote_interchain_token` message to land on a
/// Stellar destination. Mirrors the EVM/Solana variants but checks the
/// destination Soroban gateway via `is_message_executed` for the second-leg
/// id. Stellar gateway state is small (mint+associate) so we poll
/// `is_message_executed` rather than tracking PDAs.
///
/// Staged: not yet wired into a runner. Lives here so the `*_to_stellar`
/// ITS modules (currently bailed at dispatch pending Stellar trusted-chain
/// upstream config) can pick it up unchanged.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub async fn wait_for_its_remote_deploy_to_stellar(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    deploy_message_id: &str,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway_addr: &str,
    signer_pk: [u8; 32],
) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(config)?;
    let rpc = read_axelar_rpc(config)?;

    let axelarnet_gateway =
        read_axelar_contract_field(config, "/axelar/contracts/AxelarnetGateway/address")?;

    let cosm_gateway_dest = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, stellar_network_type)?;

    ui::kv("deploy message ID", deploy_message_id);
    let spinner =
        ui::wait_spinner("waiting for remote deploy to propagate through hub to Stellar...");
    let start = Instant::now();
    let timeout = Duration::from_secs(300);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DeployPhase {
        HubApproved,
        DiscoverSecondLeg,
        Routed,
        Executed,
        Done,
    }

    let mut phase = DeployPhase::HubApproved;
    let mut second_leg_id: Option<String> = None;

    loop {
        if start.elapsed() >= timeout {
            spinner.finish_and_clear();
            eyre::bail!(
                "remote deploy timed out after {}s at phase {phase:?}",
                timeout.as_secs()
            );
        }

        match phase {
            DeployPhase::HubApproved => {
                if check_hub_approved(&lcd, &axelarnet_gateway, source_chain, deploy_message_id)
                    .await
                    .unwrap_or(false)
                {
                    spinner.set_message("remote deploy: hub approved");
                    phase = DeployPhase::DiscoverSecondLeg;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for hub approval...");
            }
            DeployPhase::DiscoverSecondLeg => {
                match discover_second_leg(&rpc, deploy_message_id).await {
                    Ok(Some(info)) => {
                        spinner.set_message(format!(
                            "remote deploy: second leg discovered ({})",
                            info.message_id
                        ));
                        second_leg_id = Some(info.message_id);
                        phase = DeployPhase::Routed;
                        continue;
                    }
                    Ok(None) => {
                        spinner.set_message("remote deploy: discovering second leg...");
                    }
                    Err(e) => {
                        spinner.set_message(format!("remote deploy: second leg error: {e}"));
                    }
                }
            }
            DeployPhase::Routed => {
                let sl_id = second_leg_id.as_deref().unwrap_or("");
                if check_cosmos_routed(&lcd, &cosm_gateway_dest, "axelar", sl_id)
                    .await
                    .unwrap_or(false)
                {
                    spinner.set_message("remote deploy: routed to Stellar");
                    phase = DeployPhase::Executed;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for routing...");
            }
            DeployPhase::Executed => {
                let sl_id = second_leg_id.as_deref().unwrap_or("");
                match stellar_client
                    .gateway_is_message_executed(&signer_pk, stellar_gateway_addr, "axelar", sl_id)
                    .await
                {
                    Ok(Some(true)) => {
                        phase = DeployPhase::Done;
                        continue;
                    }
                    _ => {
                        spinner.set_message("remote deploy: waiting for Stellar execution...");
                    }
                }
            }
            DeployPhase::Done => break,
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    spinner.finish_and_clear();
    ui::success("remote token deployed on Stellar");
    Ok(())
}

// ---------------------------------------------------------------------------
// Single-shot check helpers
// ---------------------------------------------------------------------------

/// Check VotingVerifier `messages_status` for a message.
/// Returns true if status contains "succeeded" (quorum reached).
#[allow(clippy::too_many_arguments, dead_code)]
async fn check_voting_verifier(
    lcd: &str,
    voting_verifier: &str,
    source_chain: &str,
    message_id: &str,
    source_address: &str,
    destination_chain: &str,
    destination_address: &str,
    payload_hash_hex: &str,
) -> Result<bool> {
    let query = json!({
        "messages_status": [{
            "cc_id": {
                "source_chain": source_chain,
                "message_id": message_id,
            },
            "source_address": source_address,
            "destination_chain": destination_chain,
            "destination_address": destination_address,
            "payload_hash": payload_hash_hex,
        }]
    });

    let resp = lcd_cosmwasm_smart_query(lcd, voting_verifier, &query).await?;
    let resp_str = serde_json::to_string(&resp)?;
    Ok(resp_str.to_lowercase().contains("succeeded"))
}

/// Check if message is routed on destination Cosmos Gateway via `outgoing_messages`.
async fn check_cosmos_routed(
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let query = json!({
        "outgoing_messages": [{
            "source_chain": source_chain,
            "message_id": message_id,
        }]
    });

    let resp = lcd_cosmwasm_smart_query(lcd, cosm_gateway, &query).await?;
    let data = resp.get("data").or_else(|| resp.as_array().map(|_| &resp));
    Ok(match data {
        Some(arr) if arr.is_array() => {
            let items = arr.as_array().unwrap();
            !items.is_empty() && !items.iter().all(|v| v.is_null())
        }
        _ => false,
    })
}

/// Check if a message is approved on the AxelarnetGateway hub via `executable_messages`.
async fn check_hub_approved(
    lcd: &str,
    axelarnet_gateway: &str,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let query = json!({
        "executable_messages": {
            "cc_ids": [{
                "source_chain": source_chain,
                "message_id": message_id,
            }]
        }
    });

    let resp = lcd_cosmwasm_smart_query(lcd, axelarnet_gateway, &query).await?;
    let resp_str = serde_json::to_string(&resp)?;
    // The message is executable if the response is non-null and contains the message_id
    Ok(!resp_str.contains("null") && resp_str.contains(message_id))
}

// ---------------------------------------------------------------------------
// Batch check helpers — one query per phase per poll cycle
// ---------------------------------------------------------------------------

/// Max messages per Cosmos LCD batch query. The query is base64-encoded in
/// the URL, so each message adds ~500 chars. 10 keeps us under the ~8KB URL
/// limit that most HTTP servers enforce.
const COSMOS_BATCH_SIZE: usize = 10;
/// Solana's getMultipleAccounts supports up to 100 accounts per call.
const SOLANA_BATCH_SIZE: usize = 100;
/// Max concurrent batch requests per phase per poll cycle.
#[allow(dead_code)]
const MAX_BATCH_CONCURRENCY: usize = 10;

/// Batch-check VotingVerifier `messages_status` for multiple messages.
/// Returns a `Vec<bool>` aligned with the input `txs` slice — `true` = succeeded.
#[allow(dead_code)]
async fn batch_check_voting_verifier(
    lcd: &str,
    voting_verifier: &str,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    txs: &[(usize, &PendingTx)],
) -> Vec<(usize, bool)> {
    let mut results = Vec::with_capacity(txs.len());
    for chunk in txs.chunks(COSMOS_BATCH_SIZE) {
        let messages: Vec<_> = chunk
            .iter()
            .map(|(_, tx)| {
                json!({
                    "cc_id": {
                        "source_chain": source_chain,
                        "message_id": tx.message_id,
                    },
                    "source_address": tx.source_address,
                    "destination_chain": destination_chain,
                    "destination_address": destination_address,
                    "payload_hash": tx.payload_hash_hex,
                })
            })
            .collect();
        let query = json!({ "messages_status": messages });
        match lcd_cosmwasm_smart_query(lcd, voting_verifier, &query).await {
            Ok(resp) => {
                // Response is an array of {message, status} objects.
                if let Some(arr) = resp.as_array() {
                    for (j, item) in arr.iter().enumerate() {
                        if j < chunk.len() {
                            let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                            results.push((chunk[j].0, status.to_lowercase().contains("succeeded")));
                        }
                    }
                } else {
                    // Fallback: treat entire response as single check.
                    let s = serde_json::to_string(&resp).unwrap_or_default();
                    let ok = s.to_lowercase().contains("succeeded");
                    for (idx, _) in chunk {
                        results.push((*idx, ok));
                    }
                }
            }
            Err(_) => {
                // On error, mark all as not-yet so they're retried next cycle.
                for (idx, _) in chunk {
                    results.push((*idx, false));
                }
            }
        }
    }
    results
}

/// Batch-check Cosmos Gateway `outgoing_messages` for multiple messages.
#[allow(dead_code)]
async fn batch_check_cosmos_routed(
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
    txs: &[(usize, &PendingTx)],
) -> Vec<(usize, bool)> {
    let mut results = Vec::with_capacity(txs.len());
    for chunk in txs.chunks(COSMOS_BATCH_SIZE) {
        let cc_ids: Vec<_> = chunk
            .iter()
            .map(|(_, tx)| {
                json!({
                    "source_chain": source_chain,
                    "message_id": tx.message_id,
                })
            })
            .collect();
        let query = json!({ "outgoing_messages": cc_ids });
        match lcd_cosmwasm_smart_query(lcd, cosm_gateway, &query).await {
            Ok(resp) => {
                if let Some(arr) = resp.as_array() {
                    for (j, item) in arr.iter().enumerate() {
                        if j < chunk.len() {
                            results.push((chunk[j].0, !item.is_null()));
                        }
                    }
                } else {
                    for (idx, _) in chunk {
                        results.push((*idx, false));
                    }
                }
            }
            Err(_) => {
                for (idx, _) in chunk {
                    results.push((*idx, false));
                }
            }
        }
    }
    results
}

/// Batch-check AxelarnetGateway `executable_messages` for multiple messages.
#[allow(dead_code)]
async fn batch_check_hub_approved(
    lcd: &str,
    axelarnet_gateway: &str,
    source_chain: &str,
    txs: &[(usize, &PendingTx)],
) -> Vec<(usize, bool)> {
    let mut results = Vec::with_capacity(txs.len());
    for chunk in txs.chunks(COSMOS_BATCH_SIZE) {
        let cc_ids: Vec<_> = chunk
            .iter()
            .map(|(_, tx)| {
                json!({
                    "source_chain": source_chain,
                    "message_id": tx.message_id,
                })
            })
            .collect();
        let query = json!({ "executable_messages": { "cc_ids": cc_ids } });
        match lcd_cosmwasm_smart_query(lcd, axelarnet_gateway, &query).await {
            Ok(resp) => {
                if let Some(arr) = resp.as_array() {
                    for (j, item) in arr.iter().enumerate() {
                        if j < chunk.len() {
                            results.push((chunk[j].0, !item.is_null()));
                        }
                    }
                } else {
                    for (idx, _) in chunk {
                        results.push((*idx, false));
                    }
                }
            }
            Err(_) => {
                for (idx, _) in chunk {
                    results.push((*idx, false));
                }
            }
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Owned-data batch helpers — chunks run concurrently via join_all
// ---------------------------------------------------------------------------

/// Batch VotingVerifier check with owned data and concurrent chunks.
async fn batch_check_voting_verifier_owned(
    lcd: &str,
    voting_verifier: &str,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    txs: &[(usize, String, String, String)], // (idx, message_id, source_address, payload_hash_hex)
) -> Vec<(usize, bool)> {
    let futs: Vec<_> = txs
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let messages: Vec<_> = chunk
                .iter()
                .map(|(_, msg_id, src_addr, ph)| {
                    json!({
                        "cc_id": { "source_chain": source_chain, "message_id": msg_id },
                        "source_address": src_addr,
                        "destination_chain": destination_chain,
                        "destination_address": destination_address,
                        "payload_hash": ph,
                    })
                })
                .collect();
            let query = json!({ "messages_status": messages });
            let mut out = Vec::with_capacity(chunk.len());
            match lcd_cosmwasm_smart_query(lcd, voting_verifier, &query).await {
                Ok(resp) => {
                    if let Some(arr) = resp.as_array() {
                        for (j, item) in arr.iter().enumerate() {
                            if j < chunk.len() {
                                let s = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                                out.push((chunk[j].0, s.to_lowercase().contains("succeeded")));
                            }
                        }
                    } else {
                        let s = serde_json::to_string(&resp).unwrap_or_default();
                        let ok = s.to_lowercase().contains("succeeded");
                        for (idx, ..) in chunk {
                            out.push((*idx, ok));
                        }
                    }
                }
                Err(_) => {
                    for (idx, ..) in chunk {
                        out.push((*idx, false));
                    }
                }
            }
            out
        })
        .collect();
    futures::future::join_all(futs)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Batch Cosmos Gateway routed check with owned data and concurrent chunks.
async fn batch_check_cosmos_routed_owned(
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
    txs: &[(usize, String)], // (idx, message_id)
) -> Vec<(usize, bool)> {
    let futs: Vec<_> = txs
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let cc_ids: Vec<_> = chunk
                .iter()
                .map(|(_, msg_id)| json!({ "source_chain": source_chain, "message_id": msg_id }))
                .collect();
            let query = json!({ "outgoing_messages": cc_ids });
            let mut out = Vec::with_capacity(chunk.len());
            match lcd_cosmwasm_smart_query(lcd, cosm_gateway, &query).await {
                Ok(resp) => {
                    if let Some(arr) = resp.as_array() {
                        for (j, item) in arr.iter().enumerate() {
                            if j < chunk.len() {
                                out.push((chunk[j].0, !item.is_null()));
                            }
                        }
                    } else {
                        for (idx, _) in chunk {
                            out.push((*idx, false));
                        }
                    }
                }
                Err(_) => {
                    for (idx, _) in chunk {
                        out.push((*idx, false));
                    }
                }
            }
            out
        })
        .collect();
    futures::future::join_all(futs)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Batch AxelarnetGateway hub-approved check with owned data and concurrent chunks.
async fn batch_check_hub_approved_owned(
    lcd: &str,
    axelarnet_gateway: &str,
    source_chain: &str,
    txs: &[(usize, String)], // (idx, message_id)
) -> Vec<(usize, bool)> {
    let futs: Vec<_> = txs
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let cc_ids: Vec<_> = chunk
                .iter()
                .map(|(_, msg_id)| json!({ "source_chain": source_chain, "message_id": msg_id }))
                .collect();
            let query = json!({ "executable_messages": { "cc_ids": cc_ids } });
            let mut out = Vec::with_capacity(chunk.len());
            match lcd_cosmwasm_smart_query(lcd, axelarnet_gateway, &query).await {
                Ok(resp) => {
                    if let Some(arr) = resp.as_array() {
                        for (j, item) in arr.iter().enumerate() {
                            if j < chunk.len() {
                                out.push((chunk[j].0, !item.is_null()));
                            }
                        }
                    } else {
                        for (idx, _) in chunk {
                            out.push((*idx, false));
                        }
                    }
                }
                Err(_) => {
                    for (idx, _) in chunk {
                        out.push((*idx, false));
                    }
                }
            }
            out
        })
        .collect();
    futures::future::join_all(futs)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Batch-check Solana incoming message PDAs via `getMultipleAccounts`.
/// Returns `(tx_index, Option<status_byte>)` for each tx.
fn batch_check_solana_incoming_messages(
    rpc_client: &solana_client::rpc_client::RpcClient,
    txs: &[(usize, [u8; 32])], // (tx_index, command_id)
) -> Vec<(usize, Option<u8>)> {
    let mut results = Vec::with_capacity(txs.len());
    for chunk in txs.chunks(SOLANA_BATCH_SIZE) {
        let pubkeys: Vec<Pubkey> = chunk
            .iter()
            .map(|(_, cmd_id)| {
                Pubkey::find_program_address(
                    &[b"incoming message", cmd_id],
                    &solana_axelar_gateway::id(),
                )
                .0
            })
            .collect();
        match rpc_client.get_multiple_accounts(&pubkeys) {
            Ok(accounts) => {
                for (j, maybe_account) in accounts.iter().enumerate() {
                    if j < chunk.len() {
                        let status = maybe_account.as_ref().and_then(|acc| {
                            if acc.data.len() > INCOMING_MESSAGE_STATUS_OFFSET {
                                Some(acc.data[INCOMING_MESSAGE_STATUS_OFFSET])
                            } else {
                                None
                            }
                        });
                        results.push((chunk[j].0, status));
                    }
                }
            }
            Err(_) => {
                for (idx, _) in chunk {
                    results.push((*idx, None));
                }
            }
        }
    }
    results
}

/// Check `isMessageApproved` on the EVM gateway (single attempt).
async fn check_evm_is_message_approved<P: Provider>(
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

// ---------------------------------------------------------------------------
// Solana IncomingMessage PDA check
// ---------------------------------------------------------------------------

/// Incoming message account data offset for the status byte.
/// Layout: 8 (discriminator) + 1 (bump) + 1 (signing_pda_bump) + 3 (pad) = 13
const INCOMING_MESSAGE_STATUS_OFFSET: usize = 13;

/// Check the Solana IncomingMessage PDA for a given command_id.
/// Returns `Some(status_byte)` if the account exists, `None` otherwise.
/// Status: 0 = approved, non-zero = executed.
fn check_solana_incoming_message(
    rpc_client: &solana_client::rpc_client::RpcClient,
    command_id: &[u8; 32],
) -> Result<Option<u8>> {
    let (pda, _bump) = Pubkey::find_program_address(
        &[b"incoming message", command_id],
        &solana_axelar_gateway::id(),
    );

    match rpc_client.get_account_data(&pda) {
        Ok(data) => {
            if data.len() <= INCOMING_MESSAGE_STATUS_OFFSET {
                return Err(eyre::eyre!(
                    "IncomingMessage account too small: {} bytes",
                    data.len()
                ));
            }
            Ok(Some(data[INCOMING_MESSAGE_STATUS_OFFSET]))
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("AccountNotFound") || err_str.contains("could not find account") {
                Ok(None)
            } else {
                Err(eyre::eyre!("Solana RPC error: {e}"))
            }
        }
    }
}
