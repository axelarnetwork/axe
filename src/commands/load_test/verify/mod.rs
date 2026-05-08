use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use eyre::Result;
use tokio::sync::mpsc;

use super::metrics::{AmplifierTiming, TxMetrics, VerificationReport};
use crate::config::ChainsConfig;
use crate::cosmos::read_axelar_rpc;
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

mod pipeline;
mod report;
mod state;

use self::pipeline::{
    DestinationChecker, ItsHubDest, check_cosmos_routed, check_evm_is_message_approved,
    check_hub_approved, check_solana_incoming_message, discover_second_leg, parse_payload_hash,
    poll_pipeline, poll_pipeline_its_hub, poll_pipeline_its_hub_evm,
};
use self::report::compute_verification_report;
use self::state::Phase;

// Re-export `PendingTx` to the parent `load_test` module so the per-pair
// runners can receive it back from the `tx_to_pending_*` constructors and
// forward it through their verifier mpsc channels.
pub(in crate::commands::load_test) use self::state::PendingTx;

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

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
                    SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
                        tx.signature.clone()
                    }
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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();
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
                SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
                    tx.signature.clone()
                }
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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
        SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
            tx.signature.clone()
        }
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

/// Burst-mode Sui destination verifier — block on confirmed metrics array.
/// Uses Sui events polling (`MessageApproved` / `MessageExecuted` on the
/// AxelarGateway events module) for the destination-side phases.
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

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();
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
                SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
                    tx.signature.clone()
                }
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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
                SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
                    tx.signature.clone()
                }
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

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

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
    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

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
    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

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
    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)
        .or_else(|_| {
            cfg.axelar
                .contract_address("XrplGateway", destination_chain)
        })?
        .to_string();

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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)
        .or_else(|_| {
            cfg.axelar
                .contract_address("XrplGateway", destination_chain)
        })?
        .to_string();

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

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

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
    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let rpc = read_axelar_rpc(config)?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);

    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let rpc = read_axelar_rpc(config)?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

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
