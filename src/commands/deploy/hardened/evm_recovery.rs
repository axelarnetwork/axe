use std::time::Duration;

use alloy::consensus::{Transaction as _, TxEnvelope, Typed2718};
use alloy::eips::{BlockNumberOrTag, Decodable2718, Encodable2718};
use alloy::primitives::{B256, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use eyre::Result;
use tokio::time::Instant;

use super::{
    journal, session,
    types::{Confirmation, EvmConfirmationPolicy, Transaction},
};
use crate::{evm::EvmEndpoints, ui};

pub async fn resume<P: Provider>(
    provider: &P,
    saved: &Transaction,
    intent: B256,
    key: &str,
) -> Result<TransactionReceipt> {
    let Transaction::Evm {
        intent: expected,
        raw,
        hash,
        sender,
        nonce,
        ..
    } = saved
    else {
        eyre::bail!("journal transaction type mismatch");
    };
    eyre::ensure!(
        *expected == intent && keccak256(raw) == *hash,
        "{key}: journal intent or signed bytes mismatch"
    );
    let mut attempts = competing_attempts(key, saved).await?;
    let wait = if session::active() {
        session::current()?.options.evm_wait_seconds
    } else {
        0
    };
    let deadline = Instant::now() + Duration::from_secs(wait);
    let policy = if session::active() {
        super::confirmations::current().await?
    } else {
        EvmConfirmationPolicy::Finalized
    };
    if let Some(receipt) = included(provider, &attempts).await? {
        return wait_confirmed(provider, receipt, key, deadline, policy).await;
    }
    let current = provider.get_transaction_count(*sender).await?;
    eyre::ensure!(
        current <= *nonce,
        "{key}: nonce consumed but recorded transaction unavailable; recover its receipt using a same-network archival RPC before continuing"
    );
    if session::active() && session::current()?.options.bump_fees.as_deref() == Some(key) {
        attempts.push(bump(provider, key, saved).await?);
    }
    let latest = attempts
        .last()
        .ok_or_else(|| eyre::eyre!("missing EVM attempt"))?;
    broadcast(provider, latest, key).await?;
    loop {
        if let Some(receipt) = included(provider, &attempts).await? {
            return wait_confirmed(provider, receipt, key, deadline, policy).await;
        }
        if Instant::now() >= deadline {
            return Err(session::pause(format!(
                "{key}: transaction {hash} still pending. Resume, or explicitly request --bump-fees '{key}' to review a fee-only replacement"
            )));
        }
        ui::info(&format!("{key}: waiting for inclusion; Ctrl+C is safe"));
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub(super) async fn included<P: Provider>(
    provider: &P,
    attempts: &[Transaction],
) -> Result<Option<TransactionReceipt>> {
    for attempt in attempts {
        let Transaction::Evm { hash, .. } = attempt else {
            eyre::bail!("invalid EVM attempt");
        };
        if let Some(Confirmation::Evm { receipt }) =
            journal::confirmation(&hash.to_string()).await?
        {
            eyre::ensure!(
                receipt.transaction_hash == *hash,
                "cached receipt hash mismatch"
            );
            if canonical(provider, &receipt).await? {
                return Ok(Some(*receipt));
            }
            ui::info(&format!(
                "{hash}: cached receipt was reorganized; resolving the original transaction hash again"
            ));
        }
        if let Some(receipt) = provider.get_transaction_receipt(*hash).await? {
            eyre::ensure!(receipt.transaction_hash == *hash, "receipt hash mismatch");
            return Ok(Some(receipt));
        }
    }
    Ok(None)
}

async fn broadcast<P: Provider>(provider: &P, saved: &Transaction, key: &str) -> Result<()> {
    let Transaction::Evm { raw, hash, .. } = saved else {
        eyre::bail!("expected EVM transaction");
    };
    if provider.get_transaction_by_hash(*hash).await?.is_none() {
        let pending = provider.send_raw_transaction(raw).await.map_err(|error| session::pause(format!(
            "{key}: broadcast unresolved ({error}); resume to reconcile, or use --bump-fees '{key}' if fees are insufficient"
        )))?;
        eyre::ensure!(
            pending.tx_hash() == hash,
            "RPC returned a different transaction hash"
        );
    }
    ui::tx_hash(key, &hash.to_string());
    Ok(())
}

#[cfg(test)]
pub(super) async fn finalized<P: Provider>(
    provider: &P,
    receipt: TransactionReceipt,
    key: &str,
) -> Result<TransactionReceipt> {
    wait_confirmed(
        provider,
        receipt,
        key,
        Instant::now(),
        EvmConfirmationPolicy::Finalized,
    )
    .await
}

pub(super) async fn wait_receipt<P: Provider>(
    provider: &P,
    mut receipt: TransactionReceipt,
    key: &str,
    deadline: Instant,
    policy: EvmConfirmationPolicy,
) -> Result<TransactionReceipt> {
    let hash = receipt.transaction_hash;
    loop {
        let block = receipt
            .block_number
            .ok_or_else(|| eyre::eyre!("receipt has no block number"))?;
        let target = super::confirmations::required_height(policy, block)?;
        let head = provider
            .get_block_by_number(policy.block_tag())
            .await?
            .ok_or_else(|| eyre::eyre!("RPC does not expose confirmation blocks ({policy})"))?;
        if head.header.number >= target {
            eyre::ensure!(
                canonical(provider, &receipt).await?,
                "receipt is no longer canonical; resume to resolve its transaction again"
            );
            journal::confirm(
                receipt.transaction_hash.to_string(),
                Confirmation::Evm {
                    receipt: Box::new(receipt.clone()),
                },
            )
            .await?;
            return Ok(receipt);
        }
        if Instant::now() >= deadline {
            return Err(session::pause(format!(
                "{key}: included at {block}, confirmation policy {policy}, observed height {}, required {target}; resume to continue",
                head.header.number
            )));
        }
        ui::info(&format!(
            "{key}: included at {block}, confirmation policy {policy}, observed height {}, required {target}; waiting (Ctrl+C is safe)",
            head.header.number
        ));
        tokio::time::sleep(Duration::from_secs(10)).await;
        receipt = provider
            .get_transaction_receipt(receipt.transaction_hash)
            .await?
            .ok_or_else(|| {
                session::pause(
                    "receipt disappeared before the confirmation target; resume to reconcile",
                )
            })?;
        eyre::ensure!(receipt.transaction_hash == hash, "receipt hash mismatch");
    }
}

async fn canonical<P: Provider>(provider: &P, receipt: &TransactionReceipt) -> Result<bool> {
    let number = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("receipt has no block number"))?;
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .ok_or_else(|| {
            eyre::eyre!("receipt block unavailable; cannot establish canonical inclusion")
        })?;
    Ok(Some(block.header.hash) == receipt.block_hash)
}

async fn bump<P: Provider>(provider: &P, key: &str, saved: &Transaction) -> Result<Transaction> {
    let Transaction::Evm {
        raw,
        sender,
        nonce,
        intent,
        ..
    } = saved
    else {
        eyre::bail!("expected EVM transaction");
    };
    let old = TxEnvelope::decode_2718(&mut raw.as_ref())?;
    let signer: PrivateKeySigner = session::evm_key()?.parse()?;
    eyre::ensure!(
        signer.address() == *sender
            && old
                .signature()
                .recover_address_from_prehash(&old.signature_hash())?
                == *sender
            && old.nonce() == *nonce,
        "replacement signer/nonce mismatch"
    );
    let mut request = TransactionRequest::from_transaction_with_sender(old.clone(), *sender);
    let fees = provider.get_gas_price().await?;
    let doubled = old
        .max_fee_per_gas()
        .checked_mul(2)
        .ok_or_else(|| eyre::eyre!("fee overflow"))?;
    if old.gas_price().is_some() {
        request.gas_price = Some(doubled.max(fees));
    } else {
        request.max_fee_per_gas = Some(
            doubled.max(
                fees.checked_mul(2)
                    .ok_or_else(|| eyre::eyre!("fee overflow"))?,
            ),
        );
        request.max_priority_fee_per_gas = Some(
            old.max_priority_fee_per_gas()
                .unwrap_or_default()
                .checked_mul(2)
                .ok_or_else(|| eyre::eyre!("fee overflow"))?
                .max(1),
        );
    }
    let session = session::current()?;
    let envelope = EvmEndpoints::connect(std::slice::from_ref(&session.rpc))?
        .fill_and_sign(&signer, request, &ui::warn)
        .await?;
    validate_replacement(&old, &envelope)?;
    let cost = U256::from(envelope.gas_limit()) * U256::from(envelope.max_fee_per_gas());
    super::evm::check_budget(*sender, cost, Some(key)).await?;
    eyre::ensure!(
        provider.get_balance(*sender).await? >= cost + old.value(),
        "replacement signer lacks funds for gas and value"
    );
    ui::kv("replacement action", key);
    ui::kv("same nonce", &nonce.to_string());
    ui::tx_hash("original", &old.tx_hash().to_string());
    ui::kv("new maximum gas cost", &cost.to_string());
    if !ui::confirm("Approve this fee-only replacement?").await {
        return Err(session::pause("replacement declined"));
    }
    let replacement = Transaction::Evm {
        intent: *intent,
        raw: envelope.encoded_2718().into(),
        hash: *envelope.tx_hash(),
        sender: *sender,
        nonce: *nonce,
        gas_cost: cost.to_string(),
    };
    journal::replace(key, replacement.clone()).await?;
    Ok(replacement)
}

pub(super) fn validate_replacement(old: &TxEnvelope, new: &TxEnvelope) -> Result<()> {
    let a = TransactionRequest::from_transaction_with_sender(
        old.clone(),
        old.signature()
            .recover_address_from_prehash(&old.signature_hash())?,
    );
    let b = TransactionRequest::from_transaction_with_sender(
        new.clone(),
        new.signature()
            .recover_address_from_prehash(&new.signature_hash())?,
    );
    eyre::ensure!(
        super::evm_intent::hash(&a)? == super::evm_intent::hash(&b)? && old.ty() == new.ty(),
        "replacement changed execution fields"
    );
    eyre::ensure!(
        new.max_fee_per_gas() > old.max_fee_per_gas(),
        "replacement fee must increase"
    );
    Ok(())
}

pub(super) async fn competing_attempts(key: &str, saved: &Transaction) -> Result<Vec<Transaction>> {
    let Transaction::Evm { sender, nonce, .. } = saved else {
        eyre::bail!("expected EVM transaction");
    };
    Ok(journal::attempts(key, saved).await?.into_iter().filter(|attempt| {
        matches!(attempt, Transaction::Evm { sender: from, nonce: n, .. } if from == sender && n == nonce)
    }).collect())
}

pub(super) async fn wait_confirmed<P: Provider>(
    provider: &P,
    receipt: TransactionReceipt,
    key: &str,
    deadline: Instant,
    policy: EvmConfirmationPolicy,
) -> Result<TransactionReceipt> {
    let receipt = wait_receipt(provider, receipt, key, deadline, policy).await?;
    eyre::ensure!(
        receipt.status(),
        "{key}: recorded transaction reverted; use --retry-failed '{key}' to review recovery. Pinned gateway CREATE failures require address reconciliation and cannot be retried automatically"
    );
    Ok(receipt)
}
