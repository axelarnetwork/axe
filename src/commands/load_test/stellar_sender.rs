//! Stellar GMP sender. Mirrors the shape of `xrpl_sender.rs` for
//! Stellar-sourced `AxelarGateway.call_contract` invocations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use alloy::primitives::keccak256;
use alloy::sol_types::SolValue;
use eyre::{Result, eyre};
use futures::future::join_all;
use indicatif::ProgressBar;
use rand::Rng;
use tokio::sync::Mutex;

use super::metrics::{LoadTestReport, TxMetrics};
use super::sustained;
use crate::stellar::{
    StellarClient, StellarWallet, scval_address_account, scval_address_from_str, scval_bytes,
    scval_string, scval_token,
};
use crate::ui;

/// Stellar base fee in stroops (100 stroops = 0.00001 XLM). Plus the
/// simulated resource fee added by the client.
pub const BASE_FEE: u32 = 100;

/// Default cross-chain gas payment, in stroops (1 XLM = 10^7 stroops).
/// `AxelarExample.send` forwards this to `AxelarGasService.pay_gas`.
/// Leaving 0 gives the "Insufficient Fee" error on Axelarscan; 10 XLM is
/// a comfortable default on testnet/devnet.
pub const DEFAULT_GAS_STROOPS: u64 = 100_000_000; // 10 XLM

/// Generate a default ABI-encoded payload compatible with EVM SenderReceiver.
pub fn make_payload(custom: &Option<Vec<u8>>) -> Vec<u8> {
    match custom {
        Some(p) => p.clone(),
        None => {
            let mut buf = [0u8; 16];
            rand::thread_rng().fill(&mut buf);
            let suffix = hex::encode(buf);
            let message = format!("hello from axe load test {suffix}");
            (message,).abi_encode_params()
        }
    }
}

/// Build and submit a single `AxelarExample.send(...)` invocation — the
/// high-level wrapper that internally pays gas via `AxelarGasService` and
/// emits the `ContractCall` event from `AxelarGateway`. Mirrors the reference
/// `axelar-contract-deployments/stellar/gmp.js` script.
#[allow(clippy::too_many_arguments)]
async fn submit_single(
    client: &StellarClient,
    wallet: &StellarWallet,
    example_contract: &str,
    gateway_contract: &str,
    destination_chain: &str,
    destination_address: &str,
    payload: &[u8],
    gas_token_contract: &str,
    gas_amount_stroops: u64,
) -> TxMetrics {
    let submit_start = Instant::now();
    // The on-chain `ContractCall` event is emitted by the example contract
    // (which `AxelarExample.send` invokes internally), so the message's
    // `source_address` from the VotingVerifier's perspective is the example
    // contract's C-address — NOT the caller's G-address. Match what
    // Amplifier stored on-chain to make the voted-stage query succeed.
    let source_addr = example_contract.to_string();
    let _caller_addr = wallet.address();
    let payload_hash = hex::encode(keccak256(payload));

    let args = match build_send_args(
        wallet,
        destination_chain,
        destination_address,
        payload,
        gas_token_contract,
        gas_amount_stroops,
    ) {
        Ok(a) => a,
        Err(e) => return fail_metrics(submit_start, &source_addr, &format!("args: {e}")),
    };

    let gateway_filter = match crate::stellar::parse_contract_id(gateway_contract) {
        Ok(h) => Some(h),
        Err(e) => return fail_metrics(submit_start, &source_addr, &format!("gateway id: {e}")),
    };

    match client
        .invoke_contract(
            wallet,
            example_contract,
            "send",
            args,
            BASE_FEE,
            gateway_filter,
        )
        .await
    {
        Ok(invoked) => {
            #[allow(clippy::cast_possible_truncation)]
            let submit_time_ms = submit_start.elapsed().as_millis() as u64;
            // Stellar message IDs are `0x{lowercase_tx_hash}-{event_index}` per
            // the `hex_tx_hash_and_event_index` msg_id format. When
            // `AxelarExample.send` is the entrypoint, multiple events fire
            // (gas service first, then gateway emits `ContractCall`) — the
            // index is looked up dynamically from the validated tx response.
            let event_index = invoked.event_index.unwrap_or(0);
            let message_id = format!("0x{}-{event_index}", invoked.tx_hash_hex.to_lowercase());
            TxMetrics {
                signature: message_id,
                submit_time_ms,
                confirm_time_ms: Some(submit_time_ms),
                latency_ms: Some(submit_time_ms),
                compute_units: None,
                slot: None,
                success: invoked.success,
                error: if invoked.success {
                    None
                } else {
                    Some("tx failed on-chain".to_string())
                },
                payload: payload.to_vec(),
                payload_hash,
                source_address: source_addr,
                gmp_destination_chain: destination_chain.to_string(),
                gmp_destination_address: destination_address.to_string(),
                send_instant: Some(submit_start),
                amplifier_timing: None,
            }
        }
        Err(e) => fail_metrics(submit_start, &source_addr, &e.to_string()),
    }
}

fn build_send_args(
    wallet: &StellarWallet,
    destination_chain: &str,
    destination_address: &str,
    payload: &[u8],
    gas_token_contract: &str,
    gas_amount_stroops: u64,
) -> Result<Vec<stellar_xdr::curr::ScVal>> {
    Ok(vec![
        scval_address_account(&wallet.public_key_bytes),
        scval_string(destination_chain)?,
        scval_string(destination_address)?,
        scval_bytes(payload)?,
        scval_token(gas_token_contract, gas_amount_stroops)?,
    ])
}

fn fail_metrics(submit_start: Instant, source: &str, err: &str) -> TxMetrics {
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    TxMetrics {
        signature: String::new(),
        submit_time_ms: elapsed_ms,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        success: false,
        error: Some(err.to_string()),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: source.to_string(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}

// ---------------------------------------------------------------------------
// Burst mode
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn run_burst(
    client: &StellarClient,
    wallets: &[StellarWallet],
    example_contract: String,
    gateway_contract: String,
    destination_chain: &str,
    destination_address: &str,
    payload_override: Option<Vec<u8>>,
    source_chain: &str,
    gas_token_contract: String,
    gas_amount_stroops: u64,
) -> Result<LoadTestReport> {
    let key_count = wallets.len();
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{key_count} confirmed)..."));
    let test_start = Instant::now();

    let client = Arc::new(client.clone());

    let mut tasks = Vec::with_capacity(key_count);
    for w in wallets {
        let c = Arc::clone(&client);
        let w = w.clone();
        let ex = example_contract.clone();
        let gw = gateway_contract.clone();
        let dc = destination_chain.to_string();
        let da = destination_address.to_string();
        let payload = make_payload(&payload_override);
        let gas_token = gas_token_contract.clone();
        let gas = gas_amount_stroops;
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed);
        let sp = spinner.clone();
        let total = key_count;
        let handle = tokio::spawn(async move {
            let m = submit_single(&c, &w, &ex, &gw, &dc, &da, &payload, &gas_token, gas).await;
            if m.success {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                sp.set_message(format!("sending ({done}/{total} confirmed)..."));
            }
            metrics_clone.lock().await.push(m);
        });
        tasks.push(handle);
    }

    let total_submitted = tasks.len() as u64;
    join_all(tasks).await;
    let test_duration = test_start.elapsed().as_secs_f64();
    let confirmed_count = confirmed.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    Ok(build_burst_report(
        metrics,
        source_chain,
        destination_chain,
        destination_address,
        total_submitted,
        test_duration,
        key_count,
    ))
}

#[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
fn build_burst_report(
    metrics: Vec<TxMetrics>,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    total_submitted: u64,
    test_duration: f64,
    key_count: usize,
) -> LoadTestReport {
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;
    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();
    LoadTestReport {
        source_chain: source_chain.to_string(),
        destination_chain: destination_chain.to_string(),
        destination_address: destination_address.to_string(),
        protocol: String::new(),
        tps: None,
        duration_secs: None,
        num_txs: total_submitted,
        num_keys: key_count,
        total_submitted,
        total_confirmed,
        total_failed,
        test_duration_secs: test_duration,
        tps_submitted: if test_duration > 0.0 {
            total_submitted as f64 / test_duration
        } else {
            0.0
        },
        tps_confirmed: if test_duration > 0.0 {
            total_confirmed as f64 / test_duration
        } else {
            0.0
        },
        landing_rate: if total_submitted > 0 {
            total_confirmed as f64 / total_submitted as f64
        } else {
            0.0
        },
        avg_latency_ms: if latencies.is_empty() {
            None
        } else {
            Some(latencies.iter().sum::<u64>() as f64 / latencies.len() as f64)
        },
        min_latency_ms: latencies.iter().min().copied(),
        max_latency_ms: latencies.iter().max().copied(),
        avg_compute_units: None,
        min_compute_units: None,
        max_compute_units: None,
        verification: None,
        transactions: metrics,
    }
}

// ---------------------------------------------------------------------------
// Sustained mode
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_sustained(
    client: &StellarClient,
    wallets: Vec<StellarWallet>,
    example_contract: String,
    gateway_contract: String,
    destination_chain: String,
    destination_address: String,
    payload_override: Option<Vec<u8>>,
    tps: usize,
    duration_secs: u64,
    key_cycle: usize,
    verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    send_done: Option<Arc<AtomicBool>>,
    spinner: ProgressBar,
    has_voting_verifier: bool,
    destination_contract_addr: alloy::primitives::Address,
    gas_token_contract: String,
    gas_amount_stroops: u64,
) -> sustained::SustainedResult {
    let client = Arc::new(client.clone());
    let wallets = Arc::new(wallets);

    let make_task: sustained::MakeTask = Box::new(move |key_idx: usize, _nonce: Option<u64>| {
        let c = Arc::clone(&client);
        let ws = Arc::clone(&wallets);
        let ex = example_contract.clone();
        let gw = gateway_contract.clone();
        let dc = destination_chain.clone();
        let da = destination_address.clone();
        let payload = make_payload(&payload_override);
        let gas_token = gas_token_contract.clone();
        let gas = gas_amount_stroops;
        let vtx = verify_tx.clone();
        let has_vv = has_voting_verifier;
        let contract_addr = destination_contract_addr;

        Box::pin(async move {
            let wallet = &ws[key_idx % ws.len()];
            let m = submit_single(&c, wallet, &ex, &gw, &dc, &da, &payload, &gas_token, gas).await;
            if m.success
                && let Some(ref tx_sender) = vtx
            {
                let pending = super::verify::tx_to_pending_stellar(&m, has_vv, contract_addr);
                let _ = tx_sender.send(pending);
            }
            m
        })
    });

    sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        None,
        make_task,
        send_done,
        spinner,
    )
    .await
}

/// Derive deterministic Stellar wallets from a 32-byte main seed.
/// Uses the same `keccak256(main_seed || index)` pattern as the rest of axe
/// (Solana/EVM/XRPL) so re-runs recover the same ephemeral accounts.
pub fn derive_wallets(main_seed: &[u8; 32], count: usize) -> Result<Vec<StellarWallet>> {
    (0..count)
        .map(|i| {
            let mut seed_input = Vec::with_capacity(40);
            seed_input.extend_from_slice(main_seed);
            seed_input.extend_from_slice(&(i as u64).to_le_bytes());
            let hash = keccak256(&seed_input);
            Ok(StellarWallet::from_seed(hash.as_ref()))
        })
        .collect()
}

/// Ensure all derived wallets are activated (have a minimum XLM balance).
/// Testnet/futurenet: use Friendbot. Mainnet: caller must pre-fund.
pub async fn ensure_funded(
    client: &StellarClient,
    derived: &[StellarWallet],
    use_friendbot: bool,
) -> Result<()> {
    let check_pb = indicatif::ProgressBar::new(derived.len() as u64);
    check_pb.set_style(
        indicatif::ProgressStyle::with_template(
            "  {bar:40.cyan/dim} {pos}/{len} Stellar keys checked",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    let mut missing: Vec<usize> = Vec::new();
    for (i, w) in derived.iter().enumerate() {
        if client.account_sequence(&w.address()).await?.is_none() {
            missing.push(i);
        }
        check_pb.inc(1);
    }
    check_pb.finish_and_clear();

    if missing.is_empty() {
        ui::success(&format!(
            "all {} derived Stellar keys are activated",
            derived.len()
        ));
        return Ok(());
    }

    if !use_friendbot {
        return Err(eyre!(
            "{} Stellar wallet(s) are not activated and no faucet available on this network. \
             Fund them manually first (need a classic `create_account` op).",
            missing.len()
        ));
    }

    ui::info(&format!(
        "funding {}/{} Stellar keys via Friendbot...",
        missing.len(),
        derived.len()
    ));
    let pb = indicatif::ProgressBar::new(missing.len() as u64);
    pb.set_style(
        indicatif::ProgressStyle::with_template(
            "  {bar:40.cyan/dim} {pos}/{len} Stellar keys funded",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    for &i in &missing {
        client
            .friendbot_fund(&derived[i].address())
            .await
            .map_err(|e| eyre!("friendbot fund failed for key {i}: {e}"))?;
        pb.inc(1);
    }
    pb.finish_and_clear();
    ui::success(&format!(
        "funded {} Stellar keys via Friendbot",
        missing.len()
    ));
    Ok(())
}

/// Convenience: derive + fund.
#[allow(dead_code)]
pub async fn prepare_wallets(
    client: &StellarClient,
    main_seed: &[u8; 32],
    count: usize,
    use_friendbot: bool,
) -> Result<Vec<StellarWallet>> {
    if count == 0 {
        return Err(eyre!("Stellar load test needs at least 1 ephemeral wallet"));
    }
    let wallets = derive_wallets(main_seed, count)?;
    ensure_funded(client, &wallets, use_friendbot).await?;
    Ok(wallets)
}

// Suppress unused warnings while the reverse direction is staged.
#[allow(dead_code)]
fn _suppress() -> Result<()> {
    let _ = scval_address_from_str as fn(&str) -> _;
    Ok(())
}
