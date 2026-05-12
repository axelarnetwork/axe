use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;
use futures::future::join_all;
use solana_sdk::signer::Signer;
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
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenFactory, InterchainTokenService};
use crate::ui;

/// How long to wait for an EVM tx receipt before giving up.
/// Flow confirms in ~8s; other chains typically <20s. 60s gives congested
/// networks enough room while still catching silently-dropped txs.
const EVM_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

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

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    verify_axelar_prerequisites(&cfg, dest)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let evm_source = init_evm_source(&args, &evm_rpc_url).await?;
    let its = resolve_its_contracts(&cfg, src)?;
    let gas_value_wei = parse_gas_value_wei(args.gas_value.as_deref(), src)?;
    let gas_value = U256::from(gas_value_wei);
    let sizing = compute_run_sizing(&args);

    let token =
        resolve_or_deploy_token(&args, &evm_source, &its, &evm_rpc_url, &sizing, gas_value).await?;

    if let Some(ref deploy_msg_id) = token.deploy_message_id {
        super::verify::wait_for_its_remote_deploy_to_solana(
            &args.config,
            src,
            dest,
            deploy_msg_id,
            &args.destination_rpc,
        )
        .await?;
    }

    let derived =
        derive_and_fund_keys(&args, &evm_source, &evm_rpc_url, &sizing, gas_value_wei).await?;

    let token_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    distribute_tokens(
        &token_provider,
        token.token_addr,
        &derived,
        sizing.amount_per_key,
    )
    .await?;

    let sol_keypair = crate::solana::load_keypair(args.keypair.as_deref())?;
    let receiver_bytes = Bytes::from(sol_keypair.pubkey().to_bytes().to_vec());

    let targets = TransferTargets {
        its_proxy_addr: its.its_proxy_addr,
        token_id: token.token_id,
        gas_value,
        receiver_bytes,
    };

    if !sizing.burst_mode {
        run_sustained_pipeline(&args, &cfg, &evm_rpc_url, &derived, &sizing, &targets).await
    } else {
        run_burst_pipeline(&args, &evm_rpc_url, &derived, &sizing, &targets).await
    }
}

/// Source-chain EVM signer state: the user's signer plus its address and
/// raw private-key bytes (used for deriving ephemeral signers).
struct EvmSource {
    signer: PrivateKeySigner,
    deployer_address: Address,
    main_key: [u8; 32],
}

/// ITS-side addresses resolved from config for the source chain.
struct ItsContracts {
    its_factory_addr: Address,
    its_proxy_addr: Address,
}

/// Sizing parameters derived from CLI flags: chooses burst vs sustained,
/// number of ephemeral wallets, per-tx amount, and supply parameters.
struct RunSizing {
    burst_mode: bool,
    sustained_params: Option<(u64, u64)>,
    num_keys: usize,
    num_txs: usize,
    total_expected: u64,
    amount_per_tx: U256,
    amount_per_key: U256,
    total_supply: U256,
}

/// Resolved interchain token: cached, user-supplied, or freshly deployed.
/// `deploy_message_id` is `Some` only when the helper performed a remote
/// deploy in this run.
struct TokenIdentity {
    token_id: FixedBytes<32>,
    token_addr: Address,
    deploy_message_id: Option<String>,
}

/// Per-tx send parameters consumed by both the sustained and burst pipelines:
/// the ITS service to call, the token to push through it, the gas attached to
/// each interchain transfer, and the Solana recipient.
struct TransferTargets {
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    gas_value: U256,
    receiver_bytes: Bytes,
}

/// Verify Axelar-side prerequisites (cosmos Gateway for `dest`, global
/// AxelarnetGateway). Bails with the existing error strings if either is
/// missing.
fn verify_axelar_prerequisites(cfg: &ChainsConfig, dest: &str) -> eyre::Result<()> {
    if cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }

    if cfg
        .axelar
        .global_contract_address("AxelarnetGateway")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }
    Ok(())
}

/// Parse the EVM private key, log the wallet balance, and return the signer
/// state used by every downstream phase.
async fn init_evm_source(args: &LoadTestArgs, evm_rpc_url: &str) -> eyre::Result<EvmSource> {
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;

    let main_key: [u8; 32] = signer.to_bytes().into();

    {
        let balance: u128 = read_provider.get_balance(deployer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{deployer_address} ({eth:.6} ETH)"));
    }

    Ok(EvmSource {
        signer,
        deployer_address,
        main_key,
    })
}

/// Resolve the ITS factory + service addresses for the source chain and emit
/// the matching UI lines.
fn resolve_its_contracts(cfg: &ChainsConfig, src: &str) -> eyre::Result<ItsContracts> {
    let src_cfg = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?;
    let its_factory_addr: alloy::primitives::Address = src_cfg
        .contract_address("InterchainTokenFactory", src)?
        .parse()?;
    let its_proxy_addr: alloy::primitives::Address = src_cfg
        .contract_address("InterchainTokenService", src)?
        .parse()?;

    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    Ok(ItsContracts {
        its_factory_addr,
        its_proxy_addr,
    })
}

/// Parse the user-supplied gas value (wei), defaulting per source chain, and
/// emit the matching UI line.
fn parse_gas_value_wei(gas_value: Option<&str>, src: &str) -> eyre::Result<u128> {
    let gas_value_wei: u128 = match gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value_wei(src),
    };

    {
        ui::kv(
            "gas value",
            &format!(
                "{gas_value_wei} wei ({:.6} ETH)",
                gas_value_wei as f64 / 1e18
            ),
        );
    }
    Ok(gas_value_wei)
}

/// Decide burst vs sustained, ephemeral wallet count, expected tx count, and
/// per-tx / per-key / total-supply amounts.
fn compute_run_sizing(args: &LoadTestArgs) -> RunSizing {
    let sustained_params = args.tps.zip(args.duration_secs);
    let burst_mode = sustained_params.is_none();
    let (num_keys, total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let (tps, dur) = sustained_params.expect("burst_mode is false");
        let tps = tps as usize;
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

    RunSizing {
        burst_mode,
        sustained_params,
        num_keys,
        num_txs,
        total_expected,
        amount_per_tx,
        amount_per_key,
        total_supply,
    }
}

/// Resolve the ITS token to use this run: honour `--token-id`, then fall back
/// to the source/dest cache (deploying fresh if the cached token has
/// insufficient supply or no longer exists), and finally deploy a brand-new
/// token if no cache hit.
async fn resolve_or_deploy_token(
    args: &LoadTestArgs,
    evm_source: &EvmSource,
    its: &ItsContracts,
    evm_rpc_url: &str,
    sizing: &RunSizing,
    gas_value: U256,
) -> eyre::Result<TokenIdentity> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let write_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    let its_service = InterchainTokenService::new(its.its_proxy_addr, &write_provider);

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
            let needed = sizing.amount_per_key * U256::from(sizing.num_keys);
            let balance = token
                .balanceOf(evm_source.deployer_address)
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
                    its.its_factory_addr,
                    evm_source.deployer_address,
                    dest,
                    sizing.total_supply,
                    src,
                    gas_value,
                )
                .await?
            } else {
                ui::warn("cached token no longer exists, deploying fresh...");
                deploy_its_token(
                    &write_provider,
                    its.its_factory_addr,
                    evm_source.deployer_address,
                    dest,
                    sizing.total_supply,
                    src,
                    gas_value,
                )
                .await?
            }
        } else {
            deploy_its_token(
                &write_provider,
                its.its_factory_addr,
                evm_source.deployer_address,
                dest,
                sizing.total_supply,
                src,
                gas_value,
            )
            .await?
        }
    };

    Ok(TokenIdentity {
        token_id,
        token_addr,
        deploy_message_id,
    })
}

/// Derive the ephemeral EVM signers and ensure each one is funded for the
/// planned number of transfers (gas + per-key gas-value buffer).
async fn derive_and_fund_keys(
    args: &LoadTestArgs,
    evm_source: &EvmSource,
    evm_rpc_url: &str,
    sizing: &RunSizing,
    gas_value_wei: u128,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    let derived = keypairs::derive_evm_signers(&evm_source.main_key, sizing.num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    // Fund derived wallets.
    // ITS hub routing pays 2× gas_value per transfer (two commands).
    // Burst: each key fires once → gas + 2× gas_value.
    // Sustained: each key fires once every 3s → gas + ceil(duration/3)× 2× gas_value.
    let funding_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    let hub_gas_value_wei = gas_value_wei.saturating_mul(2);
    let gas_extra_per_key = if sizing.burst_mode {
        hub_gas_value_wei
    } else {
        let dur = sizing.sustained_params.expect("burst_mode is false").1;
        let rounds = dur.div_ceil(args.key_cycle);
        let buffered = rounds + rounds / 5 + 1;
        hub_gas_value_wei.saturating_mul(buffered as u128)
    };
    keypairs::ensure_funded_evm_with_extra(
        &funding_provider,
        &evm_source.signer,
        &derived,
        gas_extra_per_key,
    )
    .await?;

    Ok(derived)
}

/// Drive the sustained-mode pipeline: pre-fetch nonces, spawn the streaming
/// Solana ITS verifier, run the EVM sustained sender loop, stitch amplifier
/// timings back into the report, and hand off to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    evm_rpc_url: &str,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    targets: &TransferTargets,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let tps = sizing.sustained_params.expect("burst_mode is false").0 as usize;
    let duration_secs = sizing.sustained_params.expect("burst_mode is false").1;
    let key_cycle = args.key_cycle as usize;
    let rpc_url_str = evm_rpc_url.to_string();

    // Pre-fetch nonces.
    let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for signer in derived {
        let n = nonce_provider
            .get_transaction_count(signer.address())
            .await?;
        nonces.push(n);
    }

    // Streaming verification: run concurrently with sends.
    let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
    let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

    let has_voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &args.source_chain)
        .is_ok();

    let vconfig = args.config.clone();
    let vsource = args.source_axelar_id.clone();
    let vdest = args.destination_axelar_id.clone();
    let vdest_rpc = args.destination_rpc.clone();
    let vdone = std::sync::Arc::clone(&send_done);
    let verify_handle = tokio::spawn(async move {
        let spinner = spinner_rx.await.expect("spinner channel dropped");
        super::verify::verify_onchain_solana_its_streaming(
            &vconfig, &vsource, &vdest, &vdest_rpc, verify_rx, vdone, spinner,
        )
        .await
    });

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS send..."
    ));
    let _ = spinner_tx.send(spinner.clone());

    let test_start = Instant::now();
    let dest_chain_s = dest.to_string();
    let derived_owned: Vec<PrivateKeySigner> = derived.to_vec();
    let amount_per_tx = sizing.amount_per_tx;
    let its_proxy_addr = targets.its_proxy_addr;
    let token_id = targets.token_id;
    let gas_value = targets.gas_value;
    let receiver_bytes = targets.receiver_bytes.clone();

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
                .wallet(derived_owned[key_idx].clone())
                .connect_http(url.parse().expect("invalid RPC URL"));

            Box::pin(async move {
                let mut result = execute_interchain_transfer(
                    &provider, its_proxy, tid, &dc, &rb, amt, gv, nonce,
                )
                .await;
                if result.success {
                    match super::verify::tx_to_pending_its(&result, has_vv) {
                        Ok(pending) => {
                            let _ = vtx.send(pending);
                        }
                        Err(e) => {
                            result.success = false;
                            result.error = Some(format!("failed to build verification state: {e}"));
                        }
                    }
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
        &format!("{}", targets.its_proxy_addr),
        sizing.total_expected,
        sizing.num_keys,
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

    finish_report(args, &mut report, test_start)
}

/// Drive the burst-mode pipeline: fan out parallel ITS interchain transfers
/// (with retry on rate limits), batch-verify on the Solana destination, and
/// hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    evm_rpc_url: &str,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    targets: &TransferTargets,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let num_txs = sizing.num_txs;

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

    for derived_signer in derived {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_txs;
        let dc = dest_chain.clone();
        let gv = targets.gas_value;
        let rb = targets.receiver_bytes.clone();
        let amt = sizing.amount_per_tx;
        let its_proxy = targets.its_proxy_addr;
        let tid = targets.token_id;

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

    let mut report = LoadTestReport {
        source_chain: src.to_string(),
        destination_chain: dest.to_string(),
        destination_address: format!("{}", targets.its_proxy_addr),
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
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &format!("{}", targets.its_proxy_addr),
        &args.destination_rpc,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}

/// Deploy a new interchain token and its remote counterpart.
/// Returns (tokenId, localTokenAddress).
pub(super) async fn deploy_its_token<P: Provider>(
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

    // ITS routes via the hub, so two commands are created (source→hub and
    // hub→destination). Pay 2× gas_value so both legs are covered.
    let hub_gas = gas_value * U256::from(2);
    let remote_call = factory
        .deployRemoteInterchainToken(salt, dest_chain.to_string(), hub_gas)
        .value(hub_gas);

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
/// Pre-approve the ITS token manager on each derived key's token balance so
/// `interchainTransfer` doesn't revert with `TakeTokenFailed` when the
/// underlying token manager is lock/unlock (e.g. canonical XRP wrapped on EVM).
///
/// The spender that pulls the tokens is the **token manager** for the given
/// `token_id`, not the ITS proxy itself: ITS dispatches into
/// `tokenManager.takeToken(from, amount)`, which then does
/// `IERC20.safeTransferFrom(from, address(this), amount)` — `address(this)`
/// is the token manager. So the user's allowance is checked against the
/// token manager's address, which we look up via `ITS.tokenManagerAddress`.
///
/// Mint/burn-managed tokens (the AXE we deploy via `deployInterchainToken`)
/// don't need this — ITS is the minter and the InterchainToken's `burn(from,
/// amount)` skips the allowance check. But canonical tokens registered against
/// the ITS hub use `transferFrom(sender, token_manager, amount)` which
/// strictly requires `allowance >= amount`.
///
/// Calls are issued sequentially (cheap relative to the test itself) and
/// skipped per-key when the existing allowance already exceeds
/// `amount_per_key * 2`, so re-runs against the same derived keys reuse the
/// prior approval.
pub async fn approve_its_for_keys(
    rpc_url: &str,
    token_addr: Address,
    its_proxy: Address,
    token_id: FixedBytes<32>,
    derived: &[PrivateKeySigner],
    amount_per_key: U256,
) -> eyre::Result<()> {
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let its = InterchainTokenService::new(its_proxy, &read_provider);
    let token_manager: Address = its
        .tokenManagerAddress(token_id)
        .call()
        .await
        .map_err(|e| {
            eyre!(
                "ITS.tokenManagerAddress({}) failed — token may not be registered yet: {e}",
                hex::encode(token_id)
            )
        })?;

    let read_token = ERC20::new(token_addr, &read_provider);
    let approve_threshold = amount_per_key.saturating_mul(U256::from(2));

    let spinner = ui::wait_spinner(&format!(
        "approving token manager {token_manager} for {} keys (lock/unlock token)...",
        derived.len()
    ));

    let mut approved = 0usize;
    for (i, signer) in derived.iter().enumerate() {
        let allowance = read_token
            .allowance(signer.address(), token_manager)
            .call()
            .await
            .unwrap_or_default();
        if allowance >= approve_threshold {
            continue;
        }
        let write_provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect_http(rpc_url.parse()?);
        let token = ERC20::new(token_addr, &write_provider);
        let pending = token
            .approve(token_manager, U256::MAX)
            .send()
            .await
            .map_err(|e| eyre!("failed to approve token manager for key {i}: {e}"))?;
        pending
            .get_receipt()
            .await
            .map_err(|e| eyre!("approve receipt for key {i} failed: {e}"))?;
        approved += 1;
        spinner.set_message(format!(
            "approving token manager ({}/{} new approvals)...",
            approved,
            derived.len()
        ));
    }

    spinner.finish_and_clear();
    if approved == 0 {
        ui::info(&format!(
            "token manager already approved for all {} keys (reused from prior run)",
            derived.len()
        ));
    } else {
        ui::success(&format!(
            "approved token manager for {approved}/{} keys",
            derived.len()
        ));
    }
    Ok(())
}

pub async fn distribute_tokens<P: Provider>(
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
pub(super) async fn execute_interchain_transfer<P: Provider>(
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

    // ITS routes via the hub, so two commands are created (source→hub and
    // hub→destination). Pay 2× gas_value so both legs are covered.
    let hub_gas = gas_value * U256::from(2);
    let its = InterchainTokenService::new(its_proxy, provider);
    let base_call = its
        .interchainTransfer(
            token_id,
            dest_chain.to_string(),
            receiver_bytes.clone(),
            amount,
            Bytes::new(), // empty metadata
            hub_gas,
        )
        .value(hub_gas);
    let call = match explicit_nonce {
        Some(n) => base_call.nonce(n),
        None => base_call,
    };

    match call.send().await {
        Ok(pending) => {
            let tx_hash = *pending.tx_hash();
            match tokio::time::timeout(EVM_RECEIPT_TIMEOUT, pending.get_receipt()).await {
                Ok(Ok(receipt)) => {
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
