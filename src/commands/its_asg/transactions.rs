use std::path::PathBuf;

use alloy::consensus::{Transaction, TxEnvelope};
use alloy::eips::{Decodable2718, Encodable2718};
use alloy::primitives::{Address, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use eyre::{Result, WrapErr, ensure};

use super::types::{Context, SavedTransaction};
use super::{checks, config, signing};
use crate::commands::deploy::hardened::storage;
use crate::evm::{EvmEndpoints, finality};
use crate::ui;

pub fn preview(request: &TransactionRequest) -> Result<()> {
    ui::info(&serde_json::to_string_pretty(request)?);
    Ok(())
}

pub(super) fn journal_path(request: &TransactionRequest) -> Result<PathBuf> {
    // A changed payload for the same sender/target must reconcile the old send.
    let identity = keccak256(serde_json::to_vec(&(
        request.chain_id,
        request.from,
        request.to,
    ))?);
    Ok(crate::state::data_dir()?
        .join("asg-transactions")
        .join(format!("{identity}.json")))
}

pub(super) fn sender_lock_path(chain_id: u64, from: Address) -> Result<PathBuf> {
    let identity = keccak256(serde_json::to_vec(&(chain_id, from))?);
    Ok(crate::state::data_dir()?
        .join("locks")
        .join(format!("asg-sender-{identity}.lock")))
}

pub async fn send<P: Provider>(
    provider: &P,
    ctx: &Context,
    request: &TransactionRequest,
    asg: Option<Address>,
) -> Result<TransactionReceipt> {
    config::unchanged(ctx)?;
    ensure!(
        provider.get_chain_id().await? == ctx.chain_id,
        "EVM RPC chain ID changed"
    );
    let from = request.from.ok_or_else(|| eyre::eyre!("missing sender"))?;
    // Nonces belong to an account, regardless of target or config checkout.
    // Hold this through recovery/finality so another ASG send cannot reuse one.
    let _sender_lock = storage::lock(&sender_lock_path(ctx.chain_id, from)?)?;
    let path = journal_path(request)?;
    let _lock = storage::lock(&path.with_extension("lock"))?;
    if path.exists() {
        let saved: SavedTransaction = serde_json::from_slice(&std::fs::read(&path)?)?;
        return resume(provider, ctx, request, &saved, asg).await;
    }
    let signer = signing::sender(from)?;
    provider
        .call(request.clone())
        .await
        .wrap_err("transaction simulation failed")?;
    let mut prepared = request.clone();
    prepared.nonce = Some(provider.get_transaction_count(from).pending().await?);
    prepared.gas = Some(
        provider
            .estimate_gas(prepared.clone())
            .await?
            .saturating_mul(12)
            / 10,
    );
    prepared.gas_price = Some(provider.get_gas_price().await?);
    prepared.transaction_type = Some(0);
    let max_cost = U256::from(prepared.gas.unwrap_or_default())
        * U256::from(prepared.gas_price.unwrap_or_default());
    ensure!(
        provider.get_balance(from).await? >= max_cost,
        "sender cannot cover maximum gas cost {max_cost}"
    );
    ui::kv(
        "maximum gas cost (native base units)",
        &max_cost.to_string(),
    );
    preview(&prepared)?;
    ensure!(
        ui::confirm("Send this exact transaction?").await,
        "declined, nothing sent"
    );
    config::unchanged(ctx)?;
    ensure!(
        provider.get_chain_id().await? == ctx.chain_id,
        "EVM RPC chain ID changed"
    );
    recheck_governance(provider, ctx, from, asg).await?;
    ensure!(
        Some(provider.get_transaction_count(from).pending().await?) == prepared.nonce,
        "sender nonce changed during review, rerun to prepare a new transaction"
    );
    provider
        .call(prepared.clone())
        .await
        .wrap_err("transaction no longer simulates after approval")?;
    let endpoints = EvmEndpoints::connect(std::slice::from_ref(&ctx.rpc))?;
    let envelope = endpoints
        .fill_and_sign(&signer, prepared.clone(), &ui::warn)
        .await?;
    let saved = SavedTransaction {
        request: prepared,
        raw: envelope.encoded_2718().into(),
    };
    validate(request, &saved)?;
    storage::atomic_write(&path, &serde_json::to_vec_pretty(&saved)?)?;
    broadcast(provider, &saved.raw).await
}

pub(super) fn validate(
    request: &TransactionRequest,
    saved: &SavedTransaction,
) -> Result<TxEnvelope> {
    let mut raw = saved.raw.as_ref();
    let tx = TxEnvelope::decode_2718(&mut raw)?;
    ensure!(
        raw.is_empty() && tx.is_legacy() && saved.request.transaction_type == Some(0),
        "saved transaction encoding or type differs from review"
    );
    ensure!(
        saved.request.from == request.from
            && saved.request.to == request.to
            && saved.request.input == request.input
            && saved.request.value == request.value
            && saved.request.chain_id == request.chain_id,
        "saved transaction intent mismatch"
    );
    ensure!(
        Some(
            tx.signature()
                .recover_address_from_prehash(&tx.signature_hash())?
        ) == request.from
            && Some(tx.kind()) == request.to
            && tx.input()
                == request
                    .input
                    .input()
                    .ok_or_else(|| eyre::eyre!("missing calldata"))?
            && tx.value() == request.value.unwrap_or_default()
            && tx.chain_id() == request.chain_id
            && Some(tx.nonce()) == saved.request.nonce
            && Some(tx.gas_limit()) == saved.request.gas
            && tx.gas_price() == saved.request.gas_price,
        "signed transaction does not match reviewed transaction"
    );
    Ok(tx)
}

pub(super) async fn resume<P: Provider>(
    provider: &P,
    ctx: &Context,
    request: &TransactionRequest,
    saved: &SavedTransaction,
    asg: Option<Address>,
) -> Result<TransactionReceipt> {
    let tx = validate(request, saved)?;
    if let Some(receipt) = provider.get_transaction_receipt(*tx.tx_hash()).await? {
        ensure!(
            receipt.transaction_hash == *tx.tx_hash(),
            "RPC returned a different receipt"
        );
        finality::wait_for_finalized_receipt(provider, &receipt).await?;
        return Ok(receipt);
    }
    let from = request.from.ok_or_else(|| eyre::eyre!("missing sender"))?;
    ensure!(
        provider.get_transaction_count(from).await? <= tx.nonce(),
        "saved nonce consumed, recover the original transaction receipt before continuing"
    );
    ui::warn("Resuming the recorded transaction with identical signed bytes");
    preview(&saved.request)?;
    ensure!(
        ui::confirm("Rebroadcast this recorded transaction?").await,
        "declined, nothing sent"
    );
    config::unchanged(ctx)?;
    ensure!(
        provider.get_chain_id().await? == ctx.chain_id,
        "EVM RPC chain ID changed"
    );
    recheck_governance(provider, ctx, from, asg).await?;
    broadcast(provider, &saved.raw).await
}

async fn recheck_governance<P: Provider>(
    provider: &P,
    ctx: &Context,
    from: Address,
    asg: Option<Address>,
) -> Result<()> {
    if let Some(asg) = asg {
        let artifact = checks::artifact(ctx)?;
        let runtime =
            checks::expected_runtime(provider, checks::creation(ctx, &artifact), from).await?;
        checks::governance(provider, ctx, asg, &runtime).await?;
    }
    Ok(())
}

async fn broadcast<P: Provider>(provider: &P, raw: &[u8]) -> Result<TransactionReceipt> {
    let hash = keccak256(raw);
    ui::tx_hash("transaction", &hash.to_string());
    let pending = provider.send_raw_transaction(raw).await.wrap_err(
        "broadcast unresolved, rerun the same command to recover the recorded transaction",
    )?;
    ensure!(
        *pending.tx_hash() == hash,
        "RPC returned a different transaction hash"
    );
    let receipt = pending
        .with_timeout(Some(std::time::Duration::from_secs(120)))
        .get_receipt()
        .await
        .wrap_err("receipt unresolved, rerun the same command to recover")?;
    ensure!(
        receipt.transaction_hash == hash,
        "RPC returned a different receipt"
    );
    ensure!(
        receipt.status(),
        "transaction reverted, inspect the recorded hash before retrying"
    );
    finality::wait_for_finalized_receipt(provider, &receipt).await?;
    Ok(receipt)
}
