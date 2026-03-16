use anchor_lang::InstructionData;
use eyre::Result;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature},
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
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

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
        let gas_event_authority = Pubkey::find_program_address(
            &[b"__event_authority"],
            &solana_axelar_gas_service::id(),
        )
        .0;

        let pay_gas_data = solana_axelar_gas_service::instruction::PayGas {
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            payload_hash,
            amount: 100_000,
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
    { 2 }
    #[cfg(feature = "devnet-amplifier")]
    { 1 }
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
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
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
        AccountMeta::new_readonly(Pubkey::from_str_const("11111111111111111111111111111111"), false),
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
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
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
        AccountMeta::new_readonly(Pubkey::from_str_const("11111111111111111111111111111111"), false),
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
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
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
        AccountMeta::new_readonly(Pubkey::from_str_const("11111111111111111111111111111111"), false),
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
fn fetch_confirmed_tx(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<Option<solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta>> {
    for i in 0..10 {
        match rpc_client.get_transaction(signature, UiTransactionEncoding::Json) {
            Ok(tx) => return Ok(Some(tx)),
            Err(_) => {
                // Testnet/stagenet RPCs can be slow to index transactions.
                // Use exponential backoff: 500ms, 1s, 2s, ...
                let delay = std::cmp::min(500 * (1 << i), 5000);
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
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
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

    // Each "Program X invoke [N]" log with N >= 2 corresponds to one entry
    // in the inner_instructions array (a CPI within the top-level instruction).
    // We count them sequentially and find the gateway call_contract CPI.
    //
    // The message ID uses the index of the gateway `call_contract` invocation
    // in the inner instructions array. After call_contract, the gateway emits
    // an event via `emit_cpi!` — this does NOT create a separate "invoke" log
    // but IS a separate entry in the inner_instructions array. So the message
    // ID index = gateway_invoke_index + 1 (for the event emit that follows).
    let mut inner_ix_counter: u32 = 0;
    let mut last_gateway_idx: Option<u32> = None;

    for log in &logs {
        if let Some(rest) = log.strip_prefix("Program ") {
            if rest.ends_with(" invoke [1]") {
                continue;
            }
            if rest.contains(" invoke [") {
                if rest.split(" invoke [").next() == Some(gateway_id.as_str()) {
                    last_gateway_idx = Some(inner_ix_counter);
                }
                inner_ix_counter += 1;
            }
        }
    }

    // The emit_cpi! after call_contract adds one more inner instruction that
    // doesn't produce an "invoke" log. The message ID references that emit
    // instruction, so we add 1.
    let last_gateway_idx = last_gateway_idx.map(|idx| idx + 1);

    match last_gateway_idx {
        Some(idx) => {
            // Top-level instruction is index 0 (0-based), displayed as 1 (1-based)
            // in the message ID format.
            Ok(format!("{signature_str}-1.{idx}"))
        }
        None => Err(eyre::eyre!(
            "could not find gateway call_contract CPI in transaction logs"
        )),
    }
}
