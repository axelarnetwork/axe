//! Polling pipeline machinery shared by every `verify_onchain_*` orchestrator
//! in `mod.rs`: the [`DestinationChecker`] / [`ItsHubDest`] enums that abstract
//! over destination-chain RPCs, the three [`poll_pipeline`],
//! [`poll_pipeline_its_hub`], [`poll_pipeline_its_hub_evm`] state machines that
//! drive each tx through Voted → Routed → Approved → Executed, and the
//! cosmos/EVM/Solana check helpers they call into.
//!
//! The orchestrators in `mod.rs` build [`PendingTx`] vectors, hand them to one
//! of the `poll_pipeline*` functions, and turn the resulting [`PeakThroughput`]
//! plus the populated `tx.timing` into a `VerificationReport` via
//! [`super::report::compute_verification_report`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use alloy::primitives::{Address, FixedBytes, keccak256};
use alloy::providers::Provider;
use eyre::Result;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use super::PendingTx;
use super::checks::{batch_check_solana_incoming_messages, check_evm_is_message_approved};
use super::report::compute_peak_throughput;
use super::state::{Phase, RealTimeStats, phase_counts};
use super::{INACTIVITY_TIMEOUT, POLL_INTERVAL};
use crate::commands::load_test::metrics::PeakThroughput;
use crate::cosmos::{lcd_cosmwasm_smart_query, rpc_tx_search_event};
use crate::evm::AxelarAmplifierGateway;
use crate::ui;

/// Parse a hex-encoded 32-byte payload hash, with or without the `0x`
/// prefix. Returns an error rather than silently zero-extending so a
/// truncated hash from upstream code surfaces immediately instead of
/// propagating into a downstream "wrong gateway hash" mismatch.
pub(super) fn parse_payload_hash(hex_str: &str) -> Result<FixedBytes<32>> {
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

pub(super) enum DestinationChecker<'a, P: Provider> {
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

impl<P: Provider> DestinationChecker<'_, P> {
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
// Unified polling pipeline
// ---------------------------------------------------------------------------

pub(super) struct PollPipelineArgs<'a, P: Provider> {
    pub txs: &'a mut Vec<PendingTx>,
    pub lcd: &'a str,
    pub voting_verifier: Option<&'a str>,
    pub cosm_gateway: Option<&'a str>,
    pub source_chain: &'a str,
    pub destination_chain: &'a str,
    pub destination_address: &'a str,
    pub checker: &'a DestinationChecker<'a, P>,
    pub axelarnet_gateway: Option<&'a str>,
    pub display_chain: Option<&'a str>,
    pub rx: Option<&'a mut mpsc::UnboundedReceiver<PendingTx>>,
    pub send_done: Option<&'a AtomicBool>,
    pub external_spinner: Option<indicatif::ProgressBar>,
}

#[allow(clippy::cognitive_complexity)]
pub(super) async fn poll_pipeline<P: Provider>(args: PollPipelineArgs<'_, P>) -> PeakThroughput {
    let PollPipelineArgs {
        txs,
        lcd,
        voting_verifier,
        cosm_gateway,
        source_chain,
        destination_chain,
        destination_address,
        checker,
        axelarnet_gateway,
        display_chain,
        mut rx,
        send_done,
        external_spinner,
    } = args;
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
pub(super) struct SecondLegInfo {
    pub(super) message_id: String,
    pub(super) payload_hash: String,
    pub(super) source_address: String,
    pub(super) destination_address: String,
}

/// Discover the second-leg message_id by searching for the hub execution tx
/// that consumed the first-leg message, then extracting routing event attributes.
pub(super) async fn discover_second_leg(
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

    let Some(events) = events else {
        return Ok(None);
    };

    for event in events {
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if event_type != "wasm-routing" {
            continue;
        }

        let Some(attrs) = event.get("attributes").and_then(|v| v.as_array()) else {
            continue;
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

        // `source_chain` and `destination_chain` gate that we got a hub
        // execution event but aren't consumed downstream, so they don't
        // make it into `SecondLegInfo`.
        if let (Some(msg_id), Some(_), Some(_), Some(ph)) = (
            get_attr("message_id"),
            get_attr("source_chain"),
            get_attr("destination_chain"),
            get_attr("payload_hash"),
        ) {
            return Ok(Some(SecondLegInfo {
                message_id: msg_id,
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
pub(super) enum ItsHubDest {
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

pub(super) struct PollItsHubArgs<'a> {
    pub txs: &'a mut Vec<PendingTx>,
    pub lcd: &'a str,
    pub voting_verifier: Option<&'a str>,
    pub source_chain: &'a str,
    pub axelarnet_gateway: &'a str,
    pub rpc: &'a str,
    pub cosm_gateway_dest: &'a str,
    pub dest: ItsHubDest,
    pub rx: Option<&'a mut mpsc::UnboundedReceiver<PendingTx>>,
    pub send_done: Option<&'a AtomicBool>,
    pub external_spinner: Option<indicatif::ProgressBar>,
}

/// Full ITS polling pipeline: Voted → HubApproved → DiscoverSecondLeg → Routed → Approved → Executed.
#[allow(clippy::cognitive_complexity)]
pub(super) async fn poll_pipeline_its_hub(args: PollItsHubArgs<'_>) -> PeakThroughput {
    let PollItsHubArgs {
        txs,
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        cosm_gateway_dest,
        dest,
        mut rx,
        send_done,
        external_spinner,
    } = args;
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
                            let Some(second_leg) = txs[i].second_leg_message_id.as_deref() else {
                                continue;
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
                            let Some(second_leg) = txs[i].second_leg_message_id.as_deref() else {
                                continue;
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

pub(super) struct PollItsHubEvmArgs<'a, P: Provider> {
    pub txs: &'a mut Vec<PendingTx>,
    pub lcd: &'a str,
    pub voting_verifier: Option<&'a str>,
    pub source_chain: &'a str,
    pub axelarnet_gateway: &'a str,
    pub rpc: &'a str,
    pub cosm_gateway_dest: &'a str,
    pub gw_contract: &'a AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&'a P>,
    pub _destination_chain: &'a str,
    pub rx: Option<&'a mut mpsc::UnboundedReceiver<PendingTx>>,
    pub send_done: Option<&'a AtomicBool>,
    pub external_spinner: Option<indicatif::ProgressBar>,
}

/// Full ITS polling pipeline with EVM destination (batch + streaming):
/// Voted → HubApproved → DiscoverSecondLeg → Routed → Approved(EVM) → Executed(EVM).
#[allow(clippy::cognitive_complexity)]
pub(super) async fn poll_pipeline_its_hub_evm<P: Provider>(
    args: PollItsHubEvmArgs<'_, P>,
) -> PeakThroughput {
    let PollItsHubEvmArgs {
        txs,
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        cosm_gateway_dest,
        gw_contract,
        _destination_chain,
        mut rx,
        send_done,
        external_spinner,
    } = args;
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
pub(super) async fn check_cosmos_routed(
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
pub(super) async fn check_hub_approved(
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
