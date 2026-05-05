use anchor_lang::InstructionData;
use eyre::Result;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signature, read_keypair_file},
    signer::Signer,
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use std::time::Instant;

use crate::commands::load_test::metrics::TxMetrics;

// ---------------------------------------------------------------------------
// ITS PDA helpers
// ---------------------------------------------------------------------------

const ITS_SEED: &[u8] = b"interchain-token-service";
const TOKEN_MANAGER_SEED: &[u8] = b"token-manager";
const INTERCHAIN_TOKEN_SEED: &[u8] = b"interchain-token";
const PREFIX_INTERCHAIN_TOKEN_SALT: &[u8] = b"interchain-token-salt";
const PREFIX_INTERCHAIN_TOKEN_ID: &[u8] = b"interchain-token-id";

pub fn find_its_root_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[ITS_SEED], &solana_axelar_its::id())
}

pub fn find_token_manager_pda(its_root: &Pubkey, token_id: &[u8; 32]) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[TOKEN_MANAGER_SEED, its_root.as_ref(), token_id],
        &solana_axelar_its::id(),
    )
}

pub fn find_interchain_token_pda(its_root: &Pubkey, token_id: &[u8]) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[INTERCHAIN_TOKEN_SEED, its_root.as_ref(), token_id],
        &solana_axelar_its::id(),
    )
}

fn spl_associated_token_account_program_id() -> Pubkey {
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
}

fn mpl_token_metadata_program_id() -> Pubkey {
    Pubkey::from_str_const("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s")
}

fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &spl_associated_token_account_program_id(),
    )
    .0
}

/// Derive the interchain token ID from deployer and salt.
pub fn interchain_token_id(deployer: &Pubkey, salt: &[u8; 32]) -> [u8; 32] {
    let chain_name_hash = solana_axelar_its::CHAIN_NAME_HASH;
    let deploy_salt = solana_sdk::keccak::hashv(&[
        PREFIX_INTERCHAIN_TOKEN_SALT,
        &chain_name_hash,
        deployer.as_ref(),
        salt,
    ])
    .to_bytes();
    solana_sdk::keccak::hashv(&[PREFIX_INTERCHAIN_TOKEN_ID, &deploy_salt]).to_bytes()
}

/// Load a Solana keypair from a file path, or fall back to ~/.config/solana/id.json.
pub fn load_keypair(path: Option<&str>) -> Result<Keypair> {
    let key_path = match path {
        Some(p) => p.to_string(),
        None => {
            let home =
                dirs::home_dir().ok_or_else(|| eyre::eyre!("cannot determine home directory"))?;
            home.join(".config/solana/id.json")
                .to_string_lossy()
                .into_owned()
        }
    };
    read_keypair_file(&key_path)
        .map_err(|e| eyre::eyre!("failed to read Solana keypair from {key_path}: {e}"))
}

/// Send a call_contract instruction to the Solana Axelar Gateway.
/// Returns the transaction signature and per-tx metrics.
pub fn send_call_contract(
    rpc_url: &str,
    keypair: &dyn Signer,
    destination_chain: &str,
    destination_address: &str,
    payload: &[u8],
) -> Result<(String, TxMetrics)> {
    let submit_start = Instant::now();
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());

    let gateway_config_pda = solana_axelar_gateway::GatewayConfig::find_pda().0;
    let (event_authority_pda, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &solana_axelar_gateway::id());

    let ix_data = solana_axelar_gateway::instruction::CallContract {
        destination_chain: destination_chain.to_string(),
        destination_contract_address: destination_address.to_string(),
        payload: payload.to_vec(),
        signing_pda_bump: 0,
    }
    .data();

    let fee_payer = keypair.pubkey();
    let accounts = vec![
        AccountMeta::new(fee_payer, true),
        AccountMeta::new(fee_payer, true),
        AccountMeta::new_readonly(gateway_config_pda, false),
        AccountMeta::new_readonly(event_authority_pda, false),
        AccountMeta::new_readonly(solana_axelar_gateway::id(), false),
    ];

    let call_contract_ix = Instruction {
        program_id: solana_axelar_gateway::id(),
        accounts,
        data: ix_data,
    };

    // Pay gas on non-devnet environments so the relayer picks up the message.
    #[cfg(not(feature = "devnet-amplifier"))]
    let pay_gas_ix = {
        let payload_hash: [u8; 32] = solana_sdk::keccak::hash(payload).to_bytes();
        let treasury_pda = solana_axelar_gas_service::Treasury::find_pda().0;
        let gas_event_authority =
            Pubkey::find_program_address(&[b"__event_authority"], &solana_axelar_gas_service::id())
                .0;

        let pay_gas_data = solana_axelar_gas_service::instruction::PayGas {
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            payload_hash,
            amount: 10_000_000, // 0.01 SOL — enough for testnet relayer pickup
            refund_address: fee_payer,
        }
        .data();

        Instruction {
            program_id: solana_axelar_gas_service::id(),
            accounts: vec![
                AccountMeta::new(fee_payer, true),
                AccountMeta::new(treasury_pda, false),
                AccountMeta::new_readonly(
                    Pubkey::from_str_const("11111111111111111111111111111111"),
                    false,
                ),
                AccountMeta::new_readonly(gas_event_authority, false),
                AccountMeta::new_readonly(solana_axelar_gas_service::id(), false),
            ],
            data: pay_gas_data,
        }
    };

    #[cfg(not(feature = "devnet-amplifier"))]
    let instructions = vec![pay_gas_ix, call_contract_ix];
    #[cfg(feature = "devnet-amplifier")]
    let instructions = vec![call_contract_ix];

    let blockhash = rpc_client.get_latest_blockhash()?;
    let message = Message::new_with_blockhash(&instructions, Some(&fee_payer), &blockhash);
    let mut transaction = Transaction::new_unsigned(message);
    transaction.sign(&[keypair], blockhash);

    #[allow(clippy::cast_possible_truncation)]
    let submit_time_ms = submit_start.elapsed().as_millis() as u64;

    let signature = rpc_client.send_and_confirm_transaction(&transaction)?;

    #[allow(clippy::cast_possible_truncation)]
    let confirm_time_ms = submit_start.elapsed().as_millis() as u64;
    let latency_ms = confirm_time_ms.saturating_sub(submit_time_ms);

    let (compute_units, slot) = fetch_tx_details(&rpc_client, &signature).unwrap_or((None, None));

    let metrics = TxMetrics {
        signature: signature.to_string(),
        submit_time_ms,
        confirm_time_ms: Some(confirm_time_ms),
        latency_ms: Some(latency_ms),
        compute_units,
        slot,
        success: true,
        error: None,
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        payload: Vec::new(),
        send_instant: None,
        amplifier_timing: None,
    };

    Ok((signature.to_string(), metrics))
}

/// 1-based instruction index of `call_contract` in the transaction.
/// When a `pay_gas` instruction is prepended (non-devnet), call_contract is at index 2.
pub fn solana_call_contract_index() -> u8 {
    #[cfg(not(feature = "devnet-amplifier"))]
    {
        2
    }
    #[cfg(feature = "devnet-amplifier")]
    {
        1
    }
}

// ---------------------------------------------------------------------------
// ITS instruction builders
// ---------------------------------------------------------------------------

/// Deploy an interchain token on Solana.
/// Returns the transaction signature.
#[allow(clippy::too_many_arguments)]
pub fn send_its_deploy_interchain_token(
    rpc_url: &str,
    keypair: &dyn Signer,
    salt: &[u8; 32],
    name: &str,
    symbol: &str,
    decimals: u8,
    initial_supply: u64,
    minter: Option<&Pubkey>,
) -> Result<String> {
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let fee_payer = keypair.pubkey();
    let deployer = fee_payer;

    let token_id = interchain_token_id(&deployer, salt);
    let (its_root_pda, _) = find_its_root_pda();
    let (mint, _) = find_interchain_token_pda(&its_root_pda, &token_id);
    let (token_manager_pda, _) = find_token_manager_pda(&its_root_pda, &token_id);

    let token_program = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let associated_token_program = spl_associated_token_account_program_id();
    let mpl_metadata_program = mpl_token_metadata_program_id();

    let deployer_ata = get_associated_token_address(&deployer, &mint, &token_program);
    let token_manager_ata = get_associated_token_address(&token_manager_pda, &mint, &token_program);

    let (mpl_metadata_account, _) = Pubkey::find_program_address(
        &[b"metadata", mpl_metadata_program.as_ref(), mint.as_ref()],
        &mpl_metadata_program,
    );

    let (event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &solana_axelar_its::id());

    let (minter_account, minter_roles_pda) = if let Some(m) = minter {
        let (roles, _) = Pubkey::find_program_address(
            &[b"user-roles", token_manager_pda.as_ref(), m.as_ref()],
            &solana_axelar_its::id(),
        );
        (*m, roles)
    } else {
        (solana_axelar_its::id(), solana_axelar_its::id())
    };

    let accounts = vec![
        AccountMeta::new(fee_payer, true),
        AccountMeta::new_readonly(deployer, true),
        AccountMeta::new_readonly(
            Pubkey::from_str_const("11111111111111111111111111111111"),
            false,
        ),
        AccountMeta::new_readonly(its_root_pda, false),
        AccountMeta::new(token_manager_pda, false),
        AccountMeta::new(mint, false),
        AccountMeta::new(token_manager_ata, false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(associated_token_program, false),
        AccountMeta::new_readonly(solana_sdk::sysvar::instructions::id(), false),
        AccountMeta::new_readonly(mpl_metadata_program, false),
        AccountMeta::new(mpl_metadata_account, false),
        AccountMeta::new(deployer_ata, false),
        AccountMeta::new_readonly(minter_account, false),
        AccountMeta::new(minter_roles_pda, false),
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(solana_axelar_its::id(), false),
    ];

    let ix_data = solana_axelar_its::instruction::DeployInterchainToken {
        salt: *salt,
        name: name.to_string(),
        symbol: symbol.to_string(),
        decimals,
        initial_supply,
    }
    .data();

    let ix = Instruction {
        program_id: solana_axelar_its::id(),
        accounts,
        data: ix_data,
    };

    let blockhash = rpc_client.get_latest_blockhash()?;
    let message = Message::new_with_blockhash(&[ix], Some(&fee_payer), &blockhash);
    let mut transaction = Transaction::new_unsigned(message);
    transaction.sign(&[keypair], blockhash);

    let signature = rpc_client.send_and_confirm_transaction(&transaction)?;
    Ok(signature.to_string())
}

/// Deploy a remote interchain token from Solana to a destination chain.
/// Returns the transaction signature.
pub fn send_its_deploy_remote_interchain_token(
    rpc_url: &str,
    keypair: &dyn Signer,
    salt: &[u8; 32],
    destination_chain: &str,
    gas_value: u64,
) -> Result<String> {
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let fee_payer = keypair.pubkey();
    let deployer = fee_payer;

    let token_id = interchain_token_id(&deployer, salt);
    let (its_root_pda, _) = find_its_root_pda();
    let (mint, _) = find_interchain_token_pda(&its_root_pda, &token_id);
    let (token_manager_pda, _) = find_token_manager_pda(&its_root_pda, &token_id);

    let mpl_metadata_program = mpl_token_metadata_program_id();
    let (metadata_account, _) = Pubkey::find_program_address(
        &[b"metadata", mpl_metadata_program.as_ref(), mint.as_ref()],
        &mpl_metadata_program,
    );

    let gateway_program = solana_axelar_gateway::id();
    let (gateway_root_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_program);
    let (call_contract_signing_pda, _) =
        Pubkey::find_program_address(&[b"gtw-call-contract"], &solana_axelar_its::id());
    let (gateway_event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &gateway_program);

    let gas_service_program = solana_axelar_gas_service::id();
    let (gas_treasury, _) = Pubkey::find_program_address(&[b"gas-service"], &gas_service_program);
    let (gas_event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &gas_service_program);

    let (event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &solana_axelar_its::id());

    let accounts = vec![
        AccountMeta::new(fee_payer, true),
        AccountMeta::new_readonly(deployer, true),
        AccountMeta::new_readonly(mint, false),
        AccountMeta::new_readonly(metadata_account, false),
        AccountMeta::new_readonly(token_manager_pda, false),
        AccountMeta::new_readonly(gateway_root_pda, false),
        AccountMeta::new_readonly(gateway_program, false),
        AccountMeta::new_readonly(
            Pubkey::from_str_const("11111111111111111111111111111111"),
            false,
        ),
        AccountMeta::new_readonly(its_root_pda, false),
        AccountMeta::new_readonly(call_contract_signing_pda, false),
        AccountMeta::new_readonly(gateway_event_authority, false),
        AccountMeta::new(gas_treasury, false),
        AccountMeta::new_readonly(gas_service_program, false),
        AccountMeta::new_readonly(gas_event_authority, false),
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(solana_axelar_its::id(), false),
    ];

    let ix_data = solana_axelar_its::instruction::DeployRemoteInterchainToken {
        salt: *salt,
        destination_chain: destination_chain.to_string(),
        gas_value,
    }
    .data();

    let ix = Instruction {
        program_id: solana_axelar_its::id(),
        accounts,
        data: ix_data,
    };

    let blockhash = rpc_client.get_latest_blockhash()?;
    let message = Message::new_with_blockhash(&[ix], Some(&fee_payer), &blockhash);
    let mut transaction = Transaction::new_unsigned(message);
    transaction.sign(&[keypair], blockhash);

    let signature = rpc_client.send_and_confirm_transaction(&transaction)?;
    Ok(signature.to_string())
}

/// Send an ITS `InterchainTransfer` instruction.
/// Returns the transaction signature and per-tx metrics.
#[allow(clippy::too_many_arguments)]
pub fn send_its_interchain_transfer(
    rpc_url: &str,
    keypair: &dyn Signer,
    token_id: &[u8; 32],
    source_account: &Pubkey,
    mint: &Pubkey,
    destination_chain: &str,
    destination_address: &[u8],
    amount: u64,
    gas_value: u64,
) -> Result<(String, TxMetrics)> {
    let submit_start = Instant::now();
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let fee_payer = keypair.pubkey();

    let (its_root_pda, _) = find_its_root_pda();
    let (token_manager_pda, _) = find_token_manager_pda(&its_root_pda, token_id);

    let token_program = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let token_manager_ata = get_associated_token_address(&token_manager_pda, mint, &token_program);

    let gateway_program = solana_axelar_gateway::id();
    let (gateway_root_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_program);
    let (call_contract_signing_pda, _) =
        Pubkey::find_program_address(&[b"gtw-call-contract"], &solana_axelar_its::id());
    let (gateway_event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &gateway_program);

    let gas_service_program = solana_axelar_gas_service::id();
    let (gas_treasury, _) = Pubkey::find_program_address(&[b"gas-service"], &gas_service_program);
    let (gas_event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &gas_service_program);

    let (event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &solana_axelar_its::id());

    let accounts = vec![
        AccountMeta::new(fee_payer, true),
        AccountMeta::new_readonly(fee_payer, true), // authority = fee_payer
        AccountMeta::new_readonly(gateway_root_pda, false),
        AccountMeta::new_readonly(gateway_event_authority, false),
        AccountMeta::new_readonly(gateway_program, false),
        AccountMeta::new_readonly(call_contract_signing_pda, false),
        AccountMeta::new(gas_treasury, false),
        AccountMeta::new_readonly(gas_service_program, false),
        AccountMeta::new_readonly(gas_event_authority, false),
        AccountMeta::new_readonly(its_root_pda, false),
        AccountMeta::new(token_manager_pda, false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new(*mint, false),
        AccountMeta::new(*source_account, false),
        AccountMeta::new(token_manager_ata, false),
        AccountMeta::new_readonly(
            Pubkey::from_str_const("11111111111111111111111111111111"),
            false,
        ),
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(solana_axelar_its::id(), false),
    ];

    let ix_data = solana_axelar_its::instruction::InterchainTransfer {
        token_id: *token_id,
        destination_chain: destination_chain.to_string(),
        destination_address: destination_address.to_vec(),
        amount,
        gas_value,
        caller_program_id: None,
        caller_pda_seeds: None,
        data: None,
    }
    .data();

    let ix = Instruction {
        program_id: solana_axelar_its::id(),
        accounts,
        data: ix_data,
    };

    let blockhash = rpc_client.get_latest_blockhash()?;
    let message = Message::new_with_blockhash(&[ix], Some(&fee_payer), &blockhash);
    let mut transaction = Transaction::new_unsigned(message);
    transaction.sign(&[keypair], blockhash);

    #[allow(clippy::cast_possible_truncation)]
    let submit_time_ms = submit_start.elapsed().as_millis() as u64;

    let signature = match rpc_client.send_and_confirm_transaction(&transaction) {
        Ok(sig) => sig,
        Err(send_err) => {
            // Pre-flight rejected the tx — re-run simulate to capture the
            // program logs the original error swallows. This makes
            // diagnostics actionable instead of "Program failed to complete; N".
            let log_dump = match rpc_client.simulate_transaction(&transaction) {
                Ok(sim) => {
                    let v = sim.value;
                    let logs = v.logs.unwrap_or_default();
                    let header = match v.err {
                        Some(e) => format!("simulation error: {e:?}"),
                        None => "simulation succeeded but submit failed".to_string(),
                    };
                    if logs.is_empty() {
                        header
                    } else {
                        let body = logs
                            .iter()
                            .map(|l| format!("    {l}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("{header}\n  program logs:\n{body}")
                    }
                }
                Err(sim_err) => format!("simulate_transaction follow-up failed: {sim_err}"),
            };
            return Err(eyre::eyre!("{send_err}\n  ↳ {log_dump}"));
        }
    };

    #[allow(clippy::cast_possible_truncation)]
    let confirm_time_ms = submit_start.elapsed().as_millis() as u64;
    let latency_ms = confirm_time_ms.saturating_sub(submit_time_ms);

    let (compute_units, slot) = fetch_tx_details(&rpc_client, &signature).unwrap_or((None, None));

    let metrics = TxMetrics {
        signature: signature.to_string(),
        submit_time_ms,
        confirm_time_ms: Some(confirm_time_ms),
        latency_ms: Some(latency_ms),
        compute_units,
        slot,
        success: true,
        error: None,
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        payload: Vec::new(),
        send_instant: None,
        amplifier_timing: None,
    };

    Ok((signature.to_string(), metrics))
}

fn fetch_tx_details(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<(Option<u64>, Option<u64>)> {
    let tx = fetch_confirmed_tx(rpc_client, signature)?;
    match tx {
        Some(tx) => {
            let slot = Some(tx.slot);
            let compute_units = tx
                .transaction
                .meta
                .and_then(|m| Option::from(m.compute_units_consumed));
            Ok((compute_units, slot))
        }
        None => Ok((None, None)),
    }
}

/// Fetch a confirmed transaction with retries.
///
/// Public Solana devnet RPC (api.devnet.solana.com) often takes 30+ seconds
/// to index a freshly-confirmed transaction so it's queryable via
/// getTransaction. Use a generous retry budget (~60s wall-clock) before
/// giving up, since the alternative — guessing the message_id — costs the
/// caller a full 5-minute pipeline timeout downstream.
fn fetch_confirmed_tx(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<Option<solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta>> {
    // Slight upfront delay — `send_and_confirm_transaction` only guarantees
    // the tx is in `confirmed`, not that it's been backfilled into the
    // history endpoint queried by `getTransaction`.
    std::thread::sleep(std::time::Duration::from_millis(750));
    for i in 0..15 {
        match rpc_client.get_transaction(signature, UiTransactionEncoding::Json) {
            Ok(tx) => return Ok(Some(tx)),
            Err(_) => {
                // Exponential backoff capped at 5s: 500ms, 1s, 2s, 4s, 5s, 5s, …
                let delay = std::cmp::min(500u64 * (1 << i), 5000);
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
        }
    }
    Ok(None)
}

/// Extract the ITS gateway message ID from a confirmed transaction.
///
/// ITS instructions invoke the gateway's `call_contract` via CPI. The Solana
/// message ID format is `{signature}-{top_ix}.{inner_ix}` where `inner_ix` is
/// the index of the gateway CPI in the flattened inner-instruction list.
///
/// The inner index varies depending on whether the ITS program calls `pay_gas`
/// before `call_contract` (gas_value > 0 adds extra CPIs), and on the
/// program version / instruction layout.
///
/// We parse the transaction logs and count every CPI invocation (depth >= 2)
/// sequentially. Each such invocation corresponds to one entry in the
/// inner_instructions array. We find the last gateway invoke, which is the
/// `call_contract` that emits the `CallContractEvent`.
pub fn extract_its_message_id(rpc_url: &str, signature_str: &str) -> Result<String> {
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let sig: Signature = signature_str
        .parse()
        .map_err(|e| eyre::eyre!("invalid signature: {e}"))?;

    let tx = fetch_confirmed_tx(&rpc_client, &sig)?
        .ok_or_else(|| eyre::eyre!("could not fetch transaction {signature_str}"))?;

    let meta = tx
        .transaction
        .meta
        .ok_or_else(|| eyre::eyre!("transaction has no metadata"))?;

    let logs: Vec<String> = match meta.log_messages {
        solana_transaction_status::option_serializer::OptionSerializer::Some(logs) => logs,
        _ => return Err(eyre::eyre!("transaction has no log messages")),
    };

    let gateway_id = solana_axelar_gateway::id().to_string();

    // Parse logs to find which top-level instruction invoked the gateway and
    // which inner CPI within that instruction is the event emit.
    //
    // Log pattern:
    //   "Program X invoke [1]"  → top-level instruction start (depth 1)
    //   "Program X invoke [N]"  → CPI at depth N (inner instruction)
    //   "Program X success"     → instruction end
    //
    // We track which top-level instruction we're in (1-based group index)
    // and count inner CPIs within each group separately.
    let mut top_level_index: u32 = 0; // 1-based
    let mut inner_ix_counter: u32 = 0; // 0-based within current group
    let mut found_group: Option<u32> = None;
    let mut found_inner_idx: Option<u32> = None;

    for log in &logs {
        if let Some(rest) = log.strip_prefix("Program ") {
            if rest.ends_with(" invoke [1]") {
                // New top-level instruction
                top_level_index += 1;
                inner_ix_counter = 0;
            } else if rest.contains(" invoke [") {
                // Inner CPI
                if rest.split(" invoke [").next() == Some(gateway_id.as_str()) {
                    found_group = Some(top_level_index);
                    // The emit_cpi! after this invoke adds one more inner instruction
                    // that doesn't produce an "invoke" log. The message ID references
                    // that emit instruction, so we add 1.
                    found_inner_idx = Some(inner_ix_counter + 1);
                }
                inner_ix_counter += 1;
            }
        }
    }

    match (found_group, found_inner_idx) {
        (Some(group), Some(idx)) => {
            // Convert to 1-based indexing for the message ID format
            Ok(format!("{signature_str}-{group}.{idx}"))
        }
        _ => Err(eyre::eyre!(
            "could not find gateway call_contract CPI in transaction logs"
        )),
    }
}

// ---------------------------------------------------------------------------
// Gateway approval flow (manual relay for Solana destination)
// ---------------------------------------------------------------------------

use solana_axelar_std::execute_data::ExecuteData;
use solana_axelar_std::{MerklizedMessage, PayloadType, SigningVerifierSetInfo};

/// Deserialize the execute_data hex string from a Cosmos proof response
/// into the Solana `ExecuteData` struct.
#[allow(dead_code)]
pub fn decode_execute_data(execute_data_hex: &str) -> Result<ExecuteData> {
    let bytes = hex::decode(execute_data_hex)?;
    let execute_data: ExecuteData = borsh::BorshDeserialize::try_from_slice(&bytes)?;
    Ok(execute_data)
}

/// Step 7a: Initialize a payload verification session on the Solana gateway.
/// This creates the session PDA that tracks signature verification progress.
#[allow(dead_code)]
pub fn initialize_verification_session(
    rpc_url: &str,
    payer: &Keypair,
    payload_merkle_root: [u8; 32],
    signing_verifier_set_merkle_root: [u8; 32],
) -> Result<Signature> {
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let gateway_id = solana_axelar_gateway::id();

    let gateway_config_pda = solana_axelar_gateway::GatewayConfig::find_pda().0;

    // Derive VerifierSetTracker PDA
    let (verifier_set_tracker_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::VerifierSetTracker::SEED_PREFIX,
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    // Derive verification session PDA
    let payload_type_byte: u8 = PayloadType::ApproveMessages.into();
    let (verification_session_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::SignatureVerificationSessionData::SEED_PREFIX,
            &payload_merkle_root,
            &[payload_type_byte],
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    let ix_data = solana_axelar_gateway::instruction::InitializePayloadVerificationSession {
        merkle_root: payload_merkle_root,
        payload_type: PayloadType::ApproveMessages,
    };

    let ix = Instruction {
        program_id: gateway_id,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(gateway_config_pda, false),
            AccountMeta::new(verification_session_pda, false),
            AccountMeta::new_readonly(verifier_set_tracker_pda, false),
            AccountMeta::new_readonly(anchor_lang::prelude::system_program::ID, false),
        ],
        data: ix_data.data(),
    };

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    let sig = rpc.send_and_confirm_transaction_with_spinner(&tx)?;
    Ok(sig)
}

/// Step 7b: Verify a single signature against the verification session.
/// Called once per signer. Can be called in parallel.
#[allow(dead_code)]
pub fn verify_signature(
    rpc_url: &str,
    payer: &Keypair,
    payload_merkle_root: [u8; 32],
    signing_verifier_set_merkle_root: [u8; 32],
    verifier_info: SigningVerifierSetInfo,
) -> Result<Signature> {
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let gateway_id = solana_axelar_gateway::id();

    let gateway_config_pda = solana_axelar_gateway::GatewayConfig::find_pda().0;

    let payload_type_byte: u8 = verifier_info.payload_type.into();
    let (verification_session_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::SignatureVerificationSessionData::SEED_PREFIX,
            &payload_merkle_root,
            &[payload_type_byte],
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    let (verifier_set_tracker_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::VerifierSetTracker::SEED_PREFIX,
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    let ix_data = solana_axelar_gateway::instruction::VerifySignature {
        payload_merkle_root,
        verifier_info,
    };

    let ix = Instruction {
        program_id: gateway_id,
        accounts: vec![
            AccountMeta::new_readonly(gateway_config_pda, false),
            AccountMeta::new(verification_session_pda, false),
            AccountMeta::new_readonly(verifier_set_tracker_pda, false),
        ],
        data: ix_data.data(),
    };

    // SetComputeUnitLimit: program_id = ComputeBudget111..., data = [0x02, limit_u32_le]
    let cu_limit: u32 = 400_000;
    let cu_ix = Instruction {
        program_id: "ComputeBudget111111111111111111111111111111"
            .parse()
            .unwrap(),
        accounts: vec![],
        data: [&[0x02], cu_limit.to_le_bytes().as_slice()].concat(),
    };

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix, ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    let sig = rpc.send_and_confirm_transaction_with_spinner(&tx)?;
    Ok(sig)
}

/// Step 7c: Approve a message on the Solana gateway after all signatures
/// have been verified. Creates the IncomingMessage PDA.
#[allow(dead_code)]
pub fn approve_message(
    rpc_url: &str,
    payer: &Keypair,
    merklized_message: MerklizedMessage,
    payload_merkle_root: [u8; 32],
    signing_verifier_set_merkle_root: [u8; 32],
) -> Result<Signature> {
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let gateway_id = solana_axelar_gateway::id();

    let gateway_config_pda = solana_axelar_gateway::GatewayConfig::find_pda().0;

    let command_id = merklized_message.leaf.message.command_id();
    let (incoming_message_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::IncomingMessage::SEED_PREFIX,
            command_id.as_ref(),
        ],
        &gateway_id,
    );

    let payload_type_byte: u8 = PayloadType::ApproveMessages.into();
    let (verification_session_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::SignatureVerificationSessionData::SEED_PREFIX,
            &payload_merkle_root,
            &[payload_type_byte],
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &gateway_id);

    let ix_data = solana_axelar_gateway::instruction::ApproveMessage {
        merklized_message,
        payload_merkle_root,
    };

    let ix = Instruction {
        program_id: gateway_id,
        accounts: vec![
            AccountMeta::new_readonly(gateway_config_pda, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(verification_session_pda, false),
            AccountMeta::new(incoming_message_pda, false),
            AccountMeta::new_readonly(anchor_lang::prelude::system_program::ID, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(gateway_id, false),
        ],
        data: ix_data.data(),
    };

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    let sig = rpc.send_and_confirm_transaction_with_spinner(&tx)?;
    Ok(sig)
}

/// Run the full Solana gateway approval flow:
/// 1. Initialize verification session
/// 2. Verify all signatures
/// 3. Approve the message
#[allow(dead_code)]
pub fn approve_messages_on_gateway(
    rpc_url: &str,
    payer: &Keypair,
    execute_data: &ExecuteData,
) -> Result<()> {
    use crate::ui;
    use solana_axelar_std::execute_data::MerklizedPayload;

    // Step 7a: Initialize verification session
    ui::info("initializing payload verification session...");
    match initialize_verification_session(
        rpc_url,
        payer,
        execute_data.payload_merkle_root,
        execute_data.signing_verifier_set_merkle_root,
    ) {
        Ok(sig) => ui::tx_hash("init session", &sig.to_string()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("already in use") {
                ui::info("verification session already initialized");
            } else {
                return Err(e);
            }
        }
    }

    // Step 7b: Verify signatures (one per signer)
    let num_signers = execute_data.signing_verifier_set_leaves.len();
    ui::info(&format!("verifying {num_signers} signatures..."));

    for (i, verifier_info) in execute_data.signing_verifier_set_leaves.iter().enumerate() {
        // Normalize recovery ID: Cosmos signers produce 27/28 (Ethereum-style),
        // but Solana's secp256k1_recover expects 0/1.
        let mut info = verifier_info.clone();
        if info.signature.0[64] >= 27 {
            info.signature.0[64] -= 27;
        }
        match verify_signature(
            rpc_url,
            payer,
            execute_data.payload_merkle_root,
            execute_data.signing_verifier_set_merkle_root,
            info,
        ) {
            Ok(_) => {
                ui::info(&format!("  signature {}/{num_signers} verified", i + 1));
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("SlotAlreadyVerified") || msg.contains("already") {
                    ui::info(&format!(
                        "  signature {}/{num_signers} already verified",
                        i + 1
                    ));
                } else {
                    return Err(eyre::eyre!("failed to verify signature {}: {e}", i + 1));
                }
            }
        }
    }

    // Step 7c: Approve messages
    let messages = match &execute_data.payload_items {
        MerklizedPayload::NewMessages { messages } => messages,
        _ => {
            return Err(eyre::eyre!(
                "expected NewMessages payload, got VerifierSetRotation"
            ));
        }
    };

    ui::info(&format!("approving {} message(s)...", messages.len()));
    for (i, merklized_message) in messages.iter().enumerate() {
        match approve_message(
            rpc_url,
            payer,
            merklized_message.clone(),
            execute_data.payload_merkle_root,
            execute_data.signing_verifier_set_merkle_root,
        ) {
            Ok(sig) => {
                ui::tx_hash(&format!("approve msg {}", i + 1), &sig.to_string());
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("already in use") {
                    ui::info(&format!("  message {} already approved", i + 1));
                } else {
                    return Err(eyre::eyre!("failed to approve message {}: {e}", i + 1));
                }
            }
        }
    }

    ui::success("all messages approved on Solana gateway");
    Ok(())
}

/// Execute an approved GMP message on the Memo program.
/// Calls the Memo program's `execute` instruction which validates the message
/// against the gateway and logs the memo payload.
#[allow(dead_code)]
pub fn execute_on_memo(
    rpc_url: &str,
    payer: &Keypair,
    message: solana_axelar_std::Message,
    payload: &[u8],
) -> Result<Signature> {
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let gateway_id = solana_axelar_gateway::id();
    let memo_id = solana_axelar_memo::id();

    let command_id = message.command_id();

    // AxelarExecuteAccounts
    let (incoming_message_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::IncomingMessage::SEED_PREFIX,
            command_id.as_ref(),
        ],
        &gateway_id,
    );
    let (signing_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::ValidateMessageSigner::SEED_PREFIX,
            command_id.as_ref(),
        ],
        &memo_id,
    );
    let gateway_config_pda = solana_axelar_gateway::GatewayConfig::find_pda().0;
    let (gateway_event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &gateway_id);

    // Counter PDA for the memo program
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_id);

    // The payload passed to call_contract was AxelarMessagePayload-encoded (ABI scheme).
    // The execute instruction receives the inner payload (memo bytes) and the encoding scheme.
    // We need to decode the AxelarMessagePayload to get the inner payload.
    let decoded = solana_axelar_gateway::payload::AxelarMessagePayload::decode(payload)
        .map_err(|e| eyre::eyre!("failed to decode payload: {e:?}"))?;
    let inner_payload = decoded.payload_without_accounts().to_vec();
    let encoding_scheme = decoded.encoding_scheme();

    let ix_data = solana_axelar_memo::instruction::Execute {
        message,
        payload: inner_payload,
        encoding_scheme,
    }
    .data();

    let ix = Instruction {
        program_id: memo_id,
        accounts: vec![
            // AxelarExecuteAccounts (order from executable_accounts! macro)
            AccountMeta::new(incoming_message_pda, false),
            AccountMeta::new_readonly(signing_pda, false),
            AccountMeta::new_readonly(gateway_config_pda, false),
            AccountMeta::new_readonly(gateway_event_authority, false),
            AccountMeta::new_readonly(gateway_id, false),
            // Memo program accounts
            AccountMeta::new(counter_pda, false),
        ],
        data: ix_data,
    };

    let cu_limit: u32 = 400_000;
    let cu_ix = Instruction {
        program_id: "ComputeBudget111111111111111111111111111111"
            .parse()
            .unwrap(),
        accounts: vec![],
        data: [&[0x02], cu_limit.to_le_bytes().as_slice()].concat(),
    };

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix, ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    let sig = rpc.send_and_confirm_transaction_with_spinner(&tx)?;
    Ok(sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_testnet_gateway_pdas() {
        // Testnet Solana gateway
        let gateway_id: Pubkey = "gtwJ8LWDRWZpbvCqp8sDeTgy3GSyuoEsiaKC8wSXJqq"
            .parse()
            .unwrap();

        // GatewayConfig PDA
        let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
        println!("GatewayConfig PDA: {config_pda}");
        assert_eq!(
            config_pda.to_string(),
            "8mnEaWDXqbpDwyiGLR1T8DTc8AHuk2Fs6Pf4fRDv97WY"
        );

        // VerifierSetTracker PDA for the on-chain verifier set
        let onchain_hash =
            hex::decode("7b8163c3123a65f351c1d5b1e94c44841e731ea57b51f55479207380cab933c5")
                .unwrap();
        let (tracker_pda, _) =
            Pubkey::find_program_address(&[b"ver-set-tracker", &onchain_hash], &gateway_id);
        println!("VerifierSetTracker PDA (on-chain):  {tracker_pda}");
        assert_eq!(
            tracker_pda.to_string(),
            "F1PVLJQSGxBr28QWsRJTaTJiua7yKZQ5r97KG154uZum"
        );

        // VerifierSetTracker PDA for the MultisigProver's current set
        let prover_hash =
            hex::decode("046c15e70bf840b19ef2e727bbfe6fae18155077342b2aa41d766a2f6db32cb1")
                .unwrap();
        let (tracker_pda2, _) =
            Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
        println!("VerifierSetTracker PDA (prover):    {tracker_pda2}");

        // These should be DIFFERENT — confirming the mismatch
        assert_ne!(tracker_pda, tracker_pda2);
        println!("\nVerifier set mismatch confirmed!");
        println!("Gateway knows:      7b8163c3...");
        println!("MultisigProver uses: 046c15e7...");
        println!("rotate_signers needed on the Solana gateway");
    }
}

#[test]
fn derive_devnet_gateway_pdas() {
    let gateway_id: Pubkey = "gtwT4uGVTYSPnTGv6rSpMheyFyczUicxVWKqdtxNGw9"
        .parse()
        .unwrap();

    let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
    println!("=== DEVNET-AMPLIFIER ===");
    println!("GatewayConfig PDA: {config_pda}");

    // MultisigProver verifier set: caa238976160fcea5d5e5f4f3ea2ce0bed9106847e2d6d939de746c890c1faed
    let prover_hash =
        hex::decode("caa238976160fcea5d5e5f4f3ea2ce0bed9106847e2d6d939de746c890c1faed").unwrap();
    let (tracker_pda, _) =
        Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
    println!("VerifierSetTracker PDA (prover set): {tracker_pda}");
    println!("Check on-chain: solana account {tracker_pda} --url https://api.devnet.solana.com");
}

#[test]
fn derive_stagenet_gateway_pdas() {
    let gateway_id: Pubkey = "gtwYHfHHipAoj8Hfp3cGr3vhZ8f3UtptGCQLqjBkaSZ"
        .parse()
        .unwrap();

    let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
    println!("=== STAGENET ===");
    println!("GatewayConfig PDA: {config_pda}");

    // MultisigProver verifier set: 315ad3ca3e873b65dbc5dd4a446a62018ea368b5d9f29232fa090875fdaa50b8
    let prover_hash =
        hex::decode("315ad3ca3e873b65dbc5dd4a446a62018ea368b5d9f29232fa090875fdaa50b8").unwrap();
    let (tracker_pda, _) =
        Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
    println!("VerifierSetTracker PDA (prover set): {tracker_pda}");
    println!("Check on-chain: solana account {tracker_pda} --url https://api.testnet.solana.com");
}
