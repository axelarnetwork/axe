//! Solana ITS instruction builders for `deploy_interchain_token`,
//! `deploy_remote_interchain_token`, and `interchain_transfer`. Each
//! function constructs the full account list, signs with the supplied
//! keypair, and submits via `send_and_confirm_transaction` (finalized
//! commitment).

use std::time::Instant;

use anchor_lang::InstructionData;
use eyre::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signer::Signer,
    transaction::Transaction,
};

use super::encoding::{
    find_interchain_token_pda, find_its_root_pda, find_token_manager_pda,
    get_associated_token_address, interchain_token_id, mpl_token_metadata_program_id,
    spl_associated_token_account_program_id,
};
use super::rpc::{fetch_tx_details, rpc_client};
use crate::commands::load_test::metrics::TxMetrics;

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
    let rpc_client = rpc_client(rpc_url);
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
    let rpc_client = rpc_client(rpc_url);
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
    let rpc_client = rpc_client(rpc_url);
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
