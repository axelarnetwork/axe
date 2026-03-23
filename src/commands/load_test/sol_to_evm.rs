use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eyre::eyre;
use futures::future::join_all;
use indicatif::ProgressBar;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tokio::sync::Mutex;

use alloy::primitives::keccak256;
use alloy::sol_types::SolValue;
use rand::Rng;

use super::LoadTestArgs;
use super::keypairs;
use super::metrics::{LoadTestReport, TxMetrics};
use crate::solana;
use crate::ui;

/// Generate a unique ABI-encoded payload compatible with `SenderReceiver._execute`.
/// The contract does `abi.decode(payload_, (string))`, so we must ABI-encode the string.
fn make_payload(custom: &Option<Vec<u8>>) -> Vec<u8> {
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


/// Prepare the signing keypairs for the load test.
///
/// When `num_keys >= 2`, derives N keypairs from the main one, funds any that
/// are below the minimum balance, and returns the list. Shows progress bar
/// during funding.
///
/// When `num_keys <= 1`, returns the main keypair as the only signer.
fn prepare_keypairs(
    solana_rpc: &str,
    num_keys: usize,
    main_keypair: &Keypair,
) -> eyre::Result<Vec<Arc<dyn Signer + Send + Sync>>> {
    if num_keys <= 1 {
        return Ok(vec![Arc::new(Keypair::new_from_array(
            main_keypair.to_bytes()[..32].try_into().unwrap(),
        )) as Arc<dyn Signer + Send + Sync>]);
    }

    let derived = keypairs::derive_keypairs(main_keypair, num_keys)?;
    let balances = keypairs::ensure_funded(solana_rpc, main_keypair, &derived)?;

    #[allow(clippy::float_arithmetic)]
    let total_sol: f64 = balances.iter().sum::<u64>() as f64 / 1e9;
    ui::success(&format!(
        "funded {} keys ({:.4} SOL)",
        derived.len(),
        total_sol,
    ));

    Ok(derived
        .into_iter()
        .map(|kp| Arc::new(kp) as Arc<dyn Signer + Send + Sync>)
        .collect())
}

/// Run load test and return metrics report.
#[allow(clippy::too_many_lines, clippy::float_arithmetic)]
pub async fn run_load_test_with_metrics(
    args: &LoadTestArgs,
    destination_address: &str,
) -> eyre::Result<LoadTestReport> {
    let num_txs = args.num_txs.max(1) as usize;

    let main_keypair = solana::load_keypair(args.keypair.as_deref())?;

    // Check main wallet balance
    let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        &args.solana_rpc,
        solana_commitment_config::CommitmentConfig::confirmed(),
    );
    let pubkey = main_keypair.pubkey();
    let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
    #[allow(clippy::float_arithmetic)]
    let sol = balance as f64 / 1e9;
    ui::kv("wallet", &format!("{pubkey} ({sol:.4} SOL)"));
    if balance == 0 {
        return Err(eyre!(
            "wallet ({pubkey}) has no SOL. Fund it first:\n  solana airdrop 2 {pubkey}"
        ));
    }

    // Derive and fund keypairs (1 key per tx to avoid nonce contention)
    let keypairs = prepare_keypairs(&args.solana_rpc, num_txs, &main_keypair)?;
    let keypairs = Arc::new(keypairs);
    let key_count = keypairs.len();

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let mut pending_tasks = Vec::new();

    let test_start = Instant::now();
    let solana_rpc = args.solana_rpc.clone();

    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{key_count} confirmed)..."));

    // Fire all txs in parallel (one per keypair)
    for i in 0..key_count {
        let kp = Arc::clone(&keypairs[i]);
        let dest_chain = args.destination_chain.clone();
        let dest_addr = destination_address.to_string();
        let tx_payload = make_payload(&payload);
        let metrics_clone = Arc::clone(&metrics_list);
        let rpc = solana_rpc.clone();
        let counter = Arc::clone(&confirmed_counter);
        let sp = spinner.clone();
        let total = key_count;

        let handle = tokio::spawn(async move {
            execute_and_record(
                &rpc,
                kp,
                &dest_chain,
                &dest_addr,
                &tx_payload,
                metrics_clone,
                counter,
                sp,
                total,
            )
            .await;
        });
        pending_tasks.push(handle);
    }

    let total_submitted = pending_tasks.len() as u64;
    let test_duration = test_start.elapsed().as_secs_f64();

    join_all(pending_tasks).await;
    let confirmed_count = confirmed_counter.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!("sent {confirmed_count}/{total_submitted} confirmed"));

    let metrics = metrics_list.lock().await.clone();
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;

    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();
    let compute_units: Vec<u64> = metrics.iter().filter_map(|m| m.compute_units).collect();

    #[allow(clippy::cast_precision_loss)]
    let report = LoadTestReport {
        source_chain: args.source_chain.clone(),
        destination_chain: args.destination_chain.clone(),
        destination_address: destination_address.to_string(),
        num_txs: args.num_txs,
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
            Option::None
        } else {
            Some(latencies.iter().sum::<u64>() as f64 / latencies.len() as f64)
        },
        min_latency_ms: latencies.iter().min().copied(),
        max_latency_ms: latencies.iter().max().copied(),
        avg_compute_units: if compute_units.is_empty() {
            Option::None
        } else {
            Some(compute_units.iter().sum::<u64>() as f64 / compute_units.len() as f64)
        },
        min_compute_units: compute_units.iter().min().copied(),
        max_compute_units: compute_units.iter().max().copied(),
        verification: Option::None,
        transactions: metrics,
    };

    Ok(report)
}

/// Send a single Solana callContract tx and return metrics. Used by sustained mode.
fn send_sol_tx(
    solana_rpc: &str,
    keypair: &(dyn Signer + Send + Sync),
    dest_chain: &str,
    dest_addr: &str,
    payload: &[u8],
) -> TxMetrics {
    let submit_start = Instant::now();
    let source_addr = keypair.pubkey().to_string();
    let payload_hash = alloy::hex::encode(alloy::primitives::keccak256(payload));
    match solana::send_call_contract(solana_rpc, keypair, dest_chain, dest_addr, payload) {
        Ok((_sig, mut metrics)) => {
            metrics.payload = payload.to_vec();
            metrics.payload_hash = payload_hash;
            metrics.source_address = source_addr;
            metrics.send_instant = Some(submit_start);
            metrics
        }
        Err(e) => {
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
                error: Some(e.to_string()),
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: String::new(),
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            }
        }
    }
}

/// Run Sol->EVM sustained load test at a controlled TPS rate.
#[allow(clippy::too_many_lines, clippy::float_arithmetic)]
pub async fn run_sustained_load_test_with_metrics(
    args: &LoadTestArgs,
    destination_address: &str,
) -> eyre::Result<LoadTestReport> {
    let tps = args.tps.unwrap() as usize;
    let duration_secs = args.duration_secs.unwrap();
    let key_cycle = args.key_cycle as usize;
    let pool_size = tps * key_cycle;
    let total_expected = tps as u64 * duration_secs;

    let main_keypair = solana::load_keypair(args.keypair.as_deref())?;
    let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        &args.solana_rpc,
        solana_commitment_config::CommitmentConfig::confirmed(),
    );
    let pubkey = main_keypair.pubkey();
    let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
    #[allow(clippy::float_arithmetic)]
    let sol = balance as f64 / 1e9;
    ui::kv("wallet", &format!("{pubkey} ({sol:.4} SOL)"));
    if balance == 0 {
        return Err(eyre!(
            "wallet ({pubkey}) has no SOL. Fund it first:\n  solana airdrop 2 {pubkey}"
        ));
    }

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    // Derive and fund pool
    let keypairs_pool = prepare_keypairs(&args.solana_rpc, pool_size, &main_keypair)?;
    let keypairs_pool = Arc::new(keypairs_pool);
    ui::info(&format!(
        "derived {} Solana signing keys (pool: {} tx/s × {}s cycle)",
        pool_size, tps, key_cycle
    ));

    // Shared counters
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let src_confirmed = Arc::new(AtomicU64::new(0));
    let src_failed = Arc::new(AtomicU64::new(0));
    let fired_ctr = Arc::new(AtomicU64::new(0));

    let spinner = ui::wait_spinner(&format!("[0/{duration_secs}s] starting sustained send..."));
    let test_start = Instant::now();

    let dest_chain = args.destination_chain.clone();
    let dest_addr = destination_address.to_string();
    let solana_rpc = args.solana_rpc.clone();

    let mut all_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(total_expected as usize);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut tick: u64 = 0;
    loop {
        interval.tick().await;
        if tick >= duration_secs {
            break;
        }

        let batch_start = (tick as usize % key_cycle) * tps;
        for i in 0..tps {
            let key_idx = batch_start + i;
            let kp = Arc::clone(&keypairs_pool[key_idx]);
            let tx_payload = make_payload(&payload);

            let metrics_clone = Arc::clone(&metrics_list);
            let confirmed_ctr = Arc::clone(&src_confirmed);
            let failed_ctr = Arc::clone(&src_failed);
            let fired = Arc::clone(&fired_ctr);
            let dc = dest_chain.clone();
            let da = dest_addr.clone();
            let rpc = solana_rpc.clone();

            fired.fetch_add(1, Ordering::Relaxed);

            let handle = tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    send_sol_tx(&rpc, kp.as_ref(), &dc, &da, &tx_payload)
                }).await.unwrap_or_else(|e| {
                    #[allow(clippy::cast_possible_truncation)]
                    TxMetrics {
                        signature: String::new(),
                        submit_time_ms: 0,
                        confirm_time_ms: None,
                        latency_ms: None,
                        compute_units: None,
                        slot: None,
                        success: false,
                        error: Some(format!("task panicked: {e}")),
                        payload: Vec::new(),
                        payload_hash: String::new(),
                        source_address: String::new(),
                        gmp_destination_chain: String::new(),
                        gmp_destination_address: String::new(),
                        send_instant: None,
                        amplifier_timing: None,
                    }
                });
                if result.success {
                    confirmed_ctr.fetch_add(1, Ordering::Relaxed);
                } else {
                    failed_ctr.fetch_add(1, Ordering::Relaxed);
                }
                metrics_clone.lock().await.push(result);
            });
            all_tasks.push(handle);
        }

        // Update live display
        let elapsed_s = test_start.elapsed().as_secs();
        let f = fired_ctr.load(Ordering::Relaxed);
        let c = src_confirmed.load(Ordering::Relaxed);
        let fail = src_failed.load(Ordering::Relaxed);
        spinner.set_message(format!(
            "[{elapsed_s}/{duration_secs}s]  fired: {f}/{total_expected}  src-confirmed: {c}  failed: {fail}  (target: {tps} tx/s)"
        ));
        tick += 1;
    }

    let total_submitted = all_tasks.len() as u64;
    spinner.set_message(format!(
        "waiting for {} in-flight receipts...",
        total_submitted.saturating_sub(src_confirmed.load(Ordering::Relaxed) + src_failed.load(Ordering::Relaxed))
    ));
    join_all(all_tasks).await;

    let test_duration = test_start.elapsed().as_secs_f64();
    let confirmed_count = src_confirmed.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "send phase complete: {confirmed_count}/{total_submitted} src-confirmed in {test_duration:.1}s"
    ));

    let metrics = metrics_list.lock().await.clone();
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;

    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.success) {
            let reason = m.error.as_deref().unwrap_or("unknown").chars().take(120).collect::<String>();
            *error_counts.entry(reason).or_default() += 1;
        }
        for (reason, count) in &error_counts {
            ui::warn(&format!("{count} txs failed: {reason}"));
        }
    }

    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();
    let compute_units: Vec<u64> = metrics.iter().filter_map(|m| m.compute_units).collect();

    #[allow(clippy::cast_precision_loss)]
    let report = LoadTestReport {
        source_chain: args.source_chain.clone(),
        destination_chain: args.destination_chain.clone(),
        destination_address: destination_address.to_string(),
        num_txs: total_expected,
        num_keys: pool_size,
        total_submitted,
        total_confirmed,
        total_failed,
        test_duration_secs: test_duration,
        tps_submitted: if test_duration > 0.0 { total_submitted as f64 / test_duration } else { 0.0 },
        tps_confirmed: if test_duration > 0.0 { total_confirmed as f64 / test_duration } else { 0.0 },
        landing_rate: if total_submitted > 0 { total_confirmed as f64 / total_submitted as f64 } else { 0.0 },
        avg_latency_ms: if latencies.is_empty() { None } else { Some(latencies.iter().sum::<u64>() as f64 / latencies.len() as f64) },
        min_latency_ms: latencies.iter().min().copied(),
        max_latency_ms: latencies.iter().max().copied(),
        avg_compute_units: if compute_units.is_empty() { None } else { Some(compute_units.iter().sum::<u64>() as f64 / compute_units.len() as f64) },
        min_compute_units: compute_units.iter().min().copied(),
        max_compute_units: compute_units.iter().max().copied(),
        verification: None,
        transactions: metrics,
    };

    Ok(report)
}

#[allow(clippy::semicolon_outside_block, clippy::too_many_arguments)]
async fn execute_and_record(
    solana_rpc: &str,
    keypair: Arc<dyn Signer + Send + Sync>,
    dest_chain: &str,
    dest_addr: &str,
    payload: &[u8],
    metrics_list: Arc<Mutex<Vec<TxMetrics>>>,
    confirmed_counter: Arc<AtomicU64>,
    spinner: ProgressBar,
    total: usize,
) {
    let submit_start = Instant::now();

    let source_addr = keypair.pubkey().to_string();
    let payload_hash = alloy::hex::encode(keccak256(payload));

    match solana::send_call_contract(solana_rpc, keypair.as_ref(), dest_chain, dest_addr, payload) {
        Ok((_sig, mut metrics)) => {
            metrics.payload = payload.to_vec();
            metrics.payload_hash = payload_hash;
            metrics.source_address = source_addr;
            metrics.send_instant = Some(submit_start);
            let done = confirmed_counter.fetch_add(1, Ordering::Relaxed) + 1;
            spinner.set_message(format!("sending ({done}/{total} confirmed)..."));
            metrics_list.lock().await.push(metrics);
        }
        Err(e) => {
            #[allow(clippy::cast_possible_truncation)]
            let elapsed_ms = submit_start.elapsed().as_millis() as u64;
            let metrics = TxMetrics {
                signature: String::new(),
                submit_time_ms: elapsed_ms,
                confirm_time_ms: Option::None,
                latency_ms: Option::None,
                compute_units: Option::None,
                slot: Option::None,
                success: false,
                error: Some(e.to_string()),
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: String::new(),
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            };
            metrics_list.lock().await.push(metrics);
        }
    }
}
