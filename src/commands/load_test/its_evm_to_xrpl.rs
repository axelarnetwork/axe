//! EVM -> XRPL ITS load test.
//!
//! Source-side flow is identical to `its_evm_to_sol.rs` (deploy/cache the
//! AXE token on the EVM source, distribute to derived signers, fire
//! `interchainTransfer` calls). The destination side polls the recipient
//! XRPL account's `account_tx` for an inbound `Payment` whose `message_id`
//! memo matches the second-leg id (the XRPL relayer attaches that memo).
//!
//! Token: requires the user to supply `--token-id <hex>` for the
//! interchain token registered between the EVM source and the XRPL gateway
//! (typically the canonical XRP token id, or a custom-registered IOU).
//! Native XRP works with no trust-line setup needed on the recipient side.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::{
    primitives::{Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;
use futures::future::join_all;
use tokio::sync::{Mutex, Semaphore};

use super::keypairs;
use super::metrics::{LoadTestReport, TxMetrics};
use super::{LoadTestArgs, check_evm_balance, finish_report, validate_evm_rpc};
use crate::cosmos::{lcd_cosmwasm_smart_query, read_axelar_config, read_axelar_contract_field};
use crate::evm::InterchainTokenService;
use crate::ui;
use crate::utils::read_contract_address;
use crate::xrpl::{XrplClient, XrplWallet, faucet_url_for_network};

#[cfg(feature = "devnet-amplifier")]
fn default_gas_value_wei(_source_chain: &str) -> u128 {
    0
}
#[cfg(not(feature = "devnet-amplifier"))]
fn default_gas_value_wei(source_chain: &str) -> u128 {
    if source_chain.starts_with("flow") {
        300_000_000_000_000_000
    } else {
        10_000_000_000_000_000
    }
}

const MAX_CONCURRENT_SENDS: usize = 100;
const MAX_RETRIES: u32 = 5;

/// Per-tx transfer amount (in token's smallest unit). Kept tiny so a
/// 3000-tx run costs minimal real funds on mainnet. axlXRP on EVM has 18
/// decimals; ITS truncates to 6 decimals on the XRPL side, so we use
/// 0.001 axlXRP = 1e15 wei → 1000 drops on XRPL.
const AMOUNT_PER_TX_WEI: u128 = 1_000_000_000_000_000;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    // XRPL uses `XrplGateway/{xrpl_axelar_id}` instead of the standard
    // `Gateway/{chain}` — accept either.
    let has_dest_gateway = read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_ok()
        || read_axelar_contract_field(
            &args.config,
            &format!(
                "/axelar/contracts/XrplGateway/{}/address",
                args.destination_axelar_id
            ),
        )
        .is_ok();
    if !has_dest_gateway {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway (or XrplGateway) in the config — verification would fail."
        );
    }
    if read_axelar_contract_field(&args.config, "/axelar/contracts/AxelarnetGateway/address")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    // --- Token ID: use --token-id if provided, else auto-discover the
    // canonical XRP token from the XrplGateway CosmWasm contract. ---
    let token_id: FixedBytes<32> = if let Some(tid) = args.token_id.as_deref() {
        let parsed: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        ui::kv("token ID (provided)", tid);
        parsed
    } else {
        ui::info("looking up canonical XRP token id on XrplGateway...");
        match fetch_xrp_token_id(&args.config, &args.destination_axelar_id).await {
            Ok(id) => {
                ui::kv(
                    "token ID (XrplGateway → XRP)",
                    &format!("0x{}", hex::encode(id)),
                );
                FixedBytes::<32>::from(id)
            }
            Err(e) => {
                eyre::bail!(
                    "auto-discovery of XRP token id failed: {e}\n\
                     Workaround: pass `--token-id 0x<hex>` explicitly. \
                     The canonical id is the same across all networks where XRP is registered; \
                     for testnet/mainnet/stagenet the value is \
                     `0xba5a21ca88ef6bba2bfff5088994f90e1077e2a1cc3dcc38bd261f00fce2824f`."
                );
            }
        }
    };

    // --- EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;
    let main_key: [u8; 32] = signer.to_bytes().into();

    let its_proxy_addr = read_contract_address(&args.config, src, "InterchainTokenService")?;
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    let write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    // Verify the token is actually registered on the source EVM ITS.
    let its_service = InterchainTokenService::new(its_proxy_addr, &write_provider);
    let token_addr = its_service
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| {
            eyre!(
                "token id 0x{} not registered on EVM ITS: {e}",
                hex::encode(token_id)
            )
        })?;
    ui::address("token address (EVM)", &format!("{token_addr}"));

    // --- XRPL recipient setup ---
    let (xrpl_rpc, _xrpl_multisig, xrpl_network_type) =
        super::its_xrpl_to_evm::read_xrpl_chain_config(&args.config, dest)?;
    let xrpl_client = XrplClient::new(&xrpl_rpc);

    // Recipient: derive deterministically from XRPL_PRIVATE_KEY (or generate
    // a fresh wallet if not set — only viable on testnet with friendbot).
    let recipient_wallet = match std::env::var("XRPL_PRIVATE_KEY")
        .ok()
        .or_else(|| args.private_key.clone())
    {
        Some(k) if k.len() == 64 || k.len() == 66 => XrplWallet::from_hex(&k)?,
        _ => {
            // Derive from the EVM main key so it's deterministic across runs.
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&main_key);
            XrplWallet::from_bytes(&seed)?
        }
    };
    let recipient_addr = recipient_wallet.address();
    ui::address("XRPL recipient", &recipient_addr);

    // Activate the recipient if needed (testnet/devnet only).
    // Detect faucet from the actual RPC URL — devnet-amplifier mislabels
    // its xrpl networkType as "testnet" but uses a different ledger.
    if xrpl_client.account_info(&recipient_addr).await?.is_none() {
        if let Some(faucet) =
            faucet_url_for_network(&xrpl_rpc).or_else(|| faucet_url_for_network(&xrpl_network_type))
        {
            ui::info("activating XRPL recipient via faucet...");
            xrpl_client
                .fund_from_faucet(&recipient_addr, faucet)
                .await?;
            ui::success("recipient activated");
        } else {
            eyre::bail!(
                "XRPL recipient {recipient_addr} is not activated. Fund it with at least the \
                 base reserve (~10 XRP) before running on this network."
            );
        }
    }

    // --- Gas value ---
    let gas_value_wei: u128 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value_wei(src),
    };
    let gas_value = U256::from(gas_value_wei);
    #[allow(clippy::float_arithmetic)]
    {
        ui::kv(
            "gas value",
            &format!(
                "{gas_value_wei} wei ({:.6} ETH)",
                gas_value_wei as f64 / 1e18
            ),
        );
    }

    // --- Burst vs sustained ---
    let burst_mode = !(args.tps.is_some() && args.duration_secs.is_some());
    let (num_keys, total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let tps = args.tps.unwrap() as usize;
        let dur = args.duration_secs.unwrap();
        (tps * args.key_cycle as usize, tps as u64 * dur)
    };

    // --- Derive + fund EVM signers ---
    let derived = keypairs::derive_evm_signers(&main_key, num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));
    let funding_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    // Compute funding dynamically: each interchainTransfer costs roughly
    // GAS_LIMIT × gas_price for the call plus `gas_value` (msg.value to gas
    // service). xrpl-evm runs at ~137 gwei vs ~1 gwei on most EVM testnets,
    // so the static defaults underfund by 5-10×.
    let gas_price_wei: u128 = funding_provider
        .get_gas_price()
        .await
        .unwrap_or(1_000_000_000); // 1 gwei fallback
    const ITS_GAS_LIMIT: u128 = 1_000_000; // generous upper bound for ITS
    let per_tx_native_cost = gas_price_wei.saturating_mul(ITS_GAS_LIMIT) + gas_value_wei;
    let txs_per_key: u128 = if burst_mode {
        1
    } else {
        let dur = args.duration_secs.unwrap();
        let rounds = dur.div_ceil(args.key_cycle);
        (rounds + rounds / 5 + 1) as u128
    };
    // 2× safety multiplier in case gas price doubles mid-test.
    let gas_extra_per_key = per_tx_native_cost
        .saturating_mul(txs_per_key)
        .saturating_mul(2);
    #[allow(clippy::float_arithmetic)]
    {
        ui::kv(
            "per-key budget",
            &format!(
                "{:.6} ETH (gas-price {:.1} gwei × {ITS_GAS_LIMIT} × {txs_per_key} txs + {:.6} ETH msg.value × {txs_per_key}, ×2 buffer)",
                gas_extra_per_key as f64 / 1e18,
                gas_price_wei as f64 / 1e9,
                gas_value_wei as f64 / 1e18,
            ),
        );
    }
    keypairs::ensure_funded_evm_with_extra(&funding_provider, &signer, &derived, gas_extra_per_key)
        .await?;

    // --- Distribute the interchain token to derived signers ---
    let amount_per_tx = U256::from(AMOUNT_PER_TX_WEI);
    let amount_per_key = if burst_mode {
        amount_per_tx
    } else {
        let txs_per_key = args.duration_secs.unwrap().div_ceil(args.key_cycle) + 1;
        amount_per_tx * U256::from(txs_per_key)
    };
    let token_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    super::its_evm_to_sol::distribute_tokens(&token_provider, token_addr, &derived, amount_per_key)
        .await?;

    // --- destination_address bytes for `interchainTransfer` ---
    // For XRPL destinations, ITS expects `asciiToBytes(r-address)` in the
    // destination_address arg. The relayer parses the bytes back to a
    // string and decodes the recipient AccountId.
    let receiver_bytes = Bytes::from(recipient_addr.as_bytes().to_vec());

    // === SUSTAINED MODE ===
    if !burst_mode {
        let tps = args.tps.unwrap() as usize;
        let duration_secs = args.duration_secs.unwrap();
        let key_cycle = args.key_cycle as usize;
        let rpc_url_str = evm_rpc_url.clone();

        let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
        let mut nonces: Vec<u64> = Vec::with_capacity(num_keys);
        for s in &derived {
            let n = nonce_provider.get_transaction_count(s.address()).await?;
            nonces.push(n);
        }

        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        let has_voting_verifier = read_axelar_contract_field(
            &args.config,
            &format!(
                "/axelar/contracts/VotingVerifier/{}/address",
                args.source_axelar_id
            ),
        )
        .is_ok();

        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vxrpl_rpc = xrpl_rpc.clone();
        let vrecipient = recipient_addr.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            super::verify::verify_onchain_xrpl_its_streaming(
                &vconfig,
                &vsource,
                &vdest,
                &vxrpl_rpc,
                &vrecipient,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let spinner = ui::wait_spinner(&format!(
            "[0/{duration_secs}s] starting sustained ITS send..."
        ));
        let _ = spinner_tx.send(spinner.clone());

        let test_start = Instant::now();
        let dest_chain_s = args.destination_axelar_id.clone();

        let make_task: super::sustained::MakeTask =
            Box::new(move |key_idx: usize, nonce: Option<u64>| {
                let dc = dest_chain_s.clone();
                let gv = gas_value;
                let rb = receiver_bytes.clone();
                let amt = amount_per_tx;
                let its_proxy = its_proxy_addr;
                let tid = token_id;
                let url = rpc_url_str.clone();
                let vtx = verify_tx.clone();
                let has_vv = has_voting_verifier;

                let provider = ProviderBuilder::new()
                    .wallet(derived[key_idx].clone())
                    .connect_http(url.parse().expect("invalid RPC URL"));

                Box::pin(async move {
                    let result = super::its_evm_to_sol::execute_interchain_transfer(
                        &provider, its_proxy, tid, &dc, &rb, amt, gv, nonce,
                    )
                    .await;
                    if result.success {
                        let pending = super::verify::tx_to_pending_its(&result, has_vv);
                        let _ = vtx.send(pending);
                    }
                    result
                })
            });

        let result = super::sustained::run_sustained_loop(
            tps,
            duration_secs,
            key_cycle,
            Some(nonces),
            make_task,
            Some(send_done),
            spinner,
        )
        .await;

        let mut report = super::sustained::build_sustained_report(
            result,
            src,
            dest,
            &recipient_addr,
            total_expected,
            num_keys,
        );

        let (verification, timings) = verify_handle.await??;
        for (msg_id, timing) in timings {
            if let Some(tx) = report
                .transactions
                .iter_mut()
                .find(|t| t.signature == msg_id)
            {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        return finish_report(&args, &mut report, test_start);
    }

    // === BURST MODE ===
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_keys} confirmed)..."));
    let test_start = Instant::now();

    let mut tasks = Vec::with_capacity(num_keys);
    let dest_chain = args.destination_axelar_id.clone();

    for derived_signer in &derived {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_keys;
        let dc = dest_chain.clone();
        let gv = gas_value;
        let rb = receiver_bytes.clone();
        let amt = amount_per_tx;
        let its_proxy = its_proxy_addr;
        let tid = token_id;

        let provider = ProviderBuilder::new()
            .wallet(derived_signer.clone())
            .connect_http(evm_rpc_url.parse()?);

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let mut m = None;
            for attempt in 0..=MAX_RETRIES {
                let result = super::its_evm_to_sol::execute_interchain_transfer(
                    &provider, its_proxy, tid, &dc, &rb, amt, gv, None,
                )
                .await;
                if result.success || attempt == MAX_RETRIES {
                    m = Some(result);
                    break;
                }
                let is_rate_limited = result.error.as_deref().is_some_and(|e| e.contains("429"));
                if !is_rate_limited {
                    m = Some(result);
                    break;
                }
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
    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();

    #[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
    let mut report = LoadTestReport {
        source_chain: src.to_string(),
        destination_chain: dest.to_string(),
        destination_address: recipient_addr.clone(),
        protocol: String::new(),
        tps: None,
        duration_secs: None,
        num_txs: args.num_txs,
        num_keys,
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

    let verification = super::verify::verify_onchain_xrpl_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &xrpl_rpc,
        &recipient_addr,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

/// Query the `XrplGateway/{xrpl_axelar_id}` contract for the canonical XRP
/// token id via the `XrpTokenId` view. Matches the TS `xrpl-token-id.js`
/// reference. Returns the raw 32 bytes.
async fn fetch_xrp_token_id(
    config: &std::path::Path,
    xrpl_axelar_id: &str,
) -> eyre::Result<[u8; 32]> {
    let (lcd, _, _, _) = read_axelar_config(config)?;
    let xrpl_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/XrplGateway/{xrpl_axelar_id}/address"),
    )
    .map_err(|e| {
        eyre!(
            "no XrplGateway/{xrpl_axelar_id} address in config — required to auto-discover \
             the canonical XRP token id. Pass --token-id <hex> explicitly to skip this lookup. \
             ({e})"
        )
    })?;
    // `cw_serde` serializes unit enum variants as a plain JSON string (NOT
    // `{"variant": {}}`), so the smart-query body for `XrpTokenId` is just
    // the JSON string `"xrp_token_id"`.
    let q = serde_json::Value::String("xrp_token_id".to_string());
    let resp = lcd_cosmwasm_smart_query(&lcd, &xrpl_gateway, &q).await?;
    let s = resp
        .as_str()
        .ok_or_else(|| eyre!("XrpTokenId response was not a string: {resp}"))?;
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|e| eyre!("XrpTokenId hex decode failed: {e} (got {s:?})"))?;
    if bytes.len() != 32 {
        return Err(eyre!(
            "XrpTokenId returned {} bytes, expected 32: {s}",
            bytes.len()
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}
