use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use eyre::{Result, WrapErr};
use tokio::sync::mpsc;

use super::metrics::{AmplifierTiming, PeakThroughput, TxMetrics, VerificationReport};
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

mod checks;
mod its_deploy;
mod pipeline;
mod report;
mod state;

use self::pipeline::{
    DestinationChecker, ItsHubDest, PollItsHubArgs, PollItsHubEvmArgs, PollPipelineArgs,
    parse_payload_hash, poll_pipeline, poll_pipeline_its_hub, poll_pipeline_its_hub_evm,
};
use self::report::compute_verification_report;
use self::state::Phase;

// Re-export `PendingTx` to the parent `load_test` module so the per-pair
// runners can receive it back from the `tx_to_pending_*` constructors and
// forward it through their verifier mpsc channels.
pub(in crate::commands::load_test) use self::state::PendingTx;

// Re-export the ITS remote-deploy waiters so callers can keep using them as
// `super::verify::wait_for_its_remote_deploy*` after the move.
pub use self::its_deploy::{wait_for_its_remote_deploy, wait_for_its_remote_deploy_to_solana};

// ---------------------------------------------------------------------------
// Shared inner helpers (private)
// ---------------------------------------------------------------------------

/// Indices of confirmed transactions in a metrics slice.
fn confirmed_indices(metrics: &[TxMetrics]) -> Vec<usize> {
    metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect()
}

/// Streaming or batch dispatch for the polling pipelines.
enum VerifyMode<'a> {
    Batch,
    Stream {
        rx: &'a mut mpsc::UnboundedReceiver<PendingTx>,
        send_done: &'a AtomicBool,
        spinner: indicatif::ProgressBar,
    },
}

impl<'a> VerifyMode<'a> {
    fn parts(
        self,
    ) -> (
        Option<&'a mut mpsc::UnboundedReceiver<PendingTx>>,
        Option<&'a AtomicBool>,
        Option<indicatif::ProgressBar>,
    ) {
        match self {
            VerifyMode::Batch => (None, None, None),
            VerifyMode::Stream {
                rx,
                send_done,
                spinner,
            } => (Some(rx), Some(send_done), Some(spinner)),
        }
    }
}

/// Axelar config loaded for GMP verification.
struct GmpAxelarConfig {
    lcd: String,
    voting_verifier: Option<String>,
    cosm_gateway: String,
}

fn load_gmp_axelar_config(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
) -> Result<GmpAxelarConfig> {
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
    Ok(GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    })
}

/// Axelar config loaded for ITS-via-hub verification. Does not include the
/// Tendermint `rpc` field — callers fetch `read_axelar_rpc` separately at
/// the original call site so the ordering relative to tx construction is
/// preserved verbatim. The `cfg` field is returned so the same config object
/// can be reused for the destination `Gateway` lookup.
struct ItsAxelarConfig {
    cfg: ChainsConfig,
    lcd: String,
    voting_verifier: Option<String>,
    axelarnet_gateway: String,
}

fn load_its_axelar_config(config: &Path, source_chain: &str) -> Result<ItsAxelarConfig> {
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
    Ok(ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    })
}

/// Look up the destination cosmos `Gateway` for a chain.
fn lookup_cosm_gateway_dest(cfg: &ChainsConfig, destination_chain: &str) -> Result<String> {
    Ok(cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string())
}

/// Look up the destination cosmos `Gateway` for an XRPL chain, falling back
/// to `XrplGateway` for deployments that use that contract name.
fn lookup_xrpl_cosm_gateway_dest(cfg: &ChainsConfig, destination_chain: &str) -> Result<String> {
    Ok(cfg
        .axelar
        .contract_address("Gateway", destination_chain)
        .or_else(|_| {
            cfg.axelar
                .contract_address("XrplGateway", destination_chain)
        })?
        .to_string())
}

/// Args bundle for [`run_gmp_pipeline`].
struct RunGmpArgs {
    lcd: String,
    voting_verifier: Option<String>,
    cosm_gateway: String,
    source_chain: String,
    destination_chain: String,
    destination_address: String,
}

/// Drive the GMP polling pipeline (both batch and streaming modes).
async fn run_gmp_pipeline<P: Provider>(
    txs: &mut Vec<PendingTx>,
    checker: &DestinationChecker<'_, P>,
    mode: VerifyMode<'_>,
    args: RunGmpArgs,
) -> Result<PeakThroughput> {
    let RunGmpArgs {
        lcd,
        voting_verifier,
        cosm_gateway,
        source_chain,
        destination_chain,
        destination_address,
    } = args;
    let (rx, send_done, spinner) = mode.parts();
    poll_pipeline(
        txs,
        rx,
        send_done,
        checker,
        spinner,
        PollPipelineArgs {
            lcd,
            voting_verifier,
            cosm_gateway: Some(cosm_gateway),
            source_chain,
            destination_chain,
            destination_address,
            axelarnet_gateway: None,
            display_chain: None,
        },
    )
    .await
}

/// Args bundle for [`run_its_hub_pipeline`].
struct RunItsHubArgs {
    lcd: String,
    voting_verifier: Option<String>,
    source_chain: String,
    axelarnet_gateway: String,
    rpc: String,
    cosm_gateway_dest: String,
    dest: ItsHubDest,
}

/// Drive the ITS-via-hub polling pipeline (both batch and streaming modes).
async fn run_its_hub_pipeline(
    txs: &mut Vec<PendingTx>,
    mode: VerifyMode<'_>,
    args: RunItsHubArgs,
) -> Result<PeakThroughput> {
    let RunItsHubArgs {
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        cosm_gateway_dest,
        dest,
    } = args;
    let (rx, send_done, spinner) = mode.parts();
    poll_pipeline_its_hub(
        txs,
        rx,
        send_done,
        spinner,
        PollItsHubArgs {
            lcd,
            voting_verifier,
            source_chain,
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            dest,
        },
    )
    .await
}

/// Args bundle for [`run_its_hub_evm_pipeline`].
struct RunItsHubEvmArgs {
    lcd: String,
    voting_verifier: Option<String>,
    source_chain: String,
    axelarnet_gateway: String,
    rpc: String,
    cosm_gateway_dest: String,
    destination_chain: String,
}

/// Drive the ITS-via-hub polling pipeline with an EVM destination
/// (both batch and streaming modes).
async fn run_its_hub_evm_pipeline<P: Provider>(
    txs: &mut Vec<PendingTx>,
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    mode: VerifyMode<'_>,
    args: RunItsHubEvmArgs,
) -> Result<PeakThroughput> {
    let RunItsHubEvmArgs {
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        cosm_gateway_dest,
        destination_chain,
    } = args;
    let (rx, send_done, spinner) = mode.parts();
    poll_pipeline_its_hub_evm(
        txs,
        rx,
        send_done,
        gw_contract,
        spinner,
        PollItsHubEvmArgs {
            lcd,
            voting_verifier,
            source_chain,
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            _destination_chain: destination_chain,
        },
    )
    .await
}

/// Build the `(report, timings)` tuple returned by every streaming entry.
fn streaming_report_and_timings(
    txs: &[PendingTx],
    peaks: PeakThroughput,
) -> (VerificationReport, Vec<(String, AmplifierTiming)>) {
    let report = compute_verification_report(txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    (report, timings)
}

fn parse_first_leg_payload_hash(
    tx: &TxMetrics,
    required: bool,
) -> Result<Option<alloy::primitives::FixedBytes<32>>> {
    if tx.payload_hash.is_empty() {
        if required {
            return Err(eyre::eyre!(
                "missing payload_hash for confirmed tx {}",
                tx.signature
            ));
        }
        return Ok(None);
    }
    parse_payload_hash(&tx.payload_hash)
        .map(Some)
        .wrap_err_with(|| format!("invalid payload_hash for confirmed tx {}", tx.signature))
}

/// Build a `PendingTx` for an ITS-via-hub batch entry. The four ITS batch
/// orchestrators (`verify_onchain_{solana,stellar,xrpl,evm}_its`) share the
/// same struct literal — only the starting `phase` differs, which the caller
/// computes from `voting_verifier.is_some()` (or hardcodes for EVM-ITS).
fn pending_tx_for_its_batch(tx: &TxMetrics, idx: usize, initial_phase: Phase) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, initial_phase == Phase::Voted)?;
    Ok(PendingTx {
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
    })
}

/// Compute the source-side `message_id` from a confirmed `TxMetrics` based on
/// the source chain family. EVM/Stellar/Sui pre-format the id in
/// `tx.signature`; SVM appends the `call_contract` log index.
fn message_id_for_source(tx: &TxMetrics, source_type: SourceChainType) -> String {
    match source_type {
        SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
            tx.signature.clone()
        }
        SourceChainType::Svm => {
            format!("{}-{}.1", tx.signature, solana_call_contract_index())
        }
    }
}

/// Build a `PendingTx` for a GMP batch entry. The four GMP batch orchestrators
/// vary by destination chain — `contract_addr` (parsed for EVM, zero
/// elsewhere), `command_id` (`Some` for Solana, `None` elsewhere), and the
/// `gmp_destination_*` fields — so the caller passes those explicitly.
#[allow(clippy::too_many_arguments)]
fn pending_tx_for_gmp_batch(
    tx: &TxMetrics,
    idx: usize,
    message_id: String,
    contract_addr: Address,
    command_id: Option<[u8; 32]>,
    gmp_destination_chain: String,
    gmp_destination_address: String,
    initial_phase: Phase,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    Ok(PendingTx {
        idx,
        message_id,
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id,
        gmp_destination_chain,
        gmp_destination_address,
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: initial_phase,
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
    })
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
    let confirmed = confirmed_indices(metrics);
    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

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
            pending_tx_for_gmp_batch(
                tx,
                idx,
                message_id_for_source(tx, source_type),
                contract_addr,
                None, // EVM destination, not needed
                String::new(),
                String::new(),
                initial_phase,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let checker = DestinationChecker::Evm {
        gw_contract: &gw_contract,
    };

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Batch,
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
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
    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, &provider);

    let checker = DestinationChecker::Evm {
        gw_contract: &gw_contract,
    };

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
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
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;
    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            pending_tx_for_gmp_batch(
                tx,
                idx,
                message_id_for_source(tx, source_type),
                Address::ZERO,
                None,
                tx.gmp_destination_chain.clone(),
                destination_contract.to_string(),
                initial_phase,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, stellar_network_type)?;
    let checker: DestinationChecker<alloy::providers::RootProvider> = DestinationChecker::Stellar {
        client: stellar_client,
        gateway_contract: stellar_gateway.to_string(),
        signer_pk,
        _phantom: std::marker::PhantomData,
    };

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Batch,
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_contract.to_string(),
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
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
    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, stellar_network_type)?;
    let checker: DestinationChecker<alloy::providers::RootProvider> = DestinationChecker::Stellar {
        client: stellar_client,
        gateway_contract: stellar_gateway.to_string(),
        signer_pk,
        _phantom: std::marker::PhantomData,
    };

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_contract.to_string(),
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Convert a confirmed TxMetrics into a PendingTx for Solana verification.
pub(super) fn tx_to_pending_solana(
    tx: &TxMetrics,
    idx: usize,
    source_chain: &str,
    has_voting_verifier: bool,
    source_type: SourceChainType,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    let message_id = match source_type {
        SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
            tx.signature.clone()
        }
        SourceChainType::Svm => {
            format!("{}-{}.1", tx.signature, solana_call_contract_index())
        }
    };
    let cmd_input = [source_chain.as_bytes(), b"-", message_id.as_bytes()].concat();
    Ok(PendingTx {
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
    })
}

/// Convert a confirmed TxMetrics into a PendingTx for Stellar-sourced GMP.
/// The `signature` field on the input is the pre-formatted message id
/// (`0x{lowercase_hex_tx_hash}-{event_index}`) per the `hex_tx_hash_and_event_index`
/// format of the Stellar `VotingVerifier`.
pub(super) fn tx_to_pending_stellar(
    tx: &TxMetrics,
    has_voting_verifier: bool,
    contract_addr: Address,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    Ok(PendingTx {
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
    })
}

/// Convert a confirmed TxMetrics into a PendingTx for XRPL-sourced ITS
/// verification. The `signature` field on the input is the already-formatted
/// XRPL message id (`0x{lowercase_hex_tx_hash}`), which is what the
/// `XrplVotingVerifier` / `XrplGateway` expect.
pub(super) fn tx_to_pending_xrpl(tx: &TxMetrics, has_voting_verifier: bool) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, has_voting_verifier)?;
    Ok(PendingTx {
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
    })
}

/// Convert a confirmed TxMetrics into a PendingTx for ITS hub verification.
/// ITS messages route through the hub, so gmp_destination_chain/address are
/// set from the TxMetrics (typically "axelar" / AxelarnetGateway).
pub(super) fn tx_to_pending_its(tx: &TxMetrics, has_voting_verifier: bool) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, has_voting_verifier)?;
    Ok(PendingTx {
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
    })
}

// ---------------------------------------------------------------------------
// Sui destination verifier (GMP)
// ---------------------------------------------------------------------------

/// Burst-mode Sui destination verifier — block on confirmed metrics array.
/// Uses Sui events polling (`MessageApproved` / `MessageExecuted` on the
/// AxelarGateway events module) for the destination-side phases.
pub async fn verify_onchain_sui_gmp(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    sui_rpc: &str,
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;
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
            pending_tx_for_gmp_batch(
                tx,
                idx,
                message_id_for_source(tx, source_type),
                Address::ZERO,
                None,
                tx.gmp_destination_chain.clone(),
                destination_address.to_string(),
                initial_phase,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> = DestinationChecker::Sui {
        client: sui_client,
        gateway_pkg,
        _phantom: std::marker::PhantomData,
    };

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Batch,
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
        },
    )
    .await?;

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
    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

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

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
        },
    )
    .await?;

    // Key by message_id (signature) since streaming PendingTx idx is always 0.
    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Verify EVM->Solana transactions through the Amplifier pipeline:
///
/// 1. **Voted** — VotingVerifier verification (source EVM chain)
/// 2. **Routed** — Cosmos Gateway outgoing_messages (dest Solana chain)
/// 3. **Approved** — Solana IncomingMessage PDA exists
/// 4. **Executed** — Solana IncomingMessage PDA status = executed
pub async fn verify_onchain_solana(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    solana_rpc: &str,
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let message_id = message_id_for_source(tx, source_type);
            let cmd_input = [source_chain.as_bytes(), b"-", message_id.as_bytes()].concat();
            pending_tx_for_gmp_batch(
                tx,
                idx,
                message_id,
                Address::ZERO,
                Some(keccak256(&cmd_input).into()),
                String::new(),
                String::new(),
                initial_phase,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    ));

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> =
        DestinationChecker::Solana {
            rpc_client,
            _phantom: std::marker::PhantomData,
        };

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Batch,
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
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
pub async fn verify_onchain_solana_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    _destination_address: &str,
    solana_rpc: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Batch,
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            dest: ItsHubDest::Solana {
                rpc_url: solana_rpc.to_string(),
            },
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Streaming version of `verify_onchain_solana_its` — runs concurrently with
/// the send phase, receiving confirmed txs via the channel.
pub async fn verify_onchain_solana_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    solana_rpc: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            dest: ItsHubDest::Solana {
                rpc_url: solana_rpc.to_string(),
            },
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
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
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Batch,
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            dest: ItsHubDest::Stellar {
                rpc_url: stellar_rpc.to_string(),
                network_type: stellar_network_type.to_string(),
                gateway_contract: stellar_gateway_contract.to_string(),
                signer_pk,
            },
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
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
    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            dest: ItsHubDest::Stellar {
                rpc_url: stellar_rpc.to_string(),
                network_type: stellar_network_type.to_string(),
                gateway_contract: stellar_gateway_contract.to_string(),
                signer_pk,
            },
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Verify EVM/Solana → XRPL ITS transactions. Polls the recipient XRPL
/// account's `account_tx` for an inbound `Payment` whose `message_id` memo
/// matches the second-leg message id (the XRPL relayer attaches that memo).
pub async fn verify_onchain_xrpl_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    xrpl_rpc: &str,
    xrpl_recipient: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;
    // XRPL's destination cosmos gateway is `XrplGateway/{chain}`, not the
    // standard `Gateway/{chain}`. Try both so the same verifier works
    // regardless of which contract name the deployment uses.
    let cosm_gateway_dest = lookup_xrpl_cosm_gateway_dest(&cfg, destination_chain)?;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Batch,
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            dest: ItsHubDest::Xrpl {
                rpc_url: xrpl_rpc.to_string(),
                recipient_address: xrpl_recipient.to_string(),
            },
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
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
    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_xrpl_cosm_gateway_dest(&cfg, destination_chain)?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            dest: ItsHubDest::Xrpl {
                rpc_url: xrpl_rpc.to_string(),
                recipient_address: xrpl_recipient.to_string(),
            },
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
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
pub async fn verify_onchain_evm_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    _destination_address: &str,
    evm_gateway_addr: Address,
    evm_rpc_url: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
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
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(evm_gateway_addr, &provider);

    let peaks = run_its_hub_evm_pipeline(
        &mut txs,
        &gw_contract,
        VerifyMode::Batch,
        RunItsHubEvmArgs {
            lcd,
            voting_verifier: None, // skip VotingVerifier — no payload_hash for Solana ITS
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            destination_chain: destination_chain.to_string(),
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
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
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(evm_gateway_addr, &provider);

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_its_hub_evm_pipeline(
        &mut txs,
        &gw_contract,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunItsHubEvmArgs {
            lcd,
            voting_verifier: None, // skip VotingVerifier — Solana ITS has no payload_hash
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            destination_chain: destination_chain.to_string(),
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}
