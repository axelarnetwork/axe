use super::{
    evm_recovery, session,
    types::{EvmConfirmationPolicy, Transaction},
};
use crate::commands::deploy::DeployContext;
use alloy::{
    consensus::{Transaction as _, TxEnvelope},
    eips::Decodable2718,
    providers::{Provider, ProviderBuilder},
};
use eyre::Result;
use std::time::Duration;
use tokio::time::Instant;

pub(super) async fn prove(ctx: &DeployContext, key: &str, saved: &Transaction) -> Result<String> {
    let provider = ProviderBuilder::new().connect_http(ctx.rpc_url.parse()?);
    let session = session::current()?;
    eyre::ensure!(
        provider.get_chain_id().await? == session.plan.evm_chain_id,
        "recovery RPC network mismatch"
    );
    prove_with_provider(&provider, key, saved).await
}

pub(super) async fn prove_with_provider<P: Provider>(
    provider: &P,
    key: &str,
    saved: &Transaction,
) -> Result<String> {
    validate_action(key, saved)?;
    let session = session::current()?;
    let attempts = evm_recovery::competing_attempts(key, saved).await?;
    for attempt in &attempts {
        validate_action(key, attempt)?;
    }
    if let Transaction::Evm {
        raw, sender, nonce, ..
    } = saved
        && TxEnvelope::decode_2718(&mut raw.as_ref())?.to().is_none()
    {
        let journal = session.journal.lock().await;
        eyre::ensure!(
            !journal.actions.values().chain(journal.attempts.values().flatten()).any(|tx| {
                matches!(tx, Transaction::Evm { sender: from, nonce: later, .. } if from == sender && later > nonce)
            }),
            "a later transaction is already signed by this CREATE deployer; reconcile dependent addresses before retrying"
        );
    }
    let receipt = evm_recovery::included(provider, &attempts)
        .await?
        .ok_or_else(|| {
            eyre::eyre!("no receipt proving failure; unknown transactions cannot be retired")
        })?;
    eyre::ensure!(
        !receipt.status(),
        "successful transactions cannot be retried"
    );
    let deadline = Instant::now() + Duration::from_secs(session.options.evm_wait_seconds);
    let receipt = evm_recovery::wait_receipt(
        provider,
        receipt,
        key,
        deadline,
        EvmConfirmationPolicy::Finalized,
    )
    .await?;
    eyre::ensure!(
        !receipt.status(),
        "transaction became successful while waiting; cannot retire it"
    );
    Ok(receipt.transaction_hash.to_string())
}

pub(super) fn validate_action(key: &str, saved: &Transaction) -> Result<()> {
    eyre::ensure!(
        !key.starts_with("AxelarGateway/"),
        "gateway CREATE failure consumed a nonce pinned to the address registered on Cosmos. A retry would deploy at a different address. Preserve the journal; reconcile/update the Cosmos gateway registration through a separately reviewed recovery before deploying a replacement. No retry was authorized"
    );
    let Transaction::Evm {
        raw,
        hash,
        sender,
        nonce,
        ..
    } = saved
    else {
        eyre::bail!("expected EVM action");
    };
    let tx = TxEnvelope::decode_2718(&mut raw.as_ref())?;
    eyre::ensure!(
        tx.tx_hash() == hash
            && tx.nonce() == *nonce
            && tx
                .signature()
                .recover_address_from_prehash(&tx.signature_hash())?
                == *sender,
        "journal signed EVM transaction identity mismatch"
    );
    if tx.to().is_none() {
        eyre::ensure!(
            matches!(
                key,
                "EvmCompatibilityCheck/deploy compatibility probe"
                    | "ConstAddressDeployer/ConstAddressDeployer"
                    | "AxelarGasService/gas implementation"
                    | "AxelarGasService/gas proxy"
            ),
            "CREATE action {key} has no safe address-preserving recovery policy"
        );
    }
    Ok(())
}

pub(super) async fn validate_retry(key: &str, new: &TxEnvelope) -> Result<()> {
    let session = session::current()?;
    let journal = session.journal.lock().await;
    if let Some(Transaction::Evm { raw, sender, .. }) = journal
        .attempts
        .get(key)
        .and_then(|attempts| attempts.last())
    {
        let old = TxEnvelope::decode_2718(&mut raw.as_ref())?;
        validate_retry_fields(&old, new)?;
        eyre::ensure!(
            new.signature()
                .recover_address_from_prehash(&new.signature_hash())?
                == *sender,
            "retry signer differs from retired attempt"
        );
    }
    Ok(())
}

pub(super) fn validate_retry_fields(old: &TxEnvelope, new: &TxEnvelope) -> Result<()> {
    eyre::ensure!(
        new.nonce() > old.nonce()
            && old.chain_id() == new.chain_id()
            && old.kind() == new.kind()
            && old.value() == new.value()
            && old.input() == new.input()
            && old.access_list() == new.access_list()
            && old.authorization_list() == new.authorization_list()
            && old.blob_versioned_hashes() == new.blob_versioned_hashes(),
        "retry must advance the nonce and preserve chain, destination, value and calldata; only fees and gas limit may change"
    );
    Ok(())
}
