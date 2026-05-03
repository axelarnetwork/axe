mod destination;
mod relay;
mod sender_receiver;
mod source;

use std::path::PathBuf;
use std::time::Instant;

use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner};
use eyre::Result;
use serde_json::json;

use destination::approve_and_execute_evm;
use sender_receiver::ensure_sender_receiver_deployed;
use source::send_evm_call_contract;

use crate::cli::resolve_axelar_id;
use crate::commands::test_helpers::{
    extract_event_attr, extract_poll_id, wait_for_poll_votes, wait_for_proof,
};
use crate::cosmos::{
    build_execute_msg_any, check_axelar_balance, derive_axelar_wallet, read_axelar_config,
    read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::preflight;
use crate::state::read_state;
use crate::timing::{AMPLIFIER_POLL_ATTEMPTS_5MIN, AMPLIFIER_POLL_INTERVAL};
use crate::ui;
use crate::utils::read_contract_address;

const TOTAL_STEPS: usize = 8;

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let mut state = read_state(&axelar_id)?;
    let gmp_start = Instant::now();

    let rpc_url = state.rpc_url.clone();
    let target_json = state.target_json.clone();

    let private_key = state
        .deployer_private_key
        .clone()
        .ok_or_else(|| eyre::eyre!("no deployerPrivateKey in state"))?;

    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);

    preflight::check_deployer_balance(&rpc_url, deployer_address, &target_json, &axelar_id).await?;

    let gateway_addr = read_contract_address(&target_json, &axelar_id, "AxelarGateway")?;
    let gas_service_addr = read_contract_address(&target_json, &axelar_id, "AxelarGasService")?;

    ui::section(&format!("GMP Test: {axelar_id}"));
    ui::address("gateway", &format!("{gateway_addr}"));
    ui::address("gas service", &format!("{gas_service_addr}"));

    let sender_receiver_addr =
        ensure_sender_receiver_deployed(&provider, &mut state, gateway_addr, gas_service_addr)
            .await?;

    let sent =
        send_evm_call_contract(&provider, sender_receiver_addr, &axelar_id, 1, TOTAL_STEPS).await?;
    let source::SentGmp {
        destination_chain,
        destination_address,
        source_address,
        message_id,
        payload_bytes,
        payload_hash,
    } = sent;

    ui::section("Amplifier Routing");

    let (signing_key, axelar_address) = derive_axelar_wallet(&state.mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = read_axelar_config(&target_json)?;

    let cosm_gateway = read_axelar_contract_field(
        &target_json,
        &format!("/axelar/contracts/Gateway/{axelar_id}/address"),
    )?;
    let voting_verifier = read_axelar_contract_field(
        &target_json,
        &format!("/axelar/contracts/VotingVerifier/{axelar_id}/address"),
    )?;
    let multisig_prover = read_axelar_contract_field(
        &target_json,
        &format!("/axelar/contracts/MultisigProver/{axelar_id}/address"),
    )?;

    ui::address("cosmos gateway", &cosm_gateway);
    ui::address("voting verifier", &voting_verifier);
    ui::address("axelar address", &axelar_address);

    let gmp_msg = json!({
        "cc_id": {
            "message_id": message_id,
            "source_chain": axelar_id,
        },
        "destination_chain": destination_chain,
        "destination_address": destination_address,
        "source_address": source_address,
        "payload_hash": alloy::hex::encode(payload_hash.as_slice()),
    });

    let ctx = relay::AmplifierContext {
        signing_key: &signing_key,
        axelar_address: &axelar_address,
        lcd: &lcd,
        chain_id: &chain_id,
        fee_denom: &fee_denom,
        gas_price,
        cosm_gateway: &cosm_gateway,
        voting_verifier: &voting_verifier,
        multisig_prover: &multisig_prover,
    };
    let execute_data_hex =
        relay::run_full_sequence(&ctx, &gmp_msg, &axelar_id, &message_id, TOTAL_STEPS).await?;

    approve_and_execute_evm(
        &provider,
        gateway_addr,
        sender_receiver_addr,
        &axelar_id,
        &execute_data_hex,
        &payload_bytes,
        payload_hash,
        7,
        8,
        TOTAL_STEPS,
    )
    .await?;

    ui::section("Complete");
    ui::success(&format!(
        "GMP flow complete ({})",
        ui::format_elapsed(gmp_start)
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Config-based GMP test (supports EVM + Solana)
// ---------------------------------------------------------------------------

pub async fn run_config(
    config: PathBuf,
    source_chain: Option<String>,
    destination_chain: Option<String>,
    mnemonic_override: Option<String>,
) -> Result<()> {
    let config_content =
        std::fs::read_to_string(&config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let chains = config_root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' in config"))?;

    // Resolve source and destination chains
    let src = source_chain.ok_or_else(|| eyre::eyre!("--source-chain required with --config"))?;
    let dst = destination_chain.unwrap_or_else(|| src.clone());

    let src_type: crate::types::ChainType = chains
        .get(&src)
        .and_then(|v| v.get("chainType"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("source chain '{src}' not found in config"))?
        .parse()?;
    let dst_type: crate::types::ChainType = chains
        .get(&dst)
        .and_then(|v| v.get("chainType"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("destination chain '{dst}' not found in config"))?
        .parse()?;

    let src_rpc = chains
        .get(&src)
        .and_then(|v| v.get("rpc"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no RPC for source chain '{src}'"))?;

    let gmp_start = Instant::now();
    ui::section(&format!("GMP Test: {src} → {dst}"));
    ui::kv("source", &format!("{src} ({src_type})"));
    ui::kv("destination", &format!("{dst} ({dst_type})"));

    // --- Preflight: derive Axelar wallet and check it can pay for the relay ---
    let mnemonic = mnemonic_override
        .clone()
        .or_else(|| std::env::var("MNEMONIC").ok())
        .ok_or_else(|| eyre::eyre!("MNEMONIC env var or --mnemonic required for relay"))?;
    let (signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = read_axelar_config(&config)?;

    ui::section("Preflight");
    ui::address("axelar address", &axelar_address);
    // Min: 4 relay txs at ~5k uaxl each + headroom = 0.1 AXL.
    const MIN_RELAY_BALANCE_UAXL: u128 = 100_000;
    check_axelar_balance(
        &lcd,
        &chain_id,
        &axelar_address,
        &fee_denom,
        MIN_RELAY_BALANCE_UAXL,
    )
    .await?;

    // Solana keypair balance checks: catch underfunded keys here with a clear
    // error rather than the cryptic "Attempt to debit an account but found no
    // record of a prior credit" we get from the RPC at send-time.
    use crate::types::ChainType;
    if src_type == ChainType::Svm || dst_type == ChainType::Svm {
        use solana_sdk::signer::Signer;
        let keypair = crate::solana::load_keypair(None)?;
        if src_type == ChainType::Svm {
            crate::solana::check_solana_balance(
                src_rpc,
                "source",
                &keypair.pubkey(),
                crate::solana::MIN_SOL_SEND_LAMPORTS,
            )?;
        }
        if dst_type == ChainType::Svm {
            let dst_rpc = chains
                .get(&dst)
                .and_then(|v| v.get("rpc"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{dst}'"))?;
            crate::solana::check_solana_balance(
                dst_rpc,
                "destination",
                &keypair.pubkey(),
                crate::solana::MIN_SOL_RELAY_LAMPORTS,
            )?;
        }
    }

    let sent = match src_type {
        ChainType::Svm => source::send_svm_call_contract(src_rpc, &dst, 1, 8)?,
        ChainType::Evm => {
            return Err(eyre::eyre!(
                "EVM source not yet supported in config mode. Use --axelar-id for EVM chains."
            ));
        }
    };
    let source::SentGmp {
        destination_chain: _,
        destination_address,
        source_address,
        message_id,
        payload_bytes,
        payload_hash,
    } = sent;
    let payload_hash_hex = alloy::hex::encode(payload_hash);

    // --- Cosmos relay (steps 2-6, chain-agnostic) ---
    let cosm_gateway =
        read_axelar_contract_field(&config, &format!("/axelar/contracts/Gateway/{src}/address"))?;
    let voting_verifier = read_axelar_contract_field(
        &config,
        &format!("/axelar/contracts/VotingVerifier/{src}/address"),
    )
    .ok();

    ui::section("Amplifier Routing");
    ui::address("cosmos gateway", &cosm_gateway);
    if let Some(ref vv) = voting_verifier {
        ui::address("voting verifier", vv);
    }
    ui::address("axelar address", &axelar_address);

    let gmp_msg = json!({
        "cc_id": {
            "message_id": message_id,
            "source_chain": src,
        },
        "destination_chain": dst,
        "destination_address": destination_address,
        "source_address": source_address,
        "payload_hash": payload_hash_hex,
    });

    // Step 2: verify_messages
    ui::step_header(2, 8, "verify_messages");
    let verify_msg = json!({ "verify_messages": [gmp_msg] });
    let verify_any = build_execute_msg_any(&axelar_address, &cosm_gateway, &verify_msg)?;
    let verify_resp = sign_and_broadcast_cosmos_tx(
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        vec![verify_any],
    )
    .await?;

    if let Some(poll_id) = extract_poll_id(&verify_resp) {
        ui::kv("poll_id", &poll_id);

        // Step 3: Wait for votes + end poll
        ui::step_header(3, 8, "Wait for poll votes + end poll");
        if let Some(ref vv) = voting_verifier {
            wait_for_poll_votes(&lcd, vv, &poll_id).await?;
        }

        let vv_addr = voting_verifier
            .as_ref()
            .ok_or_else(|| eyre::eyre!("voting verifier address required to end poll"))?;
        let spinner = ui::wait_spinner("Ending poll (waiting for block expiry)...");
        for attempt in 0..AMPLIFIER_POLL_ATTEMPTS_5MIN {
            if attempt > 0 {
                tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
            }
            let end_poll_msg = json!({ "end_poll": { "poll_id": poll_id } });
            let end_poll_any = build_execute_msg_any(&axelar_address, vv_addr, &end_poll_msg)?;
            match sign_and_broadcast_cosmos_tx(
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                vec![end_poll_any],
            )
            .await
            {
                Ok(_) => {
                    spinner.finish_and_clear();
                    ui::success("poll ended");
                    break;
                }
                Err(e) => {
                    let msg = format!("{e}");
                    if msg.contains("cannot tally before poll end") {
                        spinner.set_message(format!(
                            "Poll not expired yet (attempt {})...",
                            attempt + 1
                        ));
                        continue;
                    }
                    spinner.finish_and_clear();
                    return Err(e);
                }
            }
        }
    } else {
        ui::info("no new poll — message already being verified by active verifiers");
        ui::step_header(3, 8, "Wait for poll votes + end poll");
        ui::info("skipped (existing poll)");
    }

    // Step 4: route_messages
    ui::step_header(4, 8, "route_messages");
    let _dest_gateway =
        read_axelar_contract_field(&config, &format!("/axelar/contracts/Gateway/{dst}/address"))?;
    let spinner = ui::wait_spinner("Routing message...");
    for attempt in 0..AMPLIFIER_POLL_ATTEMPTS_5MIN {
        if attempt > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        let route_msg = json!({ "route_messages": [gmp_msg] });
        let route_any = build_execute_msg_any(&axelar_address, &cosm_gateway, &route_msg)?;
        match sign_and_broadcast_cosmos_tx(
            &signing_key,
            &axelar_address,
            &lcd,
            &chain_id,
            &fee_denom,
            gas_price,
            vec![route_any],
        )
        .await
        {
            Ok(_) => {
                spinner.finish_and_clear();
                ui::success("message routed");
                break;
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("not verified") {
                    spinner.set_message(format!(
                        "Message not yet verified (attempt {}/60)...",
                        attempt + 1
                    ));
                    continue;
                }
                spinner.finish_and_clear();
                return Err(e);
            }
        }
    }

    // Step 5: construct_proof
    ui::step_header(5, 8, "construct_proof");
    let multisig_prover = read_axelar_contract_field(
        &config,
        &format!("/axelar/contracts/MultisigProver/{dst}/address"),
    )?;
    ui::address("multisig prover", &multisig_prover);

    let construct_proof_msg = json!({
        "construct_proof": [{
            "source_chain": src,
            "message_id": message_id,
        }]
    });
    let construct_any =
        build_execute_msg_any(&axelar_address, &multisig_prover, &construct_proof_msg)?;
    let construct_resp = sign_and_broadcast_cosmos_tx(
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        vec![construct_any],
    )
    .await?;

    let session_id = extract_event_attr(&construct_resp, "multisig_session_id")?;
    ui::kv("multisig_session_id", &session_id);

    // Step 6: Wait for proof
    ui::step_header(6, 8, "Wait for proof signing");
    let proof = wait_for_proof(&lcd, &multisig_prover, &session_id).await?;
    ui::success("proof ready");

    // --- Steps 7-8: destination-specific ---
    let execute_data_hex = proof["status"]["completed"]["execute_data"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no execute_data in proof response"))?;

    match dst_type {
        ChainType::Svm => {
            // Step 7: Approve on Solana gateway
            ui::step_header(7, 8, "Approve on Solana gateway");
            let dst_rpc = chains
                .get(&dst)
                .and_then(|v| v.get("rpc"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{dst}'"))?;

            let keypair = crate::solana::load_keypair(None)?;
            let execute_data = crate::solana::decode_execute_data(execute_data_hex)?;
            crate::solana::approve_messages_on_gateway(dst_rpc, &keypair, &execute_data)?;

            // Step 8: Execute on destination (memo program)
            ui::step_header(8, 8, "Execute on destination");

            // Build the Message struct matching what was sent in step 1
            let gmp_message = solana_axelar_std::Message {
                cc_id: solana_axelar_std::CrossChainId {
                    chain: src.clone(),
                    id: message_id.clone(),
                },
                source_address: source_address.clone(),
                destination_chain: dst.clone(),
                destination_address: destination_address.clone(),
                payload_hash: {
                    let bytes = alloy::hex::decode(&payload_hash_hex)?;
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&bytes);
                    arr
                },
            };

            let memo_sig =
                crate::solana::execute_on_memo(dst_rpc, &keypair, gmp_message, &payload_bytes)?;
            ui::tx_hash("execute", &memo_sig.to_string());
        }
        ChainType::Evm => {
            return Err(eyre::eyre!(
                "EVM destination not yet supported in config mode. Use --axelar-id for EVM chains."
            ));
        }
    }

    ui::section("Complete");
    ui::success(&format!(
        "GMP flow complete ({})",
        ui::format_elapsed(gmp_start)
    ));

    Ok(())
}
