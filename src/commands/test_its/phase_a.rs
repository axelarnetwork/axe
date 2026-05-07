//! Phase A — deploy an interchain token locally on Solana, fire the GMP that
//! deploys the same token remotely on the EVM destination, and drive both
//! relay legs (source → hub via [`relay_to_hub`], hub → destination EVM via
//! [`relay_to_destination`]). Caches the resulting `(token_id, dest_addr)`
//! so Phase B can be re-run without re-deploying.

use std::path::Path;
use std::time::Instant;

use alloy::{
    primitives::{Address, FixedBytes, keccak256},
    providers::Provider,
};
use eyre::Result;

use super::cache::{ItsTestCache, save_cache};
use super::encoding::{encode_inner_deploy, encode_receive_from_hub, encode_send_to_hub_deploy};
use super::relay::{relay_to_destination, relay_to_hub};
use crate::commands::event_extractors::generate_salt;
use crate::evm::{ERC20, InterchainTokenService};
use crate::timing::{DEST_CHAIN_POLL_ATTEMPTS, DEST_CHAIN_POLL_INTERVAL};
use crate::ui;

const PHASE_A_STEPS: usize = 11;

// Initial supply for the config-mode test, in base units of `ITS_CONFIG_SPEC`.
// 1_000_000_000_000 = 1000 tokens at 9 decimals.
const INITIAL_SUPPLY: u64 = 1_000_000_000_000;

/// Returns `(token_id, dest_token_addr)` on success and writes the result to
/// `cache_file` so a subsequent run skips this entire phase.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_phase_a_deploy<P: Provider>(
    src: &str,
    dst: &str,
    src_axelar_id: &crate::types::ChainAxelarId,
    dst_axelar_id: &crate::types::ChainAxelarId,
    src_rpc: &str,
    sol_keypair: &solana_sdk::signature::Keypair,
    sol_pubkey: solana_sdk::pubkey::Pubkey,
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    src_cosm_gateway: &str,
    voting_verifier: &str,
    axelarnet_gateway: &str,
    dst_cosm_gateway: &str,
    dst_multisig_prover: &str,
    axelar_rpc: &str,
    its_hub_address: &str,
    dst_its_proxy: Address,
    dst_evm_gateway: Address,
    dst_provider: &P,
    its: &InterchainTokenService::InterchainTokenServiceInstance<&P>,
    gas_value: u64,
    cache_file: &Path,
    phase_start: Instant,
) -> Result<([u8; 32], Address)> {
    let _ = src;
    let _ = dst;

    ui::section("Phase A: deploy local + remote (manual relay)");

    // Step A1: generate salt, derive token id
    let salt = generate_salt();
    let salt_bytes: [u8; 32] = salt.0;
    let token_id = crate::solana::interchain_token_id(&sol_pubkey, &salt_bytes);
    let token_id_b32 = FixedBytes::<32>::from(token_id);

    ui::step_header(1, PHASE_A_STEPS, "Generate salt + tokenId");
    ui::kv("salt", &format!("0x{}", alloy::hex::encode(salt_bytes)));
    ui::kv("tokenId", &format!("0x{}", alloy::hex::encode(token_id)));

    // Step A2: Solana — deploy local interchain token
    ui::step_header(2, PHASE_A_STEPS, "Deploy local interchain token (Solana)");
    let spec = crate::types::ITS_CONFIG_SPEC;
    let local_sig = crate::solana::send_its_deploy_interchain_token(
        src_rpc,
        sol_keypair,
        &salt_bytes,
        spec.name,
        spec.symbol,
        spec.decimals,
        INITIAL_SUPPLY,
        None,
    )?;
    ui::tx_hash("solana tx", &local_sig);
    ui::success(&format!(
        "local mint deployed (initial supply {INITIAL_SUPPLY})"
    ));

    // Step A3: Solana — deploy remote interchain token (fires GMP)
    ui::step_header(
        3,
        PHASE_A_STEPS,
        "Deploy remote interchain token (Solana → hub)",
    );
    let remote_sig = crate::solana::send_its_deploy_remote_interchain_token(
        src_rpc,
        sol_keypair,
        &salt_bytes,
        dst_axelar_id.as_str(),
        gas_value,
    )?;
    ui::tx_hash("solana tx", &remote_sig);

    let first_leg_id = crate::solana::extract_its_message_id(src_rpc, &remote_sig)?;
    ui::kv("first-leg message_id", &first_leg_id);

    // Read the actual on-chain CallContractEvent. The verifiers will look up
    // the same fields; using on-chain values eliminates encoding-mismatch risk.
    let gw = crate::solana::extract_gateway_call_contract_payload(src_rpc, &remote_sig)?;
    ui::kv("gateway sender", &gw.sender);
    ui::kv("gateway destination_chain", &gw.destination_chain);
    ui::kv("gateway destination_address", &gw.destination_address);
    ui::kv(
        "gateway payload_hash",
        &format!("0x{}", alloy::hex::encode(gw.payload_hash)),
    );
    ui::kv(
        "gateway payload (len)",
        &format!("{} bytes", gw.payload.len()),
    );

    // Sanity: the local reconstruction should match what the gateway actually saw.
    let local_payload = encode_send_to_hub_deploy(
        dst_axelar_id.as_str(),
        &token_id,
        spec.name,
        spec.symbol,
        spec.decimals,
        None,
    )?;
    let local_hash = keccak256(&local_payload);
    if local_hash.as_slice() != gw.payload_hash {
        ui::warn("local payload reconstruction does not match on-chain payload:");
        ui::warn(&format!(
            "  local  : 0x{}",
            alloy::hex::encode(local_hash.as_slice())
        ));
        ui::warn(&format!(
            "  on-chain: 0x{}",
            alloy::hex::encode(gw.payload_hash)
        ));
    }

    let first_leg_payload = gw.payload.clone();
    let first_leg_payload_hash = FixedBytes::<32>::from(gw.payload_hash);
    let gw_sender = gw.sender.clone();

    // Step A4: drive source → hub via existing relay_to_hub helper
    ui::step_header(
        4,
        PHASE_A_STEPS,
        "Source → hub (verify, route, hub-execute)",
    );
    relay_to_hub(
        src_axelar_id.as_str(),
        &first_leg_id,
        &gw_sender,
        crate::types::HubChain::NAME,
        its_hub_address,
        &first_leg_payload_hash,
        &first_leg_payload,
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        src_cosm_gateway,
        voting_verifier,
        axelarnet_gateway,
    )
    .await?;

    // Step A5..10: hub → destination EVM, manual proof + execute
    let deploy_inner = encode_inner_deploy(&token_id, spec.name, spec.symbol, spec.decimals, &[]);
    let dest_payload_deploy = encode_receive_from_hub(src_axelar_id, &deploy_inner);

    relay_to_destination(
        &first_leg_id,
        src_axelar_id,
        &dest_payload_deploy,
        dst_its_proxy,
        dst_evm_gateway,
        dst_provider,
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        dst_cosm_gateway,
        dst_multisig_prover,
        axelarnet_gateway,
        axelar_rpc,
        5, // step base for ui
        PHASE_A_STEPS,
    )
    .await?;

    // Step A11: verify destination token is deployed
    ui::step_header(11, PHASE_A_STEPS, "Verify destination token deployed");
    let dest_token_addr = its.interchainTokenAddress(token_id_b32).call().await?;
    ui::address("dest token address", &format!("{dest_token_addr}"));
    let token = ERC20::new(dest_token_addr, dst_provider);
    match token.name().call().await {
        Ok(name) => {
            ui::success(&format!("dest token responds to name() → \"{name}\""));
        }
        Err(e) => {
            ui::warn(&format!("dest token name() failed: {e}"));
            ui::info("token may still be propagating — try again or check explorer");
        }
    }

    // Persist for next run.
    let cache = ItsTestCache {
        deployer: sol_pubkey.to_string(),
        salt_hex: format!("0x{}", alloy::hex::encode(salt_bytes)),
        token_id_hex: format!("0x{}", alloy::hex::encode(token_id)),
        dest_token_address: format!("{dest_token_addr}"),
    };
    if let Err(e) = save_cache(cache_file, &cache) {
        ui::warn(&format!(
            "failed to write cache to {}: {e}",
            cache_file.display()
        ));
    } else {
        ui::info(&format!("cached tokenId at {}", cache_file.display()));
    }

    ui::section("Phase A complete");
    ui::success(&format!(
        "deploy + manual relay finished ({})",
        ui::format_elapsed(phase_start)
    ));

    Ok((token_id, dest_token_addr))
}

/// Wait for the destination-chain ITS to deploy the predicted token contract
/// (post hub relay). Uses `name()` instead of `get_code_at` since the latter
/// is unreliable on some EVMs (Flow). Returns the predicted address either
/// way; the caller can decide what to do if name() never responds.
pub(super) async fn poll_for_remote_token_deploy<P: Provider>(
    dest_provider: &P,
    dest_its_addr: Address,
    token_id: FixedBytes<32>,
) -> Result<Address> {
    use super::DEST_CHAIN;
    use super::TOTAL_STEPS;

    let dest_its = InterchainTokenService::new(dest_its_addr, dest_provider);

    ui::step_header(
        7,
        TOTAL_STEPS,
        &format!("Poll {DEST_CHAIN} for token deployment"),
    );
    ui::address(&format!("{DEST_CHAIN} ITS"), &format!("{dest_its_addr}"));
    ui::kv("tokenId", &format!("{token_id}"));

    let predicted_addr = dest_its
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| eyre::eyre!("failed to query interchainTokenAddress on {DEST_CHAIN}: {e}"))?;
    ui::address("predicted token addr", &format!("{predicted_addr}"));

    let spinner = ui::wait_spinner(&format!("Waiting for token to appear on {DEST_CHAIN}..."));
    let mut deployed = false;

    for i in 0..DEST_CHAIN_POLL_ATTEMPTS {
        if i > 0 {
            tokio::time::sleep(DEST_CHAIN_POLL_INTERVAL).await;
        }
        let token = ERC20::new(predicted_addr, dest_provider);
        match token.name().call().await {
            Ok(name) => {
                spinner.finish_and_clear();
                ui::success(&format!("Token responds to name() → \"{name}\""));
                deployed = true;
                break;
            }
            Err(_) => {
                spinner.set_message(format!(
                    "Token not yet deployed (attempt {}/30, addr={predicted_addr})...",
                    i + 1
                ));
            }
        }
    }
    spinner.finish_and_clear();

    if deployed {
        ui::success(&format!("Token deployed on {DEST_CHAIN}!"));
        ui::address(
            &format!("token address ({DEST_CHAIN})"),
            &format!("{predicted_addr}"),
        );
    } else {
        ui::warn(&format!(
            "Token not yet deployed on {DEST_CHAIN} after 5 minutes"
        ));
        ui::info("The relayer may still be processing. Check axelarscan for status.");
        ui::kv("tokenId", &format!("{token_id}"));
    }

    Ok(predicted_addr)
}
