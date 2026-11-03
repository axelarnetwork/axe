use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::providers::Provider;
use alloy::rpc::types::{Block, Transaction, TransactionReceipt};
use eyre::{Result, ensure};

use super::types::TransactionAction;
use crate::ui;

// Two confirmations include the block containing the transaction.
const EXTRA_BLOCKS: u64 = 1;

pub(super) async fn observation<P: Provider>(provider: &P) -> Result<Block<Transaction>> {
    let height = provider
        .get_block_number()
        .await?
        .checked_sub(EXTRA_BLOCKS)
        .ok_or_else(|| eyre::eyre!("chain has not reached two confirmations"))?;
    provider
        .get_block_by_number(BlockNumberOrTag::Number(height))
        .await?
        .ok_or_else(|| eyre::eyre!("confirmed block {height} unavailable"))
}

pub(super) async fn canonical<P: Provider>(provider: &P, block: &Block<Transaction>) -> Result<()> {
    let current = provider
        .get_block_by_number(BlockNumberOrTag::Number(block.header.number))
        .await?
        .ok_or_else(|| eyre::eyre!("confirmed block disappeared, rerun to verify again"))?;
    ensure!(
        current.header.hash == block.header.hash,
        "confirmed block changed, rerun to verify again"
    );
    Ok(())
}

pub(super) fn block_id(block: &Block<Transaction>) -> BlockId {
    BlockId::hash_canonical(block.header.hash)
}

pub(super) async fn wait<P: Provider>(
    provider: &P,
    receipt: &TransactionReceipt,
    action: TransactionAction,
) -> Result<()> {
    let label = match action {
        TransactionAction::Deploy(_) => "ASG deployment",
        TransactionAction::Handoff(_) => "ownership transfer",
    };
    wait_for_confirmations(
        provider,
        receipt,
        label,
        Duration::from_secs(120),
        Duration::from_secs(1),
    )
    .await
}

pub(super) async fn wait_for_confirmations<P: Provider>(
    provider: &P,
    receipt: &TransactionReceipt,
    label: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    ensure!(receipt.status(), "{label} transaction reverted");
    let inclusion = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("{label} receipt has no block number"))?;
    let expected_hash = receipt
        .block_hash
        .ok_or_else(|| eyre::eyre!("{label} receipt has no block hash"))?;
    let target = inclusion
        .checked_add(EXTRA_BLOCKS)
        .ok_or_else(|| eyre::eyre!("confirmation height overflow"))?;
    let spinner = ui::wait_spinner(&format!(
        "waiting for two {label} confirmations (including inclusion block)"
    ));
    let result = tokio::time::timeout(timeout, async {
        loop {
            let head = provider.get_block_number().await?;
            if head >= target {
                let observed = provider
                    .get_transaction_receipt(receipt.transaction_hash)
                    .await?
                    .ok_or_else(|| eyre::eyre!("{label} receipt disappeared, rerun to recover"))?;
                ensure!(
                    observed.transaction_hash == receipt.transaction_hash
                        && observed.block_number == Some(inclusion)
                        && observed.block_hash == Some(expected_hash)
                        && observed.status(),
                    "{label} receipt changed or reverted, rerun to recover"
                );
                let block = provider
                    .get_block_by_number(BlockNumberOrTag::Number(inclusion))
                    .await?
                    .ok_or_else(|| eyre::eyre!("{label} block disappeared, rerun to recover"))?;
                ensure!(
                    block.header.hash == expected_hash,
                    "{label} block reorganized, rerun to recover"
                );
                return Ok::<(), eyre::Report>(());
            }
            spinner.set_message(format!("{label} confirmations: block {head}/{target}"));
            tokio::time::sleep(poll).await;
        }
    })
    .await;
    spinner.finish_and_clear();
    result.map_err(|_| eyre::eyre!("{label} {} did not reach two confirmations within {}s, rerun the same command to recover", receipt.transaction_hash, timeout.as_secs()))??;
    ui::success(&format!("{label} has two confirmations (not finality)"));
    Ok(())
}
