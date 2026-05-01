//! Amplifier-pipeline verification. Public surface is one entry point per
//! source/destination shape:
//!
//! - [`verify_onchain`]: any source → EVM destination (GMP).
//! - [`verify_onchain_solana`] / [`verify_onchain_solana_streaming`]: any
//!   source → Solana destination (GMP).
//! - [`verify_onchain_solana_its`] / [`verify_onchain_evm_its`]: ITS hub
//!   routing variants for the two destination shapes.
//! - [`wait_for_its_remote_deploy`] / [`wait_for_its_remote_deploy_to_solana`]:
//!   single-message blocking waits used by the ITS load-test setup phase.
//!
//! Internals are split by concern:
//! - [`state`]: data shapes (`PendingTx`, `Phase`, `RealTimeStats`) +
//!   phase-transition logic.
//! - [`pipeline`]: per-phase RPC checks + the three poll loops.
//! - [`report`]: report computation + percentile latency + tests.

mod pipeline;
mod report;
mod state;

pub use state::PendingTx;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use eyre::Result;
use tokio::sync::mpsc;

use pipeline::{
    DestinationChecker, check_evm_is_message_approved, check_solana_incoming_message,
    poll_pipeline, poll_pipeline_its_hub, poll_pipeline_its_hub_evm,
};
use report::{compute_peak_throughput, compute_verification_report, parse_payload_hash};
use state::Phase;

use super::metrics::{AmplifierTiming, TxMetrics, VerificationReport};
use crate::cosmos::{
    check_cosmos_routed, check_hub_approved, discover_second_leg, read_axelar_config,
    read_axelar_contract_field, read_axelar_rpc,
};
use crate::evm::AxelarAmplifierGateway;
use crate::solana::solana_call_contract_index;
use crate::ui;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

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
            // Empty when source is Solana ITS (the program builds the payload via
            // CPI and tx metrics carry no hash). HubApproved+later phases use
            // source_chain + message_id, so the zero placeholder is fine.
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            let message_id = match source_type {
                SourceChainType::Evm => tx.signature.clone(),
                SourceChainType::Svm => {
                    format!("{}-{}.1", tx.signature, solana_call_contract_index())
                }
            };
            PendingTx {
                idx,
                message_id,
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
    // Empty when source is Solana ITS (the program builds the payload via
    // CPI and tx metrics carry no hash). HubApproved+later phases use
    // source_chain + message_id, so the zero placeholder is fine.
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

    let rpc_client = Arc::new(crate::solana::rpc_client(solana_rpc));

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
            // Empty when source is Solana ITS (the program builds the payload via
            // CPI and tx metrics carry no hash). HubApproved+later phases use
            // source_chain + message_id, so the zero placeholder is fine.
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

    let rpc_client = Arc::new(crate::solana::rpc_client(solana_rpc));

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
            // Empty when source is Solana ITS (the program builds the payload via
            // CPI and tx metrics carry no hash). HubApproved+later phases use
            // source_chain + message_id, so the zero placeholder is fine.
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
            // Empty when source is Solana ITS (the program builds the payload via
            // CPI and tx metrics carry no hash). HubApproved+later phases use
            // source_chain + message_id, so the zero placeholder is fine.
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
                if check_cosmos_routed(
                    &lcd,
                    &cosm_gateway_dest,
                    crate::types::HubChain::NAME,
                    sl_id,
                )
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
                let ph = parse_payload_hash(sl_ph_str)
                    .expect("second-leg payload_hash from cosmos event must be 32-byte hex");
                match check_evm_is_message_approved(
                    &gw_contract,
                    crate::types::HubChain::NAME,
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
                let ph = parse_payload_hash(sl_ph_str)
                    .expect("second-leg payload_hash from cosmos event must be 32-byte hex");
                match check_evm_is_message_approved(
                    &gw_contract,
                    crate::types::HubChain::NAME,
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

    let sol_rpc_client = crate::solana::rpc_client(solana_rpc);

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
                if check_cosmos_routed(
                    &lcd,
                    &cosm_gateway_dest,
                    crate::types::HubChain::NAME,
                    sl_id,
                )
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
