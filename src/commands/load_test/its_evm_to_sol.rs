use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long to wait for an EVM tx receipt before giving up.
/// Flow confirms in ~8s; other chains typically <20s. 30s catches everything real
/// without wasting time on txs that were silently dropped by the mempool.
const EVM_RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;
use futures::future::join_all;
use tokio::sync::{Mutex, Semaphore};

use super::keypairs;
use super::metrics::{LoadTestReport, TxMetrics};
use super::{
    LoadTestArgs, check_evm_balance, finish_report, read_its_cache, save_its_cache,
    validate_evm_rpc, validate_solana_rpc,
};
use crate::commands::test_its::{
    extract_contract_call_event, extract_token_deployed_event, generate_salt,
};
use crate::cosmos::read_axelar_contract_field;
use crate::evm::{ERC20, InterchainTokenFactory, InterchainTokenService};
use crate::ui;
use crate::utils::read_contract_address;

const TOKEN_NAME: &str = "AXE";
const TOKEN_SYMBOL: &str = "AXE";
const TOKEN_DECIMALS: u8 = 18;
/// Default gas value for ITS cross-chain transfers.
#[cfg(feature = "devnet-amplifier")]
fn default_gas_value_wei(_source_chain: &str) -> u128 {
    0 // devnet-amplifier relayer doesn't require gas payment
}
#[cfg(not(feature = "devnet-amplifier"))]
fn default_gas_value_wei(source_chain: &str) -> u128 {
    if source_chain.starts_with("flow") {
        300_000_000_000_000_000 // 0.3 FLOW
    } else {
        10_000_000_000_000_000 // 0.01 ETH
    }
}
const MAX_CONCURRENT_SENDS: usize = 100;
const MAX_RETRIES: u32 = 5;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    // --- Read SOURCE EVM chain info ---
    let evm_rpc_url = args.source_rpc.clone();

    // Validate RPCs
    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    // Check that verification contracts exist
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }

    // Check AxelarnetGateway exists (required for ITS hub routing)
    if read_axelar_contract_field(&args.config, "/axelar/contracts/AxelarnetGateway/address")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    // --- Set up EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;

    let main_key: [u8; 32] = signer.to_bytes().into();

    #[allow(clippy::float_arithmetic)]
    {
        let balance: u128 = read_provider.get_balance(deployer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{deployer_address} ({eth:.6} ETH)"));
    }

    // --- Read ITS contract addresses ---
    let its_factory_addr = read_contract_address(&args.config, src, "InterchainTokenFactory")?;
    let its_proxy_addr = read_contract_address(&args.config, src, "InterchainTokenService")?;

    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    let write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);

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

    // --- Token setup ---
    let burst_mode = !(args.tps.is_some() && args.duration_secs.is_some());
    let (num_keys, total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let tps = args.tps.unwrap() as usize;
        let dur = args.duration_secs.unwrap();
        (tps * args.key_cycle as usize, tps as u64 * dur)
    };
    // Keep num_txs as alias for burst compat (equals num_keys in burst mode)
    let num_txs = num_keys;
    // Amount must survive ITS hub decimal truncation between EVM (18 decimals) and Solana.
    // Use 1 full token (10^18) to ensure the truncated amount is non-zero.
    let amount_per_tx = U256::from(1_000_000_000_000_000_000u128); // 10^18 = 1 token
    // Distribute 100x per key so cached tokens last across many runs.
    let amount_per_key = amount_per_tx * U256::from(100);
    // Mint a large fixed supply so the token can be reused across runs without redeploying.
    let total_supply = U256::from(1_000_000) * U256::from(1_000_000_000_000_000_000u128); // 1M tokens

    let its_service = InterchainTokenService::new(its_proxy_addr, &write_provider);

    let (token_id, token_addr, deploy_message_id) = if let Some(ref tid) = args.token_id {
        // User provided a token ID
        let token_id: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        let addr = its_service
            .interchainTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (provided)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        (token_id, addr, None)
    } else {
        // Check cache
        let cache = read_its_cache(src, dest);
        let cached = cache
            .get("tokenId")
            .and_then(|v| v.as_str())
            .and_then(|tid| tid.parse::<FixedBytes<32>>().ok())
            .and_then(|tid| {
                cache
                    .get("tokenAddress")
                    .and_then(|v| v.as_str())
                    .and_then(|a| a.parse::<Address>().ok())
                    .map(|addr| (tid, addr))
            });

        if let Some((tid, addr)) = cached {
            // Verify token still exists and deployer has enough balance
            let token = ERC20::new(addr, &write_provider);
            let needed = amount_per_key * U256::from(num_keys);
            let balance = token
                .balanceOf(deployer_address)
                .call()
                .await
                .unwrap_or_default();
            if balance >= needed {
                ui::info(&format!("reusing cached ITS token: {addr}"));
                ui::kv("token ID (cached)", &format!("{tid}"));
                (tid, addr, None)
            } else if balance > U256::ZERO {
                ui::warn(&format!(
                    "cached token has insufficient supply ({balance} < {needed}), deploying fresh..."
                ));
                deploy_its_token(
                    &write_provider,
                    its_factory_addr,
                    deployer_address,
                    dest,
                    total_supply,
                    src,
                    gas_value,
                )
                .await?
            } else {
                ui::warn("cached token no longer exists, deploying fresh...");
                deploy_its_token(
                    &write_provider,
                    its_factory_addr,
                    deployer_address,
                    dest,
                    total_supply,
                    src,
                    gas_value,
                )
                .await?
            }
        } else {
            deploy_its_token(
                &write_provider,
                its_factory_addr,
                deployer_address,
                dest,
                total_supply,
                src,
                gas_value,
            )
            .await?
        }
    };

    // Wait for the remote deploy to propagate through the hub to Solana.
    if let Some(ref deploy_msg_id) = deploy_message_id {
        super::verify::wait_for_its_remote_deploy_to_solana(
            &args.config,
            src,
            dest,
            deploy_msg_id,
            &args.destination_rpc,
        )
        .await?;
    }

    // --- Derive N EVM signers ---
    let derived = keypairs::derive_evm_signers(&main_key, num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    // Fund derived wallets.
    // Burst: each key fires once → gas + 1× gas_value.
    // Sustained: each key fires once every 3s → gas + ceil(duration/3)× gas_value.
    let funding_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    let gas_extra_per_key = if burst_mode {
        gas_value_wei
    } else {
        let dur = args.duration_secs.unwrap();
        let rounds = dur.div_ceil(args.key_cycle);
        let buffered = rounds + rounds / 5 + 1;
        gas_value_wei.saturating_mul(buffered as u128)
    };
    keypairs::ensure_funded_evm_with_extra(&funding_provider, &signer, &derived, gas_extra_per_key)
        .await?;

    // --- Distribute ITS tokens to derived wallets ---
    // Build a fresh provider so the nonce cache is not stale after deploy transactions.
    let token_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    distribute_tokens(&token_provider, token_addr, &derived, amount_per_key).await?;

    // Receiver address on Solana — use the default Solana keypair's pubkey.
    let sol_keypair = crate::solana::load_keypair(args.keypair.as_deref())?;
    let receiver_bytes = {
        use solana_sdk::signer::Signer;
        Bytes::from(sol_keypair.pubkey().to_bytes().to_vec())
    };

    // === SUSTAINED MODE ===
    if !burst_mode {
        let tps = args.tps.unwrap() as usize;
        let duration_secs = args.duration_secs.unwrap();
        let key_cycle = args.key_cycle as usize;
        let rpc_url_str = evm_rpc_url.clone();

        // Pre-fetch nonces.
        let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
        let mut nonces: Vec<u64> = Vec::with_capacity(num_keys);
        for signer in &derived {
            let n = nonce_provider
                .get_transaction_count(signer.address())
                .await?;
            nonces.push(n);
        }

        let spinner = ui::wait_spinner(&format!(
            "[0/{duration_secs}s] starting sustained ITS send..."
        ));
        let test_start = Instant::now();
        let dest_chain_s = dest.to_string();

        let make_task: super::sustained::MakeTask =
            Box::new(move |key_idx: usize, nonce: Option<u64>| {
                let dc = dest_chain_s.clone();
                let gv = gas_value;
                let rb = receiver_bytes.clone();
                let amt = amount_per_tx;
                let its_proxy = its_proxy_addr;
                let tid = token_id;
                let url = rpc_url_str.clone();

                let provider = ProviderBuilder::new()
                    .wallet(derived[key_idx].clone())
                    .connect_http(url.parse().expect("invalid RPC URL"));

                Box::pin(async move {
                    execute_interchain_transfer(&provider, its_proxy, tid, &dc, &rb, amt, gv, nonce)
                        .await
                })
            });

        let result = super::sustained::run_sustained_loop(
            tps,
            duration_secs,
            key_cycle,
            Some(nonces),
            make_task,
            None,
            spinner,
        )
        .await;

        let mut report = super::sustained::build_sustained_report(
            result,
            src,
            dest,
            &format!("{its_proxy_addr}"),
            total_expected,
            num_keys,
        );

        let verification = super::verify::verify_onchain_solana_its(
            &args.config,
            &args.source_chain,
            &args.destination_chain,
            &format!("{its_proxy_addr}"),
            &args.destination_rpc,
            &mut report.transactions,
        )
        .await?;
        report.verification = Some(verification);

        return finish_report(&args, &mut report, test_start);
    }
    // === END SUSTAINED MODE ===

    // --- Parallel interchainTransfer sends via ITS Service ---
    // Each derived key calls ITS.interchainTransfer(tokenId, destChain, destAddr, amount, metadata, gasValue)
    // The ITS Service handles hub wrapping (SEND_TO_HUB) and emits ContractCall to "axelar".
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));
    let test_start = Instant::now();

    let mut tasks = Vec::with_capacity(num_txs);
    let dest_chain = dest.to_string();

    for derived_signer in &derived {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_txs;
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
                let result =
                    execute_interchain_transfer(&provider, its_proxy, tid, &dc, &rb, amt, gv, None)
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

    #[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
    let mut report = LoadTestReport {
        source_chain: src.to_string(),
        destination_chain: dest.to_string(),
        destination_address: format!("{its_proxy_addr}"),
        protocol: String::new(),
        tps: None,
        duration_secs: None,
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

    // --- Verify ---
    let verification = super::verify::verify_onchain_solana_its(
        &args.config,
        &args.source_chain,
        &args.destination_chain,
        &format!("{its_proxy_addr}"),
        &args.destination_rpc,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

/// Deploy a new interchain token and its remote counterpart.
/// Returns (tokenId, localTokenAddress).
#[allow(clippy::too_many_arguments)]
async fn deploy_its_token<P: Provider>(
    provider: &P,
    factory_addr: Address,
    deployer: Address,
    dest_chain: &str,
    total_supply: U256,
    source_chain: &str,
    gas_value: U256,
) -> eyre::Result<(FixedBytes<32>, Address, Option<String>)> {
    let salt = generate_salt();

    ui::info("deploying new ITS token...");
    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("supply", &format!("{total_supply}"));

    let factory = InterchainTokenFactory::new(factory_addr, provider);

    let deploy_call = factory
        .deployInterchainToken(
            salt,
            TOKEN_NAME.to_string(),
            TOKEN_SYMBOL.to_string(),
            TOKEN_DECIMALS,
            total_supply,
            deployer,
        )
        .value(U256::ZERO);

    let pending = deploy_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("deploy tx", &format!("{tx_hash}"));

    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("deploy tx timed out after 120s"))??;

    let (token_id, token_addr) = extract_token_deployed_event(&receipt)?;
    ui::kv("token ID", &format!("{token_id}"));
    ui::address("token address", &format!("{token_addr}"));

    // Deploy remote interchain token
    ui::info(&format!("deploying remote token to {dest_chain}..."));

    let remote_call = factory
        .deployRemoteInterchainToken(salt, dest_chain.to_string(), gas_value)
        .value(gas_value);

    let pending = remote_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("remote deploy tx", &format!("{tx_hash}"));

    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("remote deploy tx timed out after 120s"))??;

    ui::success(&format!(
        "remote deploy confirmed in block {}",
        receipt.block_number.unwrap_or(0)
    ));

    // Extract the remote deploy message ID from the receipt
    let deploy_message_id = match extract_contract_call_event(&receipt) {
        Ok((event_index, _, _, _, _)) => {
            let msg_id = format!("{tx_hash:#x}-{event_index}");
            ui::kv("remote deploy message ID", &msg_id);
            Some(msg_id)
        }
        Err(_) => None,
    };

    // Save to cache
    let cache = serde_json::json!({
        "tokenId": format!("{token_id}"),
        "tokenAddress": format!("{token_addr}"),
        "salt": format!("{salt}"),
    });
    save_its_cache(source_chain, dest_chain, &cache)?;

    Ok((token_id, token_addr, deploy_message_id))
}

/// Distribute ITS tokens from deployer to all derived wallets.
async fn distribute_tokens<P: Provider>(
    provider: &P,
    token_addr: Address,
    derived: &[PrivateKeySigner],
    amount_per_key: U256,
) -> eyre::Result<()> {
    let token = ERC20::new(token_addr, provider);

    let spinner = ui::wait_spinner(&format!("distributing tokens to {} keys...", derived.len()));

    for (i, signer) in derived.iter().enumerate() {
        // Check existing balance first
        let balance = token
            .balanceOf(signer.address())
            .call()
            .await
            .unwrap_or_default();
        if balance >= amount_per_key {
            continue;
        }

        let call = token.transfer(signer.address(), amount_per_key);
        let pending = call
            .send()
            .await
            .map_err(|e| eyre!("failed to transfer tokens to key {i}: {e}"))?;
        pending
            .get_receipt()
            .await
            .map_err(|e| eyre!("token transfer to key {i} failed: {e}"))?;

        spinner.set_message(format!(
            "distributing tokens ({}/{} done)...",
            i + 1,
            derived.len()
        ));
    }

    spinner.finish_and_clear();
    ui::success(&format!("distributed tokens to {} keys", derived.len()));
    Ok(())
}

/// Send a single interchainTransfer via the ITS Service and return metrics.
///
/// Calls `ITS.interchainTransfer(tokenId, destChain, destAddr, amount, metadata, gasValue)`
/// which internally wraps the payload as SEND_TO_HUB and emits a ContractCall to "axelar".
///
/// `explicit_nonce`: when `Some`, bypasses alloy's RPC-based nonce fetch to avoid
/// collisions when the same key fires again before the previous tx confirms.
#[allow(clippy::too_many_arguments)]
async fn execute_interchain_transfer<P: Provider>(
    provider: &P,
    its_proxy: Address,
    token_id: FixedBytes<32>,
    dest_chain: &str,
    receiver_bytes: &Bytes,
    amount: U256,
    gas_value: U256,
    explicit_nonce: Option<u64>,
) -> TxMetrics {
    let submit_start = Instant::now();

    let its = InterchainTokenService::new(its_proxy, provider);
    let base_call = its
        .interchainTransfer(
            token_id,
            dest_chain.to_string(),
            receiver_bytes.clone(),
            amount,
            Bytes::new(), // empty metadata
            gas_value,
        )
        .value(gas_value);
    let call = match explicit_nonce {
        Some(n) => base_call.nonce(n),
        None => base_call,
    };

    match call.send().await {
        Ok(pending) => {
            let tx_hash = *pending.tx_hash();
            match tokio::time::timeout(EVM_RECEIPT_TIMEOUT, pending.get_receipt()).await {
                Ok(Ok(receipt)) => {
                    #[allow(clippy::cast_possible_truncation)]
                    let latency_ms = submit_start.elapsed().as_millis() as u64;

                    // Extract full ContractCall event data
                    match extract_contract_call_event(&receipt) {
                        Ok((
                            event_index,
                            _payload,
                            payload_hash_bytes,
                            dest_chain,
                            dest_address,
                        )) => {
                            let message_id = format!("{tx_hash:#x}-{event_index}");
                            let source_address = format!("{its_proxy}");
                            let payload_hash = alloy::hex::encode(payload_hash_bytes.as_slice());

                            TxMetrics {
                                signature: message_id,
                                submit_time_ms: 0,
                                confirm_time_ms: Some(latency_ms),
                                latency_ms: Some(latency_ms),
                                compute_units: Some(receipt.gas_used),
                                slot: receipt.block_number,
                                success: true,
                                error: None,
                                payload: Vec::new(),
                                payload_hash,
                                source_address,
                                gmp_destination_chain: dest_chain,
                                gmp_destination_address: dest_address,
                                send_instant: Some(submit_start),
                                amplifier_timing: None,
                            }
                        }
                        Err(e) => {
                            make_failure(submit_start, &format!("no ContractCall event: {e}"))
                        }
                    }
                }
                Ok(Err(e)) => make_failure_with_hash(submit_start, &e.to_string(), Some(tx_hash)),
                Err(_) => make_failure_with_hash(submit_start, "tx timed out", Some(tx_hash)),
            }
        }
        Err(e) => make_failure(submit_start, &e.to_string()),
    }
}

fn make_failure(submit_start: Instant, error: &str) -> TxMetrics {
    make_failure_with_hash(submit_start, error, None)
}

fn make_failure_with_hash(
    submit_start: Instant,
    error: &str,
    tx_hash: Option<alloy::primitives::TxHash>,
) -> TxMetrics {
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    TxMetrics {
        signature: tx_hash.map_or_else(String::new, |h| format!("{h:#x}")),
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
