use alloy::consensus::Transaction as _;
use alloy::eips::Encodable2718;
use alloy::primitives::{U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use eyre::Result;

use super::session::{self, pause};
use super::types::Transaction;
use crate::evm::EvmEndpoints;
use crate::ui;

/// All EVM deployment writes pass here, including individual ITS sub-deployments.
pub async fn send<P: Provider>(
    provider: &P,
    mut request: TransactionRequest,
    label: &str,
) -> Result<TransactionReceipt> {
    session::guard_send()?;

    let session = session::current()?;
    let signer: PrivateKeySigner = session::evm_key()?.parse()?;
    request.from = Some(signer.address());
    request.chain_id = Some(session.plan.evm_chain_id);
    eyre::ensure!(
        provider.get_chain_id().await? == session.plan.evm_chain_id,
        "EVM RPC network changed"
    );
    let key = session::action_key(label)?;
    if let Some(limit) = session.journal.lock().await.retry_gas_limits.get(&key) {
        request.gas = Some(*limit);
    }
    let intent = super::evm_intent::hash(&request)?;
    if let Some(saved) = session.get(&key).await {
        return resume(provider, &saved, intent, &key).await;
    }
    if let Some(nonce) = request.nonce {
        eyre::ensure!(
            provider.get_transaction_count(signer.address()).await? == nonce,
            "{label}: pinned deployment nonce was consumed; reconcile before proceeding"
        );
    }
    provider.call(request.clone()).await.map_err(|error| {
        eyre::eyre!(
            "{label}: simulation failed: {}",
            crate::evm::decode_evm_error(&error)
        )
    })?;
    let endpoints = EvmEndpoints::connect(std::slice::from_ref(&session.rpc))?;
    let envelope = endpoints
        .fill_and_sign(&signer, request.clone(), &ui::warn)
        .await?;
    super::evm_retry::validate_retry(&key, &envelope).await?;
    let gas_cost = U256::from(envelope.gas_limit()) * U256::from(envelope.max_fee_per_gas());
    check_budget(signer.address(), gas_cost, None).await?;
    ui::section(label);
    ui::kv("network", &session.plan.evm_chain_id.to_string());
    ui::address("signer", &signer.address().to_string());
    ui::kv("nonce", &envelope.nonce().to_string());
    ui::kv(
        "maximum gas cost (native base units)",
        &gas_cost.to_string(),
    );
    preview_request(&request, signer.address(), envelope.nonce())?;
    if !ui::confirm("Send this transaction?").await {
        return Err(pause(
            "Transaction declined; it was not submitted. Run the continue command when you are ready to review and approve it.",
        ));
    }
    let saved = Transaction::Evm {
        intent,
        raw: envelope.encoded_2718().into(),
        hash: *envelope.tx_hash(),
        sender: signer.address(),
        nonce: envelope.nonce(),
        gas_cost: gas_cost.to_string(),
    };
    session.record(key.clone(), saved.clone()).await?;
    resume(provider, &saved, intent, &key).await
}

#[cfg(test)]
use super::evm_recovery::finalized;
pub(super) use super::evm_recovery::resume;

pub async fn check_budget(
    sender: alloy::primitives::Address,
    cost: U256,
    replacing: Option<&str>,
) -> Result<()> {
    let session = session::current()?;
    let journal = session.journal.lock().await;
    let candidate_nonce = replacing
        .and_then(|key| journal.actions.get(key))
        .and_then(|tx| {
            if let Transaction::Evm { nonce, .. } = tx {
                Some(*nonce)
            } else {
                None
            }
        });
    let spent =
        super::evm_funding::liability(&journal, sender, candidate_nonce.map(|n| (n, cost)))?;
    let total = if candidate_nonce.is_some() {
        spent
    } else {
        spent
            .checked_add(cost)
            .ok_or_else(|| eyre::eyre!("gas budget overflow"))?
    };
    eyre::ensure!(
        total <= session.plan.evm_gas_budget.parse::<U256>()?,
        "transaction exceeds approved EVM gas budget"
    );
    Ok(())
}

#[cfg(test)]
#[path = "evm_tests.rs"]
mod tests;

pub async fn reconcile(key: &str, saved: &Transaction, rpc: &str) -> Result<()> {
    if let Transaction::Evm { intent, .. } = saved {
        let provider = alloy::providers::ProviderBuilder::new().connect_http(rpc.parse()?);
        resume(&provider, saved, *intent, key).await?;
    }
    Ok(())
}

fn preview_request(
    request: &TransactionRequest,
    sender: alloy::primitives::Address,
    nonce: u64,
) -> Result<()> {
    let mut preview = serde_json::to_value(request)?;
    if let Some(input) = request.input.input()
        && input.len() > 1024
    {
        preview["input"] = serde_json::json!({"bytes":input.len(), "keccak256":keccak256(input)});
        if let Some(fields) = preview.as_object_mut() {
            fields.remove("data");
        }
    }
    if request.to.is_none_or(|to| to.is_create()) {
        ui::address("expected CREATE address", &sender.create(nonce).to_string());
    }
    ui::info(&serde_json::to_string_pretty(&preview)?);
    Ok(())
}
