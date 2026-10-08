use alloy::eips::BlockNumberOrTag;
use alloy::providers::Provider;
use alloy::rpc::types::{Block, Transaction};
use eyre::Result;

use super::{
    session, storage,
    types::{EvmConfirmationPolicy, Journal, Options},
};
use crate::{state::State, ui};

pub async fn resolve(state: &State, options: &Options) -> Result<EvmConfirmationPolicy> {
    if let Some(policy) = options.evm_confirmations {
        return Ok(policy);
    }
    let path = storage::directory(state)?.join("journal.json");
    if path.exists() {
        let journal: Journal = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        Ok(journal.evm_confirmations)
    } else {
        Ok(EvmConfirmationPolicy::default())
    }
}

pub async fn persist(session: &session::Session, policy: EvmConfirmationPolicy) -> Result<()> {
    let mut journal = session.journal.lock().await;
    journal.evm_confirmations = policy;
    storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)?;
    ui::kv("EVM confirmation policy", &policy.to_string());
    if let EvmConfirmationPolicy::Confirmations(count) = policy {
        ui::info(&format!(
            "Wait for {count} confirmation(s), counting the inclusion block. This does not establish finality; resume rechecks recorded receipt block hashes."
        ));
    }
    Ok(())
}

pub async fn current() -> Result<EvmConfirmationPolicy> {
    Ok(session::current()?.journal.lock().await.evm_confirmations)
}

pub(super) fn required_height(policy: EvmConfirmationPolicy, inclusion: u64) -> Result<u64> {
    let extra = match policy {
        EvmConfirmationPolicy::Confirmations(count) => count.get() - 1,
        EvmConfirmationPolicy::Finalized => 0,
    };
    inclusion
        .checked_add(extra)
        .ok_or_else(|| eyre::eyre!("confirmation height overflow"))
}

/// Read gateway state from a block that meets the same policy as its transactions.
pub async fn observation_block<P: Provider>(provider: &P) -> Result<Block<Transaction>> {
    let policy = current().await?;
    let head = provider
        .get_block_by_number(policy.block_tag())
        .await?
        .ok_or_else(|| eyre::eyre!("EVM confirmation block unavailable ({policy})"))?;
    let height = match policy {
        EvmConfirmationPolicy::Confirmations(count) => head
            .header
            .number
            .checked_sub(count.get() - 1)
            .ok_or_else(|| eyre::eyre!("chain has not reached the requested confirmation depth"))?,
        EvmConfirmationPolicy::Finalized => head.header.number,
    };
    if height == head.header.number {
        return Ok(head);
    }
    provider
        .get_block_by_number(BlockNumberOrTag::Number(height))
        .await?
        .ok_or_else(|| eyre::eyre!("EVM confirmation block {height} unavailable"))
}
