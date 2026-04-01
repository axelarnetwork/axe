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

use super::metrics::{
    AmplifierTiming, FailureCategory, PeakThroughput, TxMetrics, VerificationReport,
};
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

// ---------------------------------------------------------------------------
// Real-time stats (throughput + latency) for spinner display
// ---------------------------------------------------------------------------

struct RealTimeStats {
    snapshot_time: Instant,
    snapshot_counts: [usize; 5], // voted, routed, hub_approved, approved, executed
    throughputs: [Option<f64>; 5],
    latencies: Vec<f64>, // sorted executed_secs for completed txs
}

impl RealTimeStats {
    fn new() -> Self {
        Self {
            snapshot_time: Instant::now(),
            snapshot_counts: [0; 5],
            throughputs: [None; 5],
            latencies: Vec::new(),
        }
    }

    /// Update throughputs every THROUGHPUT_WINDOW and collect new latencies.
    #[allow(clippy::float_arithmetic)]
    fn update(&mut self, counts: [usize; 5], txs: &[PendingTx]) {
        let elapsed = self.snapshot_time.elapsed();
        if elapsed >= THROUGHPUT_WINDOW {
            let secs = elapsed.as_secs_f64();
            for (i, &count) in counts.iter().enumerate() {
                let delta = count.saturating_sub(self.snapshot_counts[i]);
                self.throughputs[i] = if delta > 0 {
                    Some(delta as f64 / secs)
                } else {
                    self.throughputs[i] // keep last known value
                };
            }
            self.snapshot_counts = counts;
            self.snapshot_time = Instant::now();
        }

        // Rebuild latencies from all completed txs (simple and correct).
        let new_len = txs
            .iter()
            .filter(|t| t.timing.executed_secs.is_some())
            .count();
        if new_len != self.latencies.len() {
            self.latencies.clear();
            for tx in txs {
                if let Some(secs) = tx.timing.executed_secs {
                    self.latencies.push(secs);
                }
            }
            self.latencies
                .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }
    }

    /// Format a single phase: "450/600(4.2/s)" or "450/600" if no throughput yet.
    fn fmt_phase(count: usize, total: usize, tps: Option<f64>) -> String {
        match tps {
            Some(t) => format!("{count}/{total}({t:.1}/s)"),
            None => format!("{count}/{total}"),
        }
    }

    /// Format latency summary: "e2e: avg 94.5s p50 92.1s p75 96.3s p99 102.1s"
    #[allow(clippy::float_arithmetic)]
    fn fmt_latency(&self) -> String {
        let n = self.latencies.len();
        if n == 0 {
            return String::new();
        }
        let sum: f64 = self.latencies.iter().sum();
        let avg = sum / n as f64;
        let pct = |p: f64| -> f64 {
            let idx = ((n as f64 * p) as usize).min(n - 1);
            self.latencies[idx]
        };
        let min = self.latencies[0];
        let max = self.latencies[n - 1];
        format!(
            " | e2e: avg {avg:.1}s p50 {:.1}s p75 {:.1}s p99 {:.1}s min {min:.1}s max {max:.1}s",
            pct(0.50),
            pct(0.75),
            pct(0.99),
        )
    }

    /// Build the full spinner message for GMP (no hub phase).
    fn spinner_msg_gmp(
        &self,
        counts: [usize; 5],
        total: usize,
        err: Option<&str>,
        has_voting_verifier: bool,
    ) -> String {
        let [voted, routed, _, approved, executed] = counts;
        let [tv, tr, _, ta, te] = self.throughputs;
        let mut parts = Vec::new();
        if has_voting_verifier {
            parts.push(format!("voted: {}", Self::fmt_phase(voted, total, tv)));
        }
        parts.push(format!("routed: {}", Self::fmt_phase(routed, total, tr)));
        parts.push(format!(
            "approved: {}",
            Self::fmt_phase(approved, total, ta)
        ));
        parts.push(format!(
            "executed: {}",
            Self::fmt_phase(executed, total, te)
        ));
        let mut msg = parts.join("  ");
        msg.push_str(&self.fmt_latency());
        if let Some(e) = err {
            msg.push_str(&format!("  (err: {e})"));
        }
        msg
    }

    /// Build the full spinner message for ITS (with hub phase).
    fn spinner_msg_its(&self, counts: [usize; 5], total: usize, err: Option<&str>) -> String {
        let [voted, routed, hub, approved, executed] = counts;
        let [tv, tr, th, ta, te] = self.throughputs;
        let mut msg = format!(
            "voted: {}  hub: {}  routed: {}  approved: {}  executed: {}",
            Self::fmt_phase(voted, total, tv),
            Self::fmt_phase(hub, total, th),
            Self::fmt_phase(routed, total, tr),
            Self::fmt_phase(approved, total, ta),
            Self::fmt_phase(executed, total, te),
        );
        msg.push_str(&self.fmt_latency());
        if let Some(e) = err {
            msg.push_str(&format!("  (err: {e})"));
        }
        msg
    }
}

// ---------------------------------------------------------------------------
// Phase tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Voted,
    Routed,
    HubApproved,
    DiscoverSecondLeg,
    Approved,
    Executed,
    Done,
}

enum ApprovalResult {
    Approved,
    AlreadyExecuted,
    NotYet,
}

// ---------------------------------------------------------------------------
// Per-tx state
// ---------------------------------------------------------------------------

/// Per-tx state tracked during batch verification.
pub(super) struct PendingTx {
    idx: usize,
    message_id: String,
    send_instant: Instant,
    source_address: String,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
    payload_hash_hex: String,
    /// Pre-computed command ID for Solana destination checks.
    command_id: Option<[u8; 32]>,
    /// GMP-level destination chain from ContractCall event (e.g. "axelar" for ITS).
    gmp_destination_chain: String,
    /// GMP-level destination address from ContractCall event (e.g. ITS Hub contract).
    gmp_destination_address: String,
    timing: AmplifierTiming,
    failed: bool,
    fail_reason: Option<String>,
    phase: Phase,
    /// Second-leg message_id discovered from hub execution tx (ITS only).
    second_leg_message_id: Option<String>,
    /// Second-leg payload_hash discovered from hub execution tx (ITS only).
    second_leg_payload_hash: Option<String>,
    /// Second-leg source_address (e.g. ITS Hub contract on Axelar).
    second_leg_source_address: Option<String>,
    /// Second-leg destination_address (e.g. ITS proxy on destination chain).
    second_leg_destination_address: Option<String>,
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
}

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
        }
    }

    fn approval_label(&self) -> &str {
        match self {
            Self::Evm { .. } => "EVM approval",
            Self::Solana { .. } => "Solana approval",
        }
    }

    fn execution_label(&self) -> &str {
        match self {
            Self::Evm { .. } => "EVM execution",
            Self::Solana { .. } => "Solana execution",
        }
    }
}

// ---------------------------------------------------------------------------
// Check outcome — returned from parallel checks, applied to txs afterward
// ---------------------------------------------------------------------------

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

#[allow(clippy::too_many_arguments)]
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

    loop {
        // Drain any newly-confirmed txs from the streaming channel.
        if let Some(ref mut receiver) = rx {
            while let Ok(new_tx) = receiver.try_recv() {
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

        // Fire checks with bounded concurrency to avoid overwhelming the RPC.
        let futs: Vec<_> = active
            .iter()
            .map(|&i| {
                // Extract data needed for the check (avoids borrowing txs during await)
                let phase = txs[i].phase;
                let message_id = txs[i].message_id.clone();
                let source_address = txs[i].source_address.clone();
                let contract_addr = txs[i].contract_addr;
                let payload_hash = txs[i].payload_hash;
                let payload_hash_hex = txs[i].payload_hash_hex.clone();
                let send_instant = txs[i].send_instant;
                let command_id = txs[i].command_id;
                let axelarnet_gw = axelarnet_gateway.map(|s| s.to_string());

                async move {
                    let outcome = match phase {
                        Phase::Voted => {
                            if let Some(vv) = voting_verifier {
                                match check_voting_verifier(
                                    lcd,
                                    vv,
                                    source_chain,
                                    &message_id,
                                    &source_address,
                                    destination_chain,
                                    destination_address,
                                    &payload_hash_hex,
                                )
                                .await
                                {
                                    Ok(true) => CheckOutcome::PhaseComplete {
                                        elapsed: send_instant.elapsed().as_secs_f64(),
                                    },
                                    Ok(false) => CheckOutcome::NotYet,
                                    Err(e) => CheckOutcome::Error(format!("VotingVerifier: {e}")),
                                }
                            } else {
                                CheckOutcome::SkipVoting
                            }
                        }
                        Phase::Routed => {
                            if let Some(gw) = cosm_gateway {
                                match check_cosmos_routed(lcd, gw, source_chain, &message_id).await
                                {
                                    Ok(true) => CheckOutcome::PhaseComplete {
                                        elapsed: send_instant.elapsed().as_secs_f64(),
                                    },
                                    Ok(false) => CheckOutcome::NotYet,
                                    Err(e) => CheckOutcome::Error(format!("Gateway: {e}")),
                                }
                            } else {
                                // No cosmos gateway to query — skip Routed phase
                                CheckOutcome::SkipVoting
                            }
                        }
                        Phase::HubApproved => {
                            if let Some(ref gw) = axelarnet_gw {
                                match check_hub_approved(lcd, gw, source_chain, &message_id).await {
                                    Ok(true) => CheckOutcome::PhaseComplete {
                                        elapsed: send_instant.elapsed().as_secs_f64(),
                                    },
                                    Ok(false) => CheckOutcome::NotYet,
                                    Err(e) => CheckOutcome::Error(format!("AxelarnetGateway: {e}")),
                                }
                            } else {
                                // No hub gateway — skip this phase
                                CheckOutcome::SkipVoting
                            }
                        }
                        Phase::DiscoverSecondLeg => CheckOutcome::NotYet,
                        Phase::Approved => {
                            // Build a temporary PendingTx-like view for the checker
                            let tmp = PendingTx {
                                idx: 0,
                                message_id: message_id.clone(),
                                send_instant,
                                source_address,
                                contract_addr,
                                payload_hash,
                                payload_hash_hex,
                                command_id,
                                gmp_destination_chain: String::new(),
                                gmp_destination_address: String::new(),
                                timing: AmplifierTiming::default(),
                                failed: false,
                                fail_reason: None,
                                phase,
                                second_leg_message_id: None,
                                second_leg_payload_hash: None,
                                second_leg_source_address: None,
                                second_leg_destination_address: None,
                            };
                            match checker.check_approved(&tmp, i, source_chain).await {
                                Ok(ApprovalResult::Approved) => CheckOutcome::PhaseComplete {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(ApprovalResult::AlreadyExecuted) => {
                                    CheckOutcome::AlreadyExecuted {
                                        elapsed: send_instant.elapsed().as_secs_f64(),
                                    }
                                }
                                Ok(ApprovalResult::NotYet) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!(
                                    "{}: {e}",
                                    checker.approval_label()
                                )),
                            }
                        }
                        Phase::Executed => {
                            let tmp = PendingTx {
                                idx: 0,
                                message_id: message_id.clone(),
                                send_instant,
                                source_address,
                                contract_addr,
                                payload_hash,
                                payload_hash_hex,
                                command_id,
                                gmp_destination_chain: String::new(),
                                gmp_destination_address: String::new(),
                                timing: AmplifierTiming::default(),
                                failed: false,
                                fail_reason: None,
                                phase,
                                second_leg_message_id: None,
                                second_leg_payload_hash: None,
                                second_leg_source_address: None,
                                second_leg_destination_address: None,
                            };
                            match checker.check_executed(&tmp, i, source_chain).await {
                                Ok(true) => CheckOutcome::PhaseComplete {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(false) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!(
                                    "{}: {e}",
                                    checker.execution_label()
                                )),
                            }
                        }
                        Phase::Done => CheckOutcome::NotYet,
                    };
                    (i, outcome)
                }
            })
            .collect();

        // Cap at 20 concurrent RPC calls to avoid overwhelming the endpoint.
        let results: Vec<_> = futures::stream::iter(futs)
            .buffer_unordered(20)
            .collect()
            .await;

        // Apply results back to txs
        let mut error_msg = None;
        for (i, outcome) in results {
            match outcome {
                CheckOutcome::NotYet => {}
                CheckOutcome::PhaseComplete { elapsed } => {
                    match txs[i].phase {
                        Phase::Voted => {
                            txs[i].timing.voted_secs = Some(elapsed);
                            txs[i].phase = Phase::Routed;
                        }
                        Phase::Routed => {
                            txs[i].timing.routed_secs = Some(elapsed);
                            txs[i].phase = if axelarnet_gateway.is_some() {
                                Phase::HubApproved
                            } else {
                                Phase::Approved
                            };
                        }
                        Phase::HubApproved => {
                            txs[i].timing.hub_approved_secs = Some(elapsed);
                            txs[i].phase = Phase::Approved;
                        }
                        Phase::DiscoverSecondLeg => {
                            // Not used in GMP pipeline
                        }
                        Phase::Approved => {
                            txs[i].timing.approved_secs = Some(elapsed);
                            txs[i].phase = Phase::Executed;
                        }
                        Phase::Executed => {
                            txs[i].timing.executed_secs = Some(elapsed);
                            txs[i].timing.executed_ok = Some(true);
                            txs[i].phase = Phase::Done;
                        }
                        Phase::Done => {}
                    }
                    last_progress = Instant::now();
                }
                CheckOutcome::SkipVoting => {
                    // Skip current phase — advance to next
                    txs[i].phase = match txs[i].phase {
                        Phase::Voted => Phase::Routed,
                        Phase::Routed => {
                            if axelarnet_gateway.is_some() {
                                Phase::HubApproved
                            } else {
                                Phase::Approved
                            }
                        }
                        Phase::HubApproved => Phase::Approved,
                        // SkipVoting should never fire for later phases; if it
                        // does, leave the phase unchanged to avoid looping back.
                        other => other,
                    };
                    last_progress = Instant::now();
                }
                CheckOutcome::SecondLegDiscovered { .. } => {
                    // Not used in GMP pipeline
                }
                CheckOutcome::AlreadyExecuted { elapsed } => {
                    if txs[i].timing.approved_secs.is_none() {
                        txs[i].timing.approved_secs = Some(elapsed);
                    }
                    txs[i].timing.executed_secs = Some(elapsed);
                    txs[i].timing.executed_ok = Some(true);
                    txs[i].phase = Phase::Done;
                    last_progress = Instant::now();
                }
                CheckOutcome::Error(msg) => {
                    error_msg = Some(msg);
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

/// Full ITS polling pipeline: Voted → HubApproved → DiscoverSecondLeg → Routed → Approved → Executed.
#[allow(clippy::too_many_arguments)]
async fn poll_pipeline_its_hub(
    txs: &mut [PendingTx],
    lcd: &str,
    voting_verifier: Option<&str>,
    source_chain: &str,
    axelarnet_gateway: &str,
    rpc: &str,
    cosm_gateway_dest: &str,
    solana_rpc: &str,
) {
    let total = txs.len();
    if total == 0 {
        return;
    }

    let sol_rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::confirmed(),
    ));

    let spinner = ui::wait_spinner("verifying ITS pipeline (starting)...");
    let mut last_progress = Instant::now();
    let mut rt_stats = RealTimeStats::new();

    loop {
        let active: Vec<usize> = (0..txs.len())
            .filter(|&i| !txs[i].failed && txs[i].phase != Phase::Done)
            .collect();

        if active.is_empty() {
            break;
        }

        let futs: Vec<_> = active
            .iter()
            .map(|&i| {
                let phase = txs[i].phase;
                let message_id = txs[i].message_id.clone();
                let source_address = txs[i].source_address.clone();
                let payload_hash_hex = txs[i].payload_hash_hex.clone();
                let send_instant = txs[i].send_instant;
                let dest_chain = txs[i].gmp_destination_chain.clone();
                let dest_address = txs[i].gmp_destination_address.clone();
                let second_leg_id = txs[i].second_leg_message_id.clone();
                let sol_client = sol_rpc_client.clone();

                async move {
                    let outcome = match phase {
                        Phase::Voted => {
                            if let Some(vv) = voting_verifier {
                                match check_voting_verifier(
                                    lcd,
                                    vv,
                                    source_chain,
                                    &message_id,
                                    &source_address,
                                    &dest_chain,
                                    &dest_address,
                                    &payload_hash_hex,
                                )
                                .await
                                {
                                    Ok(true) => CheckOutcome::PhaseComplete {
                                        elapsed: send_instant.elapsed().as_secs_f64(),
                                    },
                                    Ok(false) => CheckOutcome::NotYet,
                                    Err(e) => CheckOutcome::Error(format!("VotingVerifier: {e}")),
                                }
                            } else {
                                CheckOutcome::SkipVoting
                            }
                        }
                        Phase::HubApproved => {
                            match check_hub_approved(
                                lcd,
                                axelarnet_gateway,
                                source_chain,
                                &message_id,
                            )
                            .await
                            {
                                Ok(true) => CheckOutcome::PhaseComplete {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(false) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!("AxelarnetGateway: {e}")),
                            }
                        }
                        Phase::DiscoverSecondLeg => {
                            match discover_second_leg(rpc, &message_id).await {
                                Ok(Some(info)) => CheckOutcome::SecondLegDiscovered {
                                    message_id: info.message_id,
                                    payload_hash: info.payload_hash,
                                    source_address: info.source_address,
                                    destination_address: info.destination_address,
                                },
                                Ok(None) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!("second-leg discovery: {e}")),
                            }
                        }
                        Phase::Routed => {
                            let sl_id = second_leg_id.as_deref().unwrap_or("");
                            match check_cosmos_routed(lcd, cosm_gateway_dest, "axelar", sl_id).await
                            {
                                Ok(true) => CheckOutcome::PhaseComplete {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(false) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!("Gateway routing: {e}")),
                            }
                        }
                        Phase::Approved => {
                            let sl_id = second_leg_id.as_deref().unwrap_or("");
                            let input = [b"axelar-".as_slice(), sl_id.as_bytes()].concat();
                            let cmd_id: [u8; 32] = keccak256(&input).into();
                            let client = sol_client;
                            match tokio::task::spawn_blocking(move || {
                                check_solana_incoming_message(&client, &cmd_id)
                            })
                            .await
                            {
                                Ok(Ok(Some(0))) => CheckOutcome::PhaseComplete {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(Ok(Some(_))) => CheckOutcome::AlreadyExecuted {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(Ok(None)) => CheckOutcome::NotYet,
                                Ok(Err(e)) => CheckOutcome::Error(format!("Solana approval: {e}")),
                                Err(e) => CheckOutcome::Error(format!("Solana approval: {e}")),
                            }
                        }
                        Phase::Executed => {
                            let sl_id = second_leg_id.as_deref().unwrap_or("");
                            let input = [b"axelar-".as_slice(), sl_id.as_bytes()].concat();
                            let cmd_id: [u8; 32] = keccak256(&input).into();
                            let client = sol_client;
                            match tokio::task::spawn_blocking(move || {
                                check_solana_incoming_message(&client, &cmd_id)
                            })
                            .await
                            {
                                Ok(Ok(Some(status))) if status != 0 => {
                                    CheckOutcome::PhaseComplete {
                                        elapsed: send_instant.elapsed().as_secs_f64(),
                                    }
                                }
                                Ok(Ok(_)) => CheckOutcome::NotYet,
                                Ok(Err(e)) => CheckOutcome::Error(format!("Solana execution: {e}")),
                                Err(e) => CheckOutcome::Error(format!("Solana execution: {e}")),
                            }
                        }
                        Phase::Done => CheckOutcome::NotYet,
                    };
                    (i, outcome)
                }
            })
            .collect();

        let results: Vec<_> = futures::stream::iter(futs)
            .buffer_unordered(20)
            .collect()
            .await;

        let mut error_msg = None;
        for (i, outcome) in &results {
            let i = *i;
            match outcome {
                CheckOutcome::NotYet => {}
                CheckOutcome::PhaseComplete { elapsed } => {
                    let elapsed = *elapsed;
                    match txs[i].phase {
                        Phase::Voted => {
                            txs[i].timing.voted_secs = Some(elapsed);
                            // Skip Routed, go directly to HubApproved
                            txs[i].phase = Phase::HubApproved;
                        }
                        Phase::HubApproved => {
                            txs[i].timing.hub_approved_secs = Some(elapsed);
                            txs[i].phase = Phase::DiscoverSecondLeg;
                        }
                        Phase::DiscoverSecondLeg => {
                            // Should not happen — DiscoverSecondLeg uses SecondLegDiscovered
                            txs[i].phase = Phase::Routed;
                        }
                        Phase::Routed => {
                            txs[i].timing.routed_secs = Some(elapsed);
                            txs[i].phase = Phase::Approved;
                        }
                        Phase::Approved => {
                            txs[i].timing.approved_secs = Some(elapsed);
                            txs[i].phase = Phase::Executed;
                        }
                        Phase::Executed => {
                            txs[i].timing.executed_secs = Some(elapsed);
                            txs[i].timing.executed_ok = Some(true);
                            txs[i].phase = Phase::Done;
                        }
                        Phase::Done => {}
                    }
                    last_progress = Instant::now();
                }
                CheckOutcome::SkipVoting => {
                    txs[i].phase = Phase::HubApproved;
                    last_progress = Instant::now();
                }
                CheckOutcome::SecondLegDiscovered {
                    message_id: sl_msg_id,
                    payload_hash: sl_ph,
                    source_address: sl_src,
                    destination_address: sl_dst,
                } => {
                    txs[i].second_leg_message_id = Some(sl_msg_id.clone());
                    txs[i].second_leg_payload_hash = Some(sl_ph.clone());
                    txs[i].second_leg_source_address = Some(sl_src.clone());
                    txs[i].second_leg_destination_address = Some(sl_dst.clone());
                    txs[i].phase = Phase::Routed;
                    last_progress = Instant::now();
                }
                CheckOutcome::AlreadyExecuted { elapsed } => {
                    let elapsed = *elapsed;
                    if txs[i].timing.approved_secs.is_none() {
                        txs[i].timing.approved_secs = Some(elapsed);
                    }
                    txs[i].timing.executed_secs = Some(elapsed);
                    txs[i].timing.executed_ok = Some(true);
                    txs[i].phase = Phase::Done;
                    last_progress = Instant::now();
                }
                CheckOutcome::Error(msg) => {
                    error_msg = Some(msg.clone());
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

        if last_progress.elapsed() >= INACTIVITY_TIMEOUT {
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
}

/// Count how many txs have reached each phase (cumulative).
fn phase_counts(txs: &[PendingTx]) -> (usize, usize, usize, usize, usize) {
    let mut voted = 0;
    let mut routed = 0;
    let mut hub_approved = 0;
    let mut approved = 0;
    let mut executed = 0;
    for tx in txs {
        if tx.timing.voted_secs.is_some() {
            voted += 1;
        }
        if tx.timing.routed_secs.is_some() {
            routed += 1;
        }
        if tx.timing.hub_approved_secs.is_some() {
            hub_approved += 1;
        }
        if tx.timing.approved_secs.is_some() {
            approved += 1;
        }
        if tx.timing.executed_secs.is_some() {
            executed += 1;
        }
    }
    (voted, routed, hub_approved, approved, executed)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Source chain type — determines how message IDs are constructed.
#[derive(Clone, Copy)]
pub enum SourceChainType {
    /// Solana source: message ID = `{signature}-{group}.{index}`
    Svm,
    /// EVM source: message ID = `{tx_hash}-{event_index}` (already in tx.signature)
    Evm,
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
                    SourceChainType::Evm => tx.signature.clone(),
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
        SourceChainType::Evm => tx.signature.clone(),
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
        solana_commitment_config::CommitmentConfig::confirmed(),
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
                SourceChainType::Evm => tx.signature.clone(),
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
        solana_commitment_config::CommitmentConfig::confirmed(),
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

    poll_pipeline_its_hub(
        &mut txs,
        &lcd,
        voting_verifier.as_deref(),
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        solana_rpc,
    )
    .await;

    let peaks = compute_peak_throughput(&txs);
    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
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

    poll_pipeline_its_hub_evm(
        &mut txs,
        &lcd,
        None, // skip VotingVerifier — no payload_hash for Solana ITS
        source_chain,
        &axelarnet_gateway,
        &rpc,
        &cosm_gateway_dest,
        &gw_contract,
        destination_chain,
    )
    .await;

    let peaks = compute_peak_throughput(&txs);
    let report = compute_verification_report(&txs, metrics, peaks);
    Ok(report)
}

/// Full ITS polling pipeline with EVM destination:
/// Voted → HubApproved → DiscoverSecondLeg → Routed → Approved(EVM) → Executed(EVM).
#[allow(clippy::too_many_arguments)]
async fn poll_pipeline_its_hub_evm<P: Provider>(
    txs: &mut [PendingTx],
    lcd: &str,
    voting_verifier: Option<&str>,
    source_chain: &str,
    axelarnet_gateway: &str,
    rpc: &str,
    cosm_gateway_dest: &str,
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    _destination_chain: &str,
) {
    let total = txs.len();
    if total == 0 {
        return;
    }

    let spinner = ui::wait_spinner("verifying ITS pipeline (starting)...");
    let mut last_progress = Instant::now();
    let mut rt_stats = RealTimeStats::new();

    loop {
        let active: Vec<usize> = (0..txs.len())
            .filter(|&i| !txs[i].failed && txs[i].phase != Phase::Done)
            .collect();

        if active.is_empty() {
            break;
        }

        let futs: Vec<_> = active
            .iter()
            .map(|&i| {
                let phase = txs[i].phase;
                let message_id = txs[i].message_id.clone();
                let source_address = txs[i].source_address.clone();
                let payload_hash_hex = txs[i].payload_hash_hex.clone();
                let send_instant = txs[i].send_instant;
                let dest_chain = txs[i].gmp_destination_chain.clone();
                let dest_address = txs[i].gmp_destination_address.clone();
                let second_leg_id = txs[i].second_leg_message_id.clone();
                let second_leg_ph = txs[i].second_leg_payload_hash.clone();
                let second_leg_src = txs[i].second_leg_source_address.clone();
                let second_leg_dst = txs[i].second_leg_destination_address.clone();

                async move {
                    let outcome = match phase {
                        Phase::Voted => {
                            if let Some(vv) = voting_verifier {
                                match check_voting_verifier(
                                    lcd,
                                    vv,
                                    source_chain,
                                    &message_id,
                                    &source_address,
                                    &dest_chain,
                                    &dest_address,
                                    &payload_hash_hex,
                                )
                                .await
                                {
                                    Ok(true) => CheckOutcome::PhaseComplete {
                                        elapsed: send_instant.elapsed().as_secs_f64(),
                                    },
                                    Ok(false) => CheckOutcome::NotYet,
                                    Err(e) => CheckOutcome::Error(format!("VotingVerifier: {e}")),
                                }
                            } else {
                                CheckOutcome::SkipVoting
                            }
                        }
                        Phase::HubApproved => {
                            match check_hub_approved(
                                lcd,
                                axelarnet_gateway,
                                source_chain,
                                &message_id,
                            )
                            .await
                            {
                                Ok(true) => CheckOutcome::PhaseComplete {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(false) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!("AxelarnetGateway: {e}")),
                            }
                        }
                        Phase::DiscoverSecondLeg => {
                            match discover_second_leg(rpc, &message_id).await {
                                Ok(Some(info)) => CheckOutcome::SecondLegDiscovered {
                                    message_id: info.message_id,
                                    payload_hash: info.payload_hash,
                                    source_address: info.source_address,
                                    destination_address: info.destination_address,
                                },
                                Ok(None) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!("second-leg discovery: {e}")),
                            }
                        }
                        Phase::Routed => {
                            let sl_id = second_leg_id.as_deref().unwrap_or("");
                            match check_cosmos_routed(lcd, cosm_gateway_dest, "axelar", sl_id).await
                            {
                                Ok(true) => CheckOutcome::PhaseComplete {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(false) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!("Gateway routing: {e}")),
                            }
                        }
                        Phase::Approved => {
                            let sl_id = second_leg_id.as_deref().unwrap_or("");
                            let sl_ph = second_leg_ph.as_deref().unwrap_or("");
                            let ph = parse_payload_hash(sl_ph).unwrap_or_default();
                            let sl_src_addr = second_leg_src.as_deref().unwrap_or("");
                            let sl_dst_addr: Address = second_leg_dst
                                .as_deref()
                                .unwrap_or("")
                                .parse()
                                .unwrap_or(Address::ZERO);
                            match check_evm_is_message_approved(
                                gw_contract,
                                "axelar",
                                sl_id,
                                sl_src_addr,
                                sl_dst_addr,
                                ph,
                            )
                            .await
                            {
                                Ok(true) => CheckOutcome::PhaseComplete {
                                    elapsed: send_instant.elapsed().as_secs_f64(),
                                },
                                Ok(false) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!("EVM approval: {e}")),
                            }
                        }
                        Phase::Executed => {
                            let sl_id = second_leg_id.as_deref().unwrap_or("");
                            let sl_ph = second_leg_ph.as_deref().unwrap_or("");
                            let ph = parse_payload_hash(sl_ph).unwrap_or_default();
                            let sl_src_addr = second_leg_src.as_deref().unwrap_or("");
                            let sl_dst_addr: Address = second_leg_dst
                                .as_deref()
                                .unwrap_or("")
                                .parse()
                                .unwrap_or(Address::ZERO);
                            match check_evm_is_message_approved(
                                gw_contract,
                                "axelar",
                                sl_id,
                                sl_src_addr,
                                sl_dst_addr,
                                ph,
                            )
                            .await
                            {
                                Ok(false) => {
                                    // false = approval consumed = executed
                                    CheckOutcome::PhaseComplete {
                                        elapsed: send_instant.elapsed().as_secs_f64(),
                                    }
                                }
                                Ok(true) => CheckOutcome::NotYet,
                                Err(e) => CheckOutcome::Error(format!("EVM execution: {e}")),
                            }
                        }
                        Phase::Done => CheckOutcome::NotYet,
                    };
                    (i, outcome)
                }
            })
            .collect();

        let results: Vec<_> = futures::stream::iter(futs)
            .buffer_unordered(20)
            .collect()
            .await;

        let mut error_msg = None;
        for (i, outcome) in &results {
            let i = *i;
            match outcome {
                CheckOutcome::NotYet => {}
                CheckOutcome::PhaseComplete { elapsed } => {
                    let elapsed = *elapsed;
                    match txs[i].phase {
                        Phase::Voted => {
                            txs[i].timing.voted_secs = Some(elapsed);
                            txs[i].phase = Phase::HubApproved;
                        }
                        Phase::HubApproved => {
                            // Hub approved implies voted
                            if txs[i].timing.voted_secs.is_none() {
                                txs[i].timing.voted_secs = Some(elapsed);
                            }
                            txs[i].timing.hub_approved_secs = Some(elapsed);
                            txs[i].phase = Phase::DiscoverSecondLeg;
                        }
                        Phase::DiscoverSecondLeg => {
                            txs[i].phase = Phase::Routed;
                        }
                        Phase::Routed => {
                            txs[i].timing.routed_secs = Some(elapsed);
                            txs[i].phase = Phase::Approved;
                        }
                        Phase::Approved => {
                            txs[i].timing.approved_secs = Some(elapsed);
                            txs[i].phase = Phase::Executed;
                        }
                        Phase::Executed => {
                            txs[i].timing.executed_secs = Some(elapsed);
                            txs[i].timing.executed_ok = Some(true);
                            txs[i].phase = Phase::Done;
                        }
                        Phase::Done => {}
                    }
                    last_progress = Instant::now();
                }
                CheckOutcome::SkipVoting => {
                    txs[i].phase = Phase::HubApproved;
                    last_progress = Instant::now();
                }
                CheckOutcome::SecondLegDiscovered {
                    message_id: sl_msg_id,
                    payload_hash: sl_ph,
                    source_address: sl_src,
                    destination_address: sl_dst,
                } => {
                    txs[i].second_leg_message_id = Some(sl_msg_id.clone());
                    txs[i].second_leg_payload_hash = Some(sl_ph.clone());
                    txs[i].second_leg_source_address = Some(sl_src.clone());
                    txs[i].second_leg_destination_address = Some(sl_dst.clone());
                    txs[i].phase = Phase::Routed;
                    last_progress = Instant::now();
                }
                CheckOutcome::AlreadyExecuted { elapsed } => {
                    let elapsed = *elapsed;
                    if txs[i].timing.voted_secs.is_none() {
                        txs[i].timing.voted_secs = Some(elapsed);
                    }
                    if txs[i].timing.approved_secs.is_none() {
                        txs[i].timing.approved_secs = Some(elapsed);
                    }
                    txs[i].timing.executed_secs = Some(elapsed);
                    txs[i].timing.executed_ok = Some(true);
                    txs[i].phase = Phase::Done;
                    last_progress = Instant::now();
                }
                CheckOutcome::Error(msg) => {
                    error_msg = Some(msg.clone());
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

        if last_progress.elapsed() >= INACTIVITY_TIMEOUT {
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
        solana_commitment_config::CommitmentConfig::confirmed(),
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

// ---------------------------------------------------------------------------
// Single-shot check helpers
// ---------------------------------------------------------------------------

/// Check VotingVerifier `messages_status` for a message.
/// Returns true if status contains "succeeded" (quorum reached).
#[allow(clippy::too_many_arguments)]
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
// Shared report computation
// ---------------------------------------------------------------------------

/// Compute peak throughput per pipeline step using 5-second sliding windows
/// over the absolute completion timestamps.
/// Compute sustained throughput per pipeline step: count / (last - first) on
/// absolute completion timestamps. The lowest value is the pipeline bottleneck.
#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
fn compute_peak_throughput(txs: &[PendingTx]) -> PeakThroughput {
    let Some(epoch) = txs.iter().map(|t| t.send_instant).min() else {
        return PeakThroughput::default();
    };

    let mut voted_times: Vec<f64> = Vec::new();
    let mut routed_times: Vec<f64> = Vec::new();
    let mut hub_approved_times: Vec<f64> = Vec::new();
    let mut approved_times: Vec<f64> = Vec::new();
    let mut executed_times: Vec<f64> = Vec::new();

    for tx in txs {
        let base = tx.send_instant.duration_since(epoch).as_secs_f64();
        if let Some(s) = tx.timing.voted_secs {
            voted_times.push(base + s);
        }
        if let Some(s) = tx.timing.routed_secs {
            routed_times.push(base + s);
        }
        if let Some(s) = tx.timing.hub_approved_secs {
            hub_approved_times.push(base + s);
        }
        if let Some(s) = tx.timing.approved_secs {
            approved_times.push(base + s);
        }
        if let Some(s) = tx.timing.executed_secs {
            executed_times.push(base + s);
        }
    }

    fn sustained_rate(times: &[f64]) -> Option<f64> {
        if times.len() < 2 {
            return None;
        }
        let min = times.iter().cloned().reduce(f64::min)?;
        let max = times.iter().cloned().reduce(f64::max)?;
        let span = max - min;
        if span > 0.0 {
            Some(times.len() as f64 / span)
        } else {
            None
        }
    }

    PeakThroughput {
        voted_tps: sustained_rate(&voted_times),
        routed_tps: sustained_rate(&routed_times),
        hub_approved_tps: sustained_rate(&hub_approved_times),
        approved_tps: sustained_rate(&approved_times),
        executed_tps: sustained_rate(&executed_times),
    }
}

/// Compute the `VerificationReport` from pending tx results, writing timings
/// back into the original metrics array.
#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
fn compute_verification_report(
    txs: &[PendingTx],
    metrics: &mut [TxMetrics],
    peak_throughput: PeakThroughput,
) -> VerificationReport {
    let mut successful = 0u64;
    let mut failed = 0u64;
    let mut failure_reasons: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    let mut stuck_count = 0u64;
    let mut stuck_phases: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    for tx in txs {
        if tx.idx < metrics.len() {
            metrics[tx.idx].amplifier_timing = Some(tx.timing.clone());
        }
        if tx.failed {
            failed += 1;
            if let Some(ref reason) = tx.fail_reason {
                *failure_reasons.entry(reason.clone()).or_insert(0) += 1;

                // Categorize stuck txs by the phase they got stuck at
                if reason.contains("timed out") {
                    stuck_count += 1;
                    let phase = stuck_phase(tx);
                    *stuck_phases.entry(phase).or_insert(0) += 1;
                }
            }
        } else if tx.timing.executed_ok == Some(true) {
            successful += 1;
        }
    }

    let total_verified = successful + failed;
    let success_rate = if total_verified > 0 {
        successful as f64 / total_verified as f64
    } else {
        0.0
    };

    let failure_categories: Vec<FailureCategory> = failure_reasons
        .into_iter()
        .map(|(reason, count)| FailureCategory { reason, count })
        .collect();

    let stuck_at: Vec<FailureCategory> = stuck_phases
        .into_iter()
        .map(|(reason, count)| FailureCategory { reason, count })
        .collect();

    let all_timings: Vec<&AmplifierTiming> = txs.iter().map(|t| &t.timing).collect();
    let avg_voted = avg_option(all_timings.iter().filter_map(|t| t.voted_secs));
    let avg_routed = avg_option(all_timings.iter().filter_map(|t| t.routed_secs));
    let avg_hub_approved = avg_option(all_timings.iter().filter_map(|t| t.hub_approved_secs));
    let avg_approved = avg_option(all_timings.iter().filter_map(|t| t.approved_secs));
    let avg_executed = avg_option(all_timings.iter().filter_map(|t| t.executed_secs));
    let min_executed = min_option(all_timings.iter().filter_map(|t| t.executed_secs));
    let max_executed = max_option(all_timings.iter().filter_map(|t| t.executed_secs));

    // Time from earliest send to last successful execution (for throughput).
    // This excludes timeout wait for stuck txs.
    let earliest_send = txs.iter().map(|tx| tx.send_instant).min();
    let last_execution = txs
        .iter()
        .filter(|tx| tx.timing.executed_ok == Some(true))
        .filter_map(|tx| {
            let secs = tx.timing.executed_secs?;
            Some(tx.send_instant + Duration::from_secs_f64(secs))
        })
        .max();
    let time_to_last_success = match (earliest_send, last_execution) {
        (Some(start), Some(end)) if end > start => Some(end.duration_since(start).as_secs_f64()),
        _ => None,
    };

    VerificationReport {
        total_verified,
        successful,
        pending: 0,
        failed,
        success_rate,
        failure_reasons: failure_categories,
        avg_voted_secs: avg_voted,
        avg_routed_secs: avg_routed,
        avg_hub_approved_secs: avg_hub_approved,
        avg_approved_secs: avg_approved,
        avg_executed_secs: avg_executed,
        min_executed_secs: min_executed,
        max_executed_secs: max_executed,
        time_to_last_success_secs: time_to_last_success,
        peak_throughput,
        stuck: stuck_count,
        stuck_at,
    }
}

/// Determine which phase a timed-out tx got stuck at (the last phase it didn't complete).
fn stuck_phase(tx: &PendingTx) -> String {
    match tx.phase {
        Phase::Voted => "voted".into(),
        Phase::Routed => "routed".into(),
        Phase::HubApproved => "hub approved".into(),
        Phase::DiscoverSecondLeg => "second-leg discovery".into(),
        Phase::Approved => "approved".into(),
        Phase::Executed => "executed".into(),
        Phase::Done => "done".into(),
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

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

#[allow(clippy::float_arithmetic)]
fn avg_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    let vals: Vec<f64> = iter.collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }
}

fn min_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    iter.reduce(f64::min)
}

fn max_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    iter.reduce(f64::max)
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
