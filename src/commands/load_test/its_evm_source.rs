//! Shared building blocks for ITS load tests whose source chain is EVM.
//!
//! Every `its_evm_to_<dest>.rs` module follows the same source-side recipe:
//!
//! 1. Validate Axelar prerequisites for the destination chain.
//! 2. Parse the EVM private key into a signer + main-key bytes.
//! 3. Resolve the ITS factory + service addresses for the source chain.
//! 4. Pick a gas-value default (`estimateGasFee` quote × 1.5, fallback constant).
//! 5. Deploy or reuse an `InterchainToken` on the source EVM + its remote
//!    counterpart on the destination chain.
//! 6. Derive a pool of ephemeral signers and fund them with native + gas-value.
//! 7. Distribute the ITS token across the derived signers.
//! 8. Run the per-tx `interchainTransfer` loop (burst or sustained).
//!
//! Steps 1-4 and 6-8 are destination-agnostic and live here. Step 5 is mostly
//! destination-agnostic (the factory call is identical; only the cache schema
//! and the wait-for-remote-deploy hook differ) — the helper here covers the
//! identical bits; per-destination modules supply their own wait-for-deploy.
//!
//! Per-destination modules keep:
//!   * Destination-receiver-bytes encoding (varies: 20B EVM, 32B Solana, etc.).
//!   * Pre-flight checks for the destination chain (RPC validation, etc.).
//!   * Wait-for-remote-deploy on the destination chain.
//!   * Final on-chain verification (Solana / EVM / Stellar / etc.).

use std::time::{Duration, Instant};

use alloy::{
    primitives::{Address, Bytes, FixedBytes, TxHash, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;

use super::keypairs;
use super::metrics::TxMetrics;
use super::{LoadTestArgs, check_evm_balance, save_its_cache};
use crate::commands::test_its::{
    extract_contract_call_event, extract_token_deployed_event, generate_salt,
};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenFactory, InterchainTokenService};
use crate::ui;

/// How long to wait for an EVM tx receipt before giving up. Flow confirms in
/// ~8s; other chains typically <20s. 60s gives congested networks enough room
/// while still catching silently-dropped txs.
const EVM_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

const TOKEN_NAME: &str = "AXE";
const TOKEN_SYMBOL: &str = "AXE";
const TOKEN_DECIMALS: u8 = 18;

/// Source-chain EVM signer state: the user's signer plus its address and
/// raw private-key bytes (used for deriving ephemeral signers).
pub(super) struct EvmSource {
    pub signer: PrivateKeySigner,
    pub deployer_address: Address,
    pub main_key: [u8; 32],
}

/// ITS-side addresses resolved from config for the source chain.
pub(super) struct ItsContracts {
    pub its_factory_addr: Address,
    pub its_proxy_addr: Address,
}

/// Default gas value for ITS cross-chain transfers.
///
/// Tries the Axelarscan `estimateGasFee` quote for this route (× 1.5);
/// when the API can't be reached, falls back to `fallback_wei` — caller
/// supplies the per-(source-chain, destination-class) constant.
pub(super) async fn default_gas_value_wei(args: &LoadTestArgs, fallback_wei: u128) -> u128 {
    if let Some(quoted) = quote_route_gas(args).await {
        return quoted;
    }
    fallback_wei
}

async fn quote_route_gas(args: &LoadTestArgs) -> Option<u128> {
    let cfg = ChainsConfig::load(&args.config).ok()?;
    let symbol = cfg
        .chains
        .get(&args.source_chain)?
        .token_symbol
        .as_deref()?;
    super::gas_estimate::estimate_route_gas(
        &args.source_axelar_id,
        &args.destination_axelar_id,
        symbol,
        super::gas_estimate::DEFAULT_DEST_GAS_LIMIT,
    )
    .await
}

/// Verify Axelar-side prerequisites (cosmos Gateway for `dest`, global
/// AxelarnetGateway). Bails with the original error strings if either is
/// missing.
pub(super) fn verify_axelar_prerequisites(cfg: &ChainsConfig, dest: &str) -> eyre::Result<()> {
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
pub(super) async fn init_evm_source(
    args: &LoadTestArgs,
    evm_rpc_url: &str,
) -> eyre::Result<EvmSource> {
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
pub(super) fn resolve_its_contracts(cfg: &ChainsConfig, src: &str) -> eyre::Result<ItsContracts> {
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

    Ok(ItsContracts {
        its_factory_addr,
        its_proxy_addr,
    })
}

/// Derive ephemeral signers and ensure each one is funded for the planned
/// number of transfers (native gas + per-key gas-value buffer).
///
/// `hub_gas_extra_per_key` is the wei amount each derived signer needs *on
/// top of* the baseline native-gas funding — i.e., the `gas_value × 2`
/// (hub round-trip) × the number of rounds the key will fire.
pub(super) async fn derive_and_fund_keys(
    main_signer: &PrivateKeySigner,
    main_key: &[u8; 32],
    evm_rpc_url: &str,
    num_keys: usize,
    hub_gas_extra_per_key: u128,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    let derived = keypairs::derive_evm_signers(main_key, num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    let funding_provider = ProviderBuilder::new()
        .wallet(main_signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    keypairs::ensure_funded_evm_with_extra(
        &funding_provider,
        main_signer,
        &derived,
        hub_gas_extra_per_key,
    )
    .await?;

    Ok(derived)
}

/// Deploy a new interchain token and its remote counterpart.
/// Returns `(tokenId, localTokenAddress, deployMessageId)`.
///
/// The cache schema is shared across destinations — `tokenId`, `tokenAddress`,
/// `salt` — so a token deployed for, say, an EVM→EVM run is reusable on an
/// EVM→Solana run with the same source/destination pair if the destination
/// chain string matches.
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

    let deploy_message_id = match extract_contract_call_event(&receipt) {
        Ok((event_index, _, _, _, _)) => {
            let msg_id = format!("{tx_hash:#x}-{event_index}");
            ui::kv("remote deploy message ID", &msg_id);
            Some(msg_id)
        }
        Err(_) => None,
    };

    let cache = serde_json::json!({
        "tokenId": format!("{token_id}"),
        "tokenAddress": format!("{token_addr}"),
        "salt": format!("{salt}"),
    });
    save_its_cache(source_chain, dest_chain, &cache)?;

    Ok((token_id, token_addr, deploy_message_id))
}

/// Pre-approve the ITS token manager on each derived key's balance so
/// `interchainTransfer` doesn't revert with `TakeTokenFailed` when the
/// underlying token manager is lock/unlock (e.g. canonical XRP wrapped on
/// EVM).
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
pub(super) async fn approve_its_for_keys(
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

/// Distribute ITS tokens from the deployer wallet to every derived signer.
/// Per-key sends are skipped when the existing balance already meets
/// `amount_per_key`, so re-runs against the same derived keys reuse prior
/// distribution.
pub(super) async fn distribute_tokens<P: Provider>(
    provider: &P,
    token_addr: Address,
    derived: &[PrivateKeySigner],
    amount_per_key: U256,
) -> eyre::Result<()> {
    let token = ERC20::new(token_addr, provider);

    let spinner = ui::wait_spinner(&format!("distributing tokens to {} keys...", derived.len()));

    for (i, signer) in derived.iter().enumerate() {
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

/// Send a single `interchainTransfer` via the ITS Service and return metrics.
///
/// Calls `ITS.interchainTransfer(tokenId, destChain, destAddr, amount, metadata, gasValue)`,
/// which internally wraps the payload as `SEND_TO_HUB` and emits a `ContractCall`
/// to "axelar".
///
/// `explicit_nonce`: when `Some`, bypasses alloy's RPC-based nonce fetch to
/// avoid collisions when the same key fires again before the previous tx
/// confirms.
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

    let hub_gas = gas_value * U256::from(2);
    let its = InterchainTokenService::new(its_proxy, provider);
    let base_call = its
        .interchainTransfer(
            token_id,
            dest_chain.to_string(),
            receiver_bytes.clone(),
            amount,
            Bytes::new(),
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
    tx_hash: Option<TxHash>,
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
