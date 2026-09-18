use std::time::Duration;

use alloy::{eips::BlockNumberOrTag, providers::Provider, rpc::types::TransactionReceipt};
use eyre::{Result, WrapErr};

use crate::timing::{EVM_FINALITY_POLL_INTERVAL, EVM_FINALITY_TIMEOUT};
use crate::ui;

#[cfg(test)]
mod tests;

pub async fn wait_for_finalized_receipt<P: Provider>(
    provider: &P,
    receipt: &TransactionReceipt,
) -> Result<()> {
    wait_with_timeout(
        provider,
        receipt,
        EVM_FINALITY_TIMEOUT,
        EVM_FINALITY_POLL_INTERVAL,
    )
    .await
}

async fn wait_with_timeout<P: Provider>(
    provider: &P,
    receipt: &TransactionReceipt,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<()> {
    let target = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("source receipt has no block number"))?;
    let expected_hash = receipt
        .block_hash
        .ok_or_else(|| eyre::eyre!("source receipt has no block hash"))?;
    eyre::ensure!(receipt.status(), "source transaction reverted");
    let tx_hash = receipt.transaction_hash;
    let spinner = ui::wait_spinner(&format!(
        "waiting for source block {target} to be finalized..."
    ));
    let result = tokio::time::timeout(timeout, async {
        loop {
            let block = provider.get_block_by_number(BlockNumberOrTag::Finalized).await
                .wrap_err("cannot read source finality, verification has not been requested")?
                .ok_or_else(|| eyre::eyre!("RPC returned no finalized block, verification has not been requested"))?;
            let finalized = block.header.number;
            spinner.set_message(format!("source finality: finalized block {finalized}, need {target}"));
            if finalized >= target {
                let observed = provider.get_transaction_receipt(tx_hash).await?
                    .ok_or_else(|| eyre::eyre!("source transaction {tx_hash} disappeared before finality"))?;
                eyre::ensure!(observed.block_hash == Some(expected_hash) && observed.block_number == Some(target),
                    "source transaction {tx_hash} moved to a different block before finality, verification has not been requested");
                eyre::ensure!(observed.status(), "source transaction {tx_hash} reverted");
                return Ok::<(), eyre::Report>(());
            }
            tokio::time::sleep(poll_interval).await;
        }
    }).await;
    spinner.finish_and_clear();
    result.map_err(|_| eyre::eyre!(
        "source transaction {tx_hash} did not finalize within {}s (block {target}). Verification has not been requested",
        timeout.as_secs()
    ))??;
    ui::success(&format!("source transaction finalized at block {target}"));
    Ok(())
}
