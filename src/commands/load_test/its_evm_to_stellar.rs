//! EVM -> Stellar ITS load test.
//!
//! Source-side flow is identical to `its_evm_to_sol.rs` (deploy/cache the
//! AXE token on the EVM source, distribute to derived signers, fire
//! `interchainTransfer` calls). The destination side uses Stellar's
//! `is_message_approved` / `is_message_executed` view calls via the
//! `verify_onchain_stellar_its[_streaming]` verifier.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
use super::{LoadTestArgs, check_evm_balance, finish_report, read_its_cache, validate_evm_rpc};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenService};
use crate::ui;

/// Default gas value for ITS cross-chain transfers.
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

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    verify_axelar_prerequisites(&cfg, dest)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let evm_src = init_evm_source_context(args.private_key.as_deref(), evm_rpc_url.clone()).await?;
    let evm_targets = resolve_evm_targets(&cfg, src)?;
    let stellar = resolve_stellar_targets(&args, evm_src.deployer_address)?;
    let gas_value_wei = parse_gas_value_wei(args.gas_value.as_deref(), src)?;
    let gas_value = U256::from(gas_value_wei);
    let sizing = compute_run_sizing(&args);

    let (token_id, token_addr) =
        resolve_or_deploy_token(&args, &evm_src, &evm_targets, dest, src, gas_value, &sizing)
            .await?;

    let derived = derive_and_fund_signers(&args, &evm_src, &sizing, gas_value_wei).await?;
    distribute_axe_tokens(&evm_src, token_addr, &derived, sizing.amount_per_key).await?;

    let (stellar_recipient_addr, receiver_bytes) =
        load_stellar_recipient(args.private_key.as_deref())?;

    let transfer = TransferContext {
        rpc_url: evm_rpc_url,
        derived,
        its_proxy_addr: evm_targets.its_proxy_addr,
        token_id,
        receiver_bytes,
        amount_per_tx: sizing.amount_per_tx,
        gas_value,
    };

    if !sizing.burst_mode {
        run_sustained_pipeline(
            &args,
            &cfg,
            transfer,
            &stellar,
            &stellar_recipient_addr,
            &sizing,
        )
        .await
    } else {
        run_burst_pipeline(&args, transfer, &stellar, &stellar_recipient_addr, &sizing).await
    }
}

/// EVM source-side state: signer, derived addresses, and the RPC URL used for
/// every per-call provider this module builds.
struct EvmSourceContext {
    signer: PrivateKeySigner,
    deployer_address: Address,
    main_key: [u8; 32],
    rpc_url: String,
}

/// EVM source ITS contract addresses.
struct EvmTargets {
    its_factory_addr: Address,
    its_proxy_addr: Address,
}

/// Stellar destination configuration plus the deterministic 32-byte source
/// pubkey used by the simulate-only verify view calls.
struct StellarTargets {
    rpc: String,
    network_type: String,
    gateway_addr: String,
    signer_pk: [u8; 32],
}

/// Sizing parameters derived from CLI flags: burst vs sustained, key counts,
/// expected transfer total, and per-tx / per-key / total-supply amounts.
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

/// Bundle of values consumed by both the burst and sustained pipelines when
/// firing `interchainTransfer` calls from the derived EVM signers.
struct TransferContext {
    rpc_url: String,
    derived: Vec<PrivateKeySigner>,
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    receiver_bytes: Bytes,
    amount_per_tx: U256,
    gas_value: U256,
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

/// Parse the EVM private key, check the funding balance, and emit the
/// matching UI line. Returns the signer plus the values derived from it
/// (deployer address, main key bytes, RPC URL).
async fn init_evm_source_context(
    private_key: Option<&str>,
    rpc_url: String,
) -> eyre::Result<EvmSourceContext> {
    let private_key = private_key.ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;
    let main_key: [u8; 32] = signer.to_bytes().into();

    {
        let balance: u128 = read_provider.get_balance(deployer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{deployer_address} ({eth:.6} ETH)"));
    }

    Ok(EvmSourceContext {
        signer,
        deployer_address,
        main_key,
        rpc_url,
    })
}

/// Resolve the EVM source ITS factory + service addresses from config and
/// emit the matching UI lines.
fn resolve_evm_targets(cfg: &ChainsConfig, src: &str) -> eyre::Result<EvmTargets> {
    let src_cfg = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?;
    let its_factory_addr: Address = src_cfg
        .contract_address("InterchainTokenFactory", src)?
        .parse()?;
    let its_proxy_addr: Address = src_cfg
        .contract_address("InterchainTokenService", src)?
        .parse()?;
    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS service", &format!("{its_proxy_addr}"));
    Ok(EvmTargets {
        its_factory_addr,
        its_proxy_addr,
    })
}

/// Resolve Stellar destination config (RPC, network type, gateway/example
/// addresses) and derive the deterministic dummy ed25519 pubkey used as the
/// simulation envelope's source account.
fn resolve_stellar_targets(
    args: &LoadTestArgs,
    deployer_address: Address,
) -> eyre::Result<StellarTargets> {
    let dest = &args.destination_chain;
    let stellar_rpc = args.destination_rpc.clone();
    let stellar_network_type = super::read_stellar_network_type(&args.config, dest)?;
    let stellar_gateway_addr =
        super::read_stellar_contract_address(&args.config, dest, "AxelarGateway")?;
    let stellar_example_addr =
        super::read_stellar_contract_address(&args.config, dest, "AxelarExample")?;
    ui::address("Stellar AxelarGateway", &stellar_gateway_addr);
    ui::address("Stellar AxelarExample", &stellar_example_addr);

    // For the simulate-only view calls, we need a 32-byte source pubkey but
    // it's not authorizing anything — just used as the simulation envelope's
    // source account. Use the deployer's EVM address right-padded; any
    // existing Stellar account works too. Easiest: derive a deterministic
    // dummy ed25519 pk from the EVM key.
    let signer_pk: [u8; 32] = alloy::primitives::keccak256(deployer_address.as_slice()).into();

    Ok(StellarTargets {
        rpc: stellar_rpc,
        network_type: stellar_network_type,
        gateway_addr: stellar_gateway_addr,
        signer_pk,
    })
}

/// Parse the user-supplied gas value (wei), defaulting to
/// `default_gas_value_wei(src)`, and emit the matching UI line.
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

/// Decide burst vs sustained, key/tx counts, and amounts.
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
    let num_txs = num_keys;
    let amount_per_tx = U256::from(1_000_000_000_000_000_000u128);
    let amount_per_key = amount_per_tx * U256::from(100);
    let total_supply = U256::from(1_000_000) * U256::from(1_000_000_000_000_000_000u128);

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

/// Resolve the ITS token to use: either the user-supplied `--token-id`, the
/// cached deployment if its balance still covers the planned run, or a fresh
/// `deploy_its_token` call. The `_deploy_message_id` returned by the deploy
/// helper is intentionally discarded — it's not consulted further down.
///
/// Note: we deliberately do NOT call `wait_for_its_remote_deploy` here —
/// that helper is hardcoded to poll an EVM destination gateway. For
/// Stellar destinations we trust the deploy_remote will eventually land
/// (interchainTransfer calls retry on 429s and the verify pipeline picks
/// them up regardless). On a fresh first run, the first few transfers
/// may stall at "approved" on Stellar until the remote token is registered;
/// they'll progress as soon as the registration completes.
async fn resolve_or_deploy_token(
    args: &LoadTestArgs,
    evm_src: &EvmSourceContext,
    evm_targets: &EvmTargets,
    dest: &str,
    src: &str,
    gas_value: U256,
    sizing: &RunSizing,
) -> eyre::Result<(FixedBytes<32>, Address)> {
    let write_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_src.rpc_url.parse()?);
    let its_service = InterchainTokenService::new(evm_targets.its_proxy_addr, &write_provider);

    let (token_id, token_addr, _deploy_message_id) = if let Some(ref tid) = args.token_id {
        let token_id: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        let addr = its_service
            .interchainTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (provided)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        (token_id, addr, None::<String>)
    } else {
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
            let token = ERC20::new(addr, &write_provider);
            let needed = sizing.amount_per_key * U256::from(sizing.num_keys);
            let balance = token
                .balanceOf(evm_src.deployer_address)
                .call()
                .await
                .unwrap_or_default();
            if balance >= needed {
                ui::info(&format!("reusing cached ITS token: {addr}"));
                ui::kv("token ID (cached)", &format!("{tid}"));
                (tid, addr, None)
            } else {
                ui::warn(&format!(
                    "cached token has insufficient supply ({balance} < {needed}), deploying fresh..."
                ));
                super::its_evm_to_sol::deploy_its_token(
                    &write_provider,
                    evm_targets.its_factory_addr,
                    evm_src.deployer_address,
                    dest,
                    sizing.total_supply,
                    src,
                    gas_value,
                )
                .await?
            }
        } else {
            super::its_evm_to_sol::deploy_its_token(
                &write_provider,
                evm_targets.its_factory_addr,
                evm_src.deployer_address,
                dest,
                sizing.total_supply,
                src,
                gas_value,
            )
            .await?
        }
    };

    Ok((token_id, token_addr))
}

/// Derive ephemeral EVM signers from the main key and ensure each is funded
/// with enough native gas to cover the planned send rounds plus the gas
/// value passed to ITS.
async fn derive_and_fund_signers(
    args: &LoadTestArgs,
    evm_src: &EvmSourceContext,
    sizing: &RunSizing,
    gas_value_wei: u128,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    let derived = keypairs::derive_evm_signers(&evm_src.main_key, sizing.num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    let funding_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_src.rpc_url.parse()?);
    let gas_extra_per_key = if sizing.burst_mode {
        gas_value_wei
    } else {
        let dur = sizing.sustained_params.expect("burst_mode is false").1;
        let rounds = dur.div_ceil(args.key_cycle);
        let buffered = rounds + rounds / 5 + 1;
        gas_value_wei.saturating_mul(buffered as u128)
    };
    keypairs::ensure_funded_evm_with_extra(
        &funding_provider,
        &evm_src.signer,
        &derived,
        gas_extra_per_key,
    )
    .await?;
    Ok(derived)
}

/// Distribute the AXE token balance from the deployer to each derived signer
/// so they can each fund their share of `interchainTransfer` calls.
async fn distribute_axe_tokens(
    evm_src: &EvmSourceContext,
    token_addr: Address,
    derived: &[PrivateKeySigner],
    amount_per_key: U256,
) -> eyre::Result<()> {
    let token_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_src.rpc_url.parse()?);
    super::its_evm_to_sol::distribute_tokens(&token_provider, token_addr, derived, amount_per_key)
        .await
}

/// Load the Stellar recipient wallet (requires `STELLAR_PRIVATE_KEY`) and
/// produce the ASCII-bytes encoding ITS expects for Stellar destinations.
///
/// ITS expects `encodeITSDestination` for Stellar = ASCII bytes of the
/// destination address. For plain `interchain_transfer` (no data), the
/// recipient must be a Stellar account (G-address) — sending to a contract
/// C-address makes ITS try `execute_with_data` with empty data, which the
/// AxelarExample callback rejects (Contract Error #17).
fn load_stellar_recipient(private_key: Option<&str>) -> eyre::Result<(String, Bytes)> {
    let stellar_recipient_wallet = super::load_stellar_main_wallet(private_key)
        .map_err(|e| eyre!("EVM→Stellar ITS needs STELLAR_PRIVATE_KEY for the recipient: {e}"))?;
    let stellar_recipient_addr = stellar_recipient_wallet.address();
    let receiver_bytes = Bytes::from(stellar_recipient_addr.as_bytes().to_vec());
    ui::address("destination Stellar account", &stellar_recipient_addr);
    Ok((stellar_recipient_addr, receiver_bytes))
}

/// Drive the sustained-mode pipeline: pre-fetch nonces, spawn the streaming
/// Stellar verifier, run the per-second send loop, stitch amplifier timings
/// back into the report, and hand off to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    transfer: TransferContext,
    stellar: &StellarTargets,
    stellar_recipient_addr: &str,
    sizing: &RunSizing,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let tps = sizing.sustained_params.expect("burst_mode is false").0 as usize;
    let duration_secs = sizing.sustained_params.expect("burst_mode is false").1;
    let key_cycle = args.key_cycle as usize;
    let rpc_url_str = transfer.rpc_url.clone();

    let nonce_provider = ProviderBuilder::new().connect_http(transfer.rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for s in &transfer.derived {
        let n = nonce_provider.get_transaction_count(s.address()).await?;
        nonces.push(n);
    }

    let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
    let send_done = Arc::new(AtomicBool::new(false));
    let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

    let has_voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &args.source_axelar_id)
        .is_ok();

    let vconfig = args.config.clone();
    let vsource = args.source_axelar_id.clone();
    let vdest = args.destination_axelar_id.clone();
    let vstellar_rpc = stellar.rpc.clone();
    let vstellar_net = stellar.network_type.clone();
    let vstellar_gw = stellar.gateway_addr.clone();
    let vsigner_pk = stellar.signer_pk;
    let vdone = Arc::clone(&send_done);
    let verify_handle = tokio::spawn(async move {
        let spinner = spinner_rx.await.expect("spinner channel dropped");
        super::verify::verify_onchain_stellar_its_streaming(
            &vconfig,
            &vsource,
            &vdest,
            &vstellar_rpc,
            &vstellar_net,
            &vstellar_gw,
            vsigner_pk,
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
    let TransferContext {
        derived,
        its_proxy_addr,
        token_id,
        receiver_bytes,
        amount_per_tx,
        gas_value,
        ..
    } = transfer;

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
        stellar_recipient_addr,
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

/// Drive the burst-mode pipeline: fan out one `interchainTransfer` per derived
/// signer (with bounded concurrency and 429-aware retries), run the batch
/// Stellar verifier on the confirmed set, and hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    transfer: TransferContext,
    stellar: &StellarTargets,
    stellar_recipient_addr: &str,
    sizing: &RunSizing,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let num_txs = sizing.num_txs;

    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));
    let test_start = Instant::now();

    let mut tasks = Vec::with_capacity(num_txs);
    let dest_chain = args.destination_axelar_id.clone();

    for derived_signer in &transfer.derived {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_txs;
        let dc = dest_chain.clone();
        let gv = transfer.gas_value;
        let rb = transfer.receiver_bytes.clone();
        let amt = transfer.amount_per_tx;
        let its_proxy = transfer.its_proxy_addr;
        let tid = transfer.token_id;

        let provider = ProviderBuilder::new()
            .wallet(derived_signer.clone())
            .connect_http(transfer.rpc_url.parse()?);

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

    let mut report = LoadTestReport {
        source_chain: src.to_string(),
        destination_chain: dest.to_string(),
        destination_address: stellar_recipient_addr.to_string(),
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

    let verification = super::verify::verify_onchain_stellar_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        stellar_recipient_addr,
        &stellar.rpc,
        &stellar.network_type,
        &stellar.gateway_addr,
        stellar.signer_pk,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}
