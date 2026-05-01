//! All the per-phase RPC checks plus the three poll-loops that drive them.
//! Each loop walks every active tx once per `POLL_INTERVAL`, fans out the
//! per-tx phase checks across `buffer_unordered(20)`, and folds the results
//! back into the tx state via [`apply_check_outcome`](super::state).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use alloy::primitives::{Address, FixedBytes, keccak256};
use alloy::providers::Provider;
use eyre::Result;
use futures::StreamExt;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

use super::super::metrics::{AmplifierTiming, PeakThroughput};
use super::report::{compute_peak_throughput, parse_payload_hash};
use super::state::{
    ApplyResult, ApprovalResult, CheckOutcome, INACTIVITY_TIMEOUT, POLL_INTERVAL, PendingTx, Phase,
    RealTimeStats, TxPhaseSnapshot, apply_check_outcome, phase_counts,
};
use crate::cosmos::{
    check_cosmos_routed, check_hub_approved, discover_second_leg, lcd_cosmwasm_smart_query,
};
use crate::evm::AxelarAmplifierGateway;
use crate::ui;

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
                // command_id must be set by the time we're checking Solana approval —
                // the GMP path derives it up front, the ITS path populates it on
                // second-leg discovery. None here is a flow bug, not "not yet".
                let cmd_id = tx.command_id.ok_or_else(|| {
                    eyre::eyre!("command_id not set when checking Solana approval")
                })?;
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
                let cmd_id = tx.command_id.ok_or_else(|| {
                    eyre::eyre!("command_id not set when checking Solana execution")
                })?;
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
// Per-tx phase check
// ---------------------------------------------------------------------------

/// Run one phase check for one tx — picks the right cosmwasm/destination call
/// based on the tx's current phase and reports a [`CheckOutcome`].
#[allow(clippy::too_many_arguments)]
async fn check_tx_phase<P: Provider>(
    i: usize,
    snapshot: TxPhaseSnapshot,
    lcd: &str,
    voting_verifier: Option<&str>,
    cosm_gateway: Option<&str>,
    axelarnet_gateway: Option<&str>,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    checker: &DestinationChecker<'_, P>,
) -> CheckOutcome {
    let TxPhaseSnapshot {
        phase,
        message_id,
        source_address,
        contract_addr,
        payload_hash,
        payload_hash_hex,
        send_instant,
        command_id,
    } = snapshot;

    match phase {
        Phase::Voted => {
            let Some(vv) = voting_verifier else {
                return CheckOutcome::SkipVoting;
            };
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
        }
        Phase::Routed => {
            let Some(gw) = cosm_gateway else {
                return CheckOutcome::SkipVoting;
            };
            match check_cosmos_routed(lcd, gw, source_chain, &message_id).await {
                Ok(true) => CheckOutcome::PhaseComplete {
                    elapsed: send_instant.elapsed().as_secs_f64(),
                },
                Ok(false) => CheckOutcome::NotYet,
                Err(e) => CheckOutcome::Error(format!("Gateway: {e}")),
            }
        }
        Phase::HubApproved => {
            let Some(gw) = axelarnet_gateway else {
                return CheckOutcome::SkipVoting;
            };
            match check_hub_approved(lcd, gw, source_chain, &message_id).await {
                Ok(true) => CheckOutcome::PhaseComplete {
                    elapsed: send_instant.elapsed().as_secs_f64(),
                },
                Ok(false) => CheckOutcome::NotYet,
                Err(e) => CheckOutcome::Error(format!("AxelarnetGateway: {e}")),
            }
        }
        Phase::DiscoverSecondLeg | Phase::Done => CheckOutcome::NotYet,
        Phase::Approved => {
            let tmp = make_pending_view(
                phase,
                &message_id,
                &source_address,
                contract_addr,
                payload_hash,
                &payload_hash_hex,
                send_instant,
                command_id,
            );
            match checker.check_approved(&tmp, i, source_chain).await {
                Ok(ApprovalResult::Approved) => CheckOutcome::PhaseComplete {
                    elapsed: send_instant.elapsed().as_secs_f64(),
                },
                Ok(ApprovalResult::AlreadyExecuted) => CheckOutcome::AlreadyExecuted {
                    elapsed: send_instant.elapsed().as_secs_f64(),
                },
                Ok(ApprovalResult::NotYet) => CheckOutcome::NotYet,
                Err(e) => CheckOutcome::Error(format!("{}: {e}", checker.approval_label())),
            }
        }
        Phase::Executed => {
            let tmp = make_pending_view(
                phase,
                &message_id,
                &source_address,
                contract_addr,
                payload_hash,
                &payload_hash_hex,
                send_instant,
                command_id,
            );
            match checker.check_executed(&tmp, i, source_chain).await {
                Ok(true) => CheckOutcome::PhaseComplete {
                    elapsed: send_instant.elapsed().as_secs_f64(),
                },
                Ok(false) => CheckOutcome::NotYet,
                Err(e) => CheckOutcome::Error(format!("{}: {e}", checker.execution_label())),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn make_pending_view(
    phase: Phase,
    message_id: &str,
    source_address: &str,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
    payload_hash_hex: &str,
    send_instant: Instant,
    command_id: Option<[u8; 32]>,
) -> PendingTx {
    PendingTx {
        idx: 0,
        message_id: message_id.to_string(),
        send_instant,
        source_address: source_address.to_string(),
        contract_addr,
        payload_hash,
        payload_hash_hex: payload_hash_hex.to_string(),
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
    }
}

// ---------------------------------------------------------------------------
// Unified polling pipeline
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(super) async fn poll_pipeline<P: Provider>(
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
                let snapshot = TxPhaseSnapshot {
                    phase: txs[i].phase,
                    message_id: txs[i].message_id.clone(),
                    source_address: txs[i].source_address.clone(),
                    contract_addr: txs[i].contract_addr,
                    payload_hash: txs[i].payload_hash,
                    payload_hash_hex: txs[i].payload_hash_hex.clone(),
                    send_instant: txs[i].send_instant,
                    command_id: txs[i].command_id,
                };
                async move {
                    let outcome = check_tx_phase(
                        i,
                        snapshot,
                        lcd,
                        voting_verifier,
                        cosm_gateway,
                        axelarnet_gateway,
                        source_chain,
                        destination_chain,
                        destination_address,
                        checker,
                    )
                    .await;
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
            match apply_check_outcome(&mut txs[i], outcome, axelarnet_gateway.is_some()) {
                ApplyResult::Progress => last_progress = Instant::now(),
                ApplyResult::NoChange => {}
                ApplyResult::Error(msg) => {
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
// ITS hub-only pipeline (Solana destination)
// ---------------------------------------------------------------------------

/// Full ITS polling pipeline: Voted → HubApproved → DiscoverSecondLeg → Routed → Approved → Executed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn poll_pipeline_its_hub(
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

    let sol_rpc_client = Arc::new(crate::solana::rpc_client(solana_rpc));

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
                            match check_cosmos_routed(
                                lcd,
                                cosm_gateway_dest,
                                crate::types::HubChain::NAME,
                                sl_id,
                            )
                            .await
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

// ---------------------------------------------------------------------------
// ITS hub pipeline with EVM destination
// ---------------------------------------------------------------------------

/// Full ITS polling pipeline with EVM destination:
/// Voted → HubApproved → DiscoverSecondLeg → Routed → Approved(EVM) → Executed(EVM).
#[allow(clippy::too_many_arguments)]
pub(super) async fn poll_pipeline_its_hub_evm<P: Provider>(
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
                            match check_cosmos_routed(
                                lcd,
                                cosm_gateway_dest,
                                crate::types::HubChain::NAME,
                                sl_id,
                            )
                            .await
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
                            let ph = parse_payload_hash(sl_ph).expect(
                                "second-leg payload_hash from cosmos event must be 32-byte hex",
                            );
                            let sl_src_addr = second_leg_src.as_deref().unwrap_or("");
                            let sl_dst_addr: Address = second_leg_dst
                                .as_deref()
                                .unwrap_or("")
                                .parse()
                                .unwrap_or(Address::ZERO);
                            match check_evm_is_message_approved(
                                gw_contract,
                                crate::types::HubChain::NAME,
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
                            let ph = parse_payload_hash(sl_ph).expect(
                                "second-leg payload_hash from cosmos event must be 32-byte hex",
                            );
                            let sl_src_addr = second_leg_src.as_deref().unwrap_or("");
                            let sl_dst_addr: Address = second_leg_dst
                                .as_deref()
                                .unwrap_or("")
                                .parse()
                                .unwrap_or(Address::ZERO);
                            match check_evm_is_message_approved(
                                gw_contract,
                                crate::types::HubChain::NAME,
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

// ---------------------------------------------------------------------------
// Single-shot check helpers
// ---------------------------------------------------------------------------

/// Check VotingVerifier `messages_status` for a message.
/// Returns true if status contains "succeeded" (quorum reached).
#[allow(clippy::too_many_arguments)]
pub(super) async fn check_voting_verifier(
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

/// Check `isMessageApproved` on the EVM gateway (single attempt).
pub(super) async fn check_evm_is_message_approved<P: Provider>(
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
pub(super) fn check_solana_incoming_message(
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
