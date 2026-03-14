use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::{
    primitives::{Address, Bytes, FixedBytes, keccak256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
    sol_types::{SolEvent, SolValue},
};
use eyre::eyre;
use futures::future::join_all;
use rand::Rng;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

use super::LoadTestArgs;
use super::keypairs;
use super::metrics::{LoadTestReport, TxMetrics};
use crate::evm::{AxelarAmplifierGateway, ContractCall};
use crate::ui;

/// Solana memo program address (devnet).
pub const MEMO_PROGRAM_ADDRESS: &str = "memKnP9ex71TveNFpsFNVqAYGEe1v9uHVsHNdFPW6FY";

// Solana ExecutablePayload ABI types (matches axelar-amplifier-solana gateway)
sol! {
    struct SolanaAccountRepr {
        bytes32 pubkey;
        bool is_signer;
        bool is_writable;
    }

    struct SolanaGatewayPayload {
        bytes execute_payload;
        SolanaAccountRepr[] accounts;
    }
}

/// Build an ExecutablePayload in ABI encoding format for the memo program.
///
/// Format: [0x01 (ABI scheme)] [ABI-encoded SolanaGatewayPayload]
/// The memo program needs the counter PDA as a writable account.
fn make_executable_payload(custom: &Option<Vec<u8>>, counter_pda: &Pubkey) -> Vec<u8> {
    let memo_bytes: Vec<u8> = match custom {
        Some(p) => p.clone(),
        None => {
            let mut buf = [0u8; 16];
            rand::thread_rng().fill(&mut buf);
            format!("hello from axe load test {}", hex::encode(buf)).into_bytes()
        }
    };

    let gateway_payload = SolanaGatewayPayload {
        execute_payload: memo_bytes.into(),
        accounts: vec![SolanaAccountRepr {
            pubkey: FixedBytes::from(counter_pda.to_bytes()),
            is_signer: false,
            is_writable: true,
        }],
    };

    let encoded = gateway_payload.abi_encode_params();

    // Prepend ABI encoding scheme byte (0x01)
    let mut full_payload = Vec::with_capacity(1 + encoded.len());
    full_payload.push(0x01);
    full_payload.extend(encoded);
    full_payload
}

/// Run EVM->Sol load test with parallel sends from derived wallets.
///
/// Derives N EVM signers from the main private key, funds them, then fires
/// all callContract() txs in parallel (one per derived wallet).
#[allow(clippy::too_many_arguments, clippy::float_arithmetic)]
pub async fn run_load_test_with_metrics(
    args: &LoadTestArgs,
    gateway_addr: Address,
    main_key: &[u8; 32],
    evm_rpc_url: &str,
    destination_address: &str,
) -> eyre::Result<LoadTestReport> {
    let num_txs = args.num_txs.max(1) as usize;

    // Derive the memo program's counter PDA
    let memo_program_id = Pubkey::from_str(MEMO_PROGRAM_ADDRESS)
        .map_err(|e| eyre!("invalid memo program address: {e}"))?;
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_program_id);

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };

    // Derive N EVM signers from main private key
    let derived = keypairs::derive_evm_signers(main_key, num_txs)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    // Fund derived wallets from main wallet
    let main_signer = PrivateKeySigner::from_bytes(&(*main_key).into())
        .map_err(|e| eyre!("invalid main EVM key: {e}"))?;
    let funding_provider = ProviderBuilder::new()
        .wallet(main_signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    keypairs::ensure_funded_evm(&funding_provider, &main_signer, &derived).await?;

    // Fire txs in parallel, capped to avoid overwhelming the RPC.
    // Each send does multiple RPC calls (estimate gas, nonce, send, receipt),
    // so even 10 concurrent senders means ~40+ RPC calls in flight.
    const MAX_CONCURRENT_SENDS: usize = 100;
    const MAX_RETRIES: u32 = 5;

    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));
    let test_start = Instant::now();

    let mut tasks = Vec::with_capacity(num_txs);
    let dest_chain = args.destination_chain.clone();
    let dest_addr = destination_address.to_string();

    for signer in &derived {
        let tx_payload = make_executable_payload(&payload, &counter_pda);
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_txs;
        let dc = dest_chain.clone();
        let da = dest_addr.clone();
        let gw = gateway_addr;

        // Each task gets its own provider with its own signer — no nonce contention
        let provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect_http(evm_rpc_url.parse()?);
        let signer_addr = signer.address();

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            // Retry with exponential backoff on rate-limit (429) errors
            let mut m = None;
            for attempt in 0..=MAX_RETRIES {
                let result =
                    execute_and_record_evm(&provider, gw, signer_addr, &dc, &da, &tx_payload).await;

                if result.success || attempt == MAX_RETRIES {
                    m = Some(result);
                    break;
                }

                // Only retry on 429 errors
                let is_rate_limited = result.error.as_deref().is_some_and(|e| e.contains("429"));
                if !is_rate_limited {
                    m = Some(result);
                    break;
                }

                // Exponential backoff: 1s, 2s, 4s, 8s, 16s
                let backoff = Duration::from_secs(1 << attempt);
                tokio::time::sleep(backoff).await;
            }

            let m = m.unwrap();
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

    let confirmed_count = confirmed_counter.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;

    // Show error breakdown if there were failures
    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.success) {
            let reason = m
                .error
                .as_deref()
                .unwrap_or("unknown")
                .chars()
                .take(120)
                .collect::<String>();
            *error_counts.entry(reason).or_default() += 1;
        }
        for (reason, count) in &error_counts {
            ui::warn(&format!("{count} txs failed: {reason}"));
        }
    }

    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();

    #[allow(clippy::cast_precision_loss)]
    let report = LoadTestReport {
        source_chain: args.source_chain.clone(),
        destination_chain: args.destination_chain.clone(),
        destination_address: dest_addr,
        num_txs: args.num_txs,
        num_keys: num_txs,
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
    };

    Ok(report)
}

/// Send a single callContract tx on the EVM gateway and return metrics.
async fn execute_and_record_evm<P: Provider>(
    provider: &P,
    gateway_addr: Address,
    signer_address: Address,
    dest_chain: &str,
    dest_addr: &str,
    payload: &[u8],
) -> TxMetrics {
    let submit_start = Instant::now();
    let payload_hash = alloy::hex::encode(keccak256(payload));

    let gateway = AxelarAmplifierGateway::new(gateway_addr, provider);
    let call = gateway.callContract(
        dest_chain.to_string(),
        dest_addr.to_string(),
        Bytes::from(payload.to_vec()),
    );

    match call.send().await {
        Ok(pending) => {
            let tx_hash = *pending.tx_hash();
            match tokio::time::timeout(std::time::Duration::from_secs(120), pending.get_receipt())
                .await
            {
                Ok(Ok(receipt)) => {
                    #[allow(clippy::cast_possible_truncation)]
                    let latency_ms = submit_start.elapsed().as_millis() as u64;

                    // Extract ContractCall event index
                    let event_index = receipt
                        .inner
                        .logs()
                        .iter()
                        .enumerate()
                        .find_map(|(i, log)| {
                            if log.topics().first() == Some(&ContractCall::SIGNATURE_HASH) {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);

                    // message_id format matching Axelar convention
                    let message_id = format!("{tx_hash:#x}-{event_index}");

                    TxMetrics {
                        signature: message_id,
                        submit_time_ms: 0,
                        confirm_time_ms: Some(latency_ms),
                        latency_ms: Some(latency_ms),
                        compute_units: Some(receipt.gas_used),
                        slot: receipt.block_number,
                        success: true,
                        error: None,
                        payload: payload.to_vec(),
                        payload_hash,
                        source_address: format!("{signer_address}"),
                        gmp_destination_chain: String::new(),
                        gmp_destination_address: String::new(),
                        send_instant: Some(submit_start),
                        amplifier_timing: None,
                    }
                }
                Ok(Err(e)) => make_failure(submit_start, &e.to_string()),
                Err(_) => make_failure(submit_start, "tx timed out"),
            }
        }
        Err(e) => make_failure(submit_start, &e.to_string()),
    }
}

fn make_failure(submit_start: Instant, error: &str) -> TxMetrics {
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
        error: Some(error.to_string()),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}
