use std::path::Path;

use alloy::primitives::keccak256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use eyre::{Result, ensure};
use serde_json::json;

use crate::commands::deploy::hardened::storage;
use crate::ui;

/// Call only after a chain-specific terminality check. Never retire on timeout alone.
pub(super) async fn archive(path: &Path, evidence: &str) -> Result<()> {
    let bytes = std::fs::read(path)?;
    ui::kv("recorded action", &path.display().to_string());
    ui::kv("terminal evidence", evidence);
    ensure!(
        ui::confirm("Archive this non-executable record and re-evaluate the remaining work?").await,
        "declined; record preserved"
    );
    persist(path, &bytes, evidence)
}

fn persist(path: &Path, bytes: &[u8], evidence: &str) -> Result<()> {
    let digest = keccak256(bytes);
    let archive = path.with_extension(format!("{digest}.retired"));
    ensure!(
        std::fs::read(path)? == bytes,
        "journal changed during retirement review"
    );
    storage::atomic_write(&archive, bytes)?;
    storage::atomic_write(
        &archive.with_extension("evidence"),
        &serde_json::to_vec_pretty(&json!({"journalHash":digest,"reason":evidence}))?,
    )?;
    std::fs::remove_file(path)?;
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| eyre::eyre!("journal parent missing"))?,
    )?
    .sync_all()?;
    Ok(())
}

pub(super) async fn evm_terminal<P: Provider>(
    provider: &P,
    request: &TransactionRequest,
    raw: &[u8],
    retire_success: bool,
) -> Result<Option<String>> {
    let hash = keccak256(raw);
    let mut receipt = provider.get_transaction_receipt(hash).await?;
    if receipt.is_none() {
        let from = request
            .from
            .ok_or_else(|| eyre::eyre!("recorded sender missing"))?;
        let nonce = request
            .nonce
            .ok_or_else(|| eyre::eyre!("recorded nonce missing"))?;
        let head = provider.get_block_number().await?.saturating_sub(1);
        if provider
            .get_transaction_count(from)
            .block_id(alloy::eips::BlockId::number(head))
            .await?
            <= nonce
        {
            return Ok(None);
        }
        for _ in 0..3 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            receipt = provider.get_transaction_receipt(hash).await?;
            if receipt.is_some() {
                break;
            }
        }
    }
    let terminal = if let Some(receipt) = receipt {
        if receipt.status() && !retire_success {
            return Ok(None);
        }
        let block = receipt
            .block_number
            .ok_or_else(|| eyre::eyre!("terminal receipt has no block"))?;
        let header = provider
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(block))
            .await?
            .ok_or_else(|| eyre::eyre!("receipt block unavailable"))?;
        ensure!(
            receipt.transaction_hash == hash
                && Some(header.header.hash) == receipt.block_hash
                && provider.get_block_number().await? >= block.saturating_add(1),
            "terminal receipt is not canonically confirmed"
        );
        let outcome = if receipt.status() {
            "succeeded"
        } else {
            "reverted"
        };
        format!("{hash} {outcome} in canonical block {block}")
    } else {
        let from = request
            .from
            .ok_or_else(|| eyre::eyre!("recorded sender missing"))?;
        let nonce = request
            .nonce
            .ok_or_else(|| eyre::eyre!("recorded nonce missing"))?;
        let head = provider.get_block_number().await?.saturating_sub(1);
        if provider
            .get_transaction_count(from)
            .block_id(alloy::eips::BlockId::number(head))
            .await?
            <= nonce
        {
            return Ok(None);
        }
        format!(
            "{hash} unavailable, but nonce {nonce} is consumed at confirmed block {head}; signed bytes cannot execute again. Live effects will be re-read"
        )
    };
    Ok(Some(terminal))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_preserves_bytes_evidence_and_rejects_changed_reviewed_record() {
        let root = std::env::temp_dir().join(format!("asg-retire-test-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("transaction.json");
        std::fs::write(&path, b"signed public bytes").unwrap();
        assert!(persist(&path, b"different reviewed bytes", "evidence").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"signed public bytes");
        persist(&path, b"signed public bytes", "canonical revert").unwrap();
        assert!(!path.exists());
        let archive = path.with_extension(format!("{}.retired", keccak256(b"signed public bytes")));
        assert_eq!(std::fs::read(&archive).unwrap(), b"signed public bytes");
        let evidence: serde_json::Value =
            serde_json::from_slice(&std::fs::read(archive.with_extension("evidence")).unwrap())
                .unwrap();
        assert_eq!(evidence["reason"], "canonical revert");
        std::fs::remove_dir_all(root).unwrap();
    }
}
