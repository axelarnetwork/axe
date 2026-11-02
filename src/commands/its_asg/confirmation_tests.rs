use std::time::Duration;

use alloy::primitives::{Address, B256};
use alloy::rpc::types::{Block, Transaction, TransactionReceipt};
use serde_json::{Value, json};

use super::{confirmations, types::TransactionAction};
use crate::evm::{connect_evm, test_rpc::serve};

fn receipt() -> TransactionReceipt {
    serde_json::from_value(json!({
        "type":"0x0", "status":"0x1", "cumulativeGasUsed":"0x5208",
        "logsBloom":format!("0x{}", "00".repeat(256)), "logs":[],
        "transactionHash":B256::repeat_byte(1), "blockHash":B256::repeat_byte(2),
        "blockNumber":"0xa", "gasUsed":"0x5208", "effectiveGasPrice":"0x1",
        "from":Address::ZERO, "to":null, "contractAddress":null
    }))
    .unwrap()
}

fn block(number: u64, hash: B256) -> Block<Transaction> {
    let mut block = Block::<Transaction>::default();
    block.header.inner.number = number;
    block.header.hash = hash;
    block
}

#[tokio::test]
async fn deployment_waits_for_one_successor_and_rechecks_canonical_receipt() {
    let receipt = receipt();
    let (url, server) = serve(vec![
        Some(json!({"result":"0xa"})),
        Some(json!({"result":"0xb"})),
        Some(json!({"result":receipt})),
        Some(json!({"result":block(10, receipt.block_hash.unwrap())})),
    ])
    .await;
    let provider = connect_evm(&[url]).unwrap();
    confirmations::wait(
        &provider,
        &receipt,
        TransactionAction::Deploy(Address::repeat_byte(3)),
    )
    .await
    .unwrap();
    let calls = server.await.unwrap();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0]["method"], "eth_blockNumber");
    assert_eq!(calls[1]["method"], "eth_blockNumber");
    assert_eq!(calls[2]["method"], "eth_getTransactionReceipt");
    assert_eq!(calls[3]["params"], json!(["0xa", false]));
}

#[tokio::test]
async fn changed_missing_or_reverted_deployment_receipt_stops_recovery() {
    let original = receipt();
    let mut moved = original.clone();
    moved.block_hash = Some(B256::repeat_byte(9));
    let mut wrong_tx = original.clone();
    wrong_tx.transaction_hash = B256::repeat_byte(8);
    let mut reverted = serde_json::to_value(&original).unwrap();
    reverted["status"] = json!("0x0");
    for observed in [Value::Null, json!(moved), json!(wrong_tx), reverted] {
        let (url, server) = serve(vec![
            Some(json!({"result":"0xb"})),
            Some(json!({"result":observed})),
        ])
        .await;
        let provider = connect_evm(&[url]).unwrap();
        assert!(
            confirmations::wait(
                &provider,
                &original,
                TransactionAction::Deploy(Address::ZERO)
            )
            .await
            .is_err()
        );
        assert_eq!(server.await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn stale_receipt_on_noncanonical_block_is_rejected() {
    let receipt = receipt();
    let (url, server) = serve(vec![
        Some(json!({"result":"0xb"})),
        Some(json!({"result":receipt})),
        Some(json!({"result":block(10, B256::repeat_byte(9))})),
    ])
    .await;
    let provider = connect_evm(&[url]).unwrap();
    let error = confirmations::wait(
        &provider,
        &receipt,
        TransactionAction::Deploy(Address::ZERO),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("reorganized"));
    server.await.unwrap();
}

#[tokio::test]
async fn stalled_deployment_has_a_bounded_wait() {
    let (url, server) = serve(vec![Some(json!({"result":"0xa"}))]).await;
    let provider = connect_evm(&[url]).unwrap();
    let error = confirmations::wait_for_deployment(
        &provider,
        &receipt(),
        Duration::from_millis(100),
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("rerun the same command"));
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn observation_uses_confirmed_hash_and_detects_reorg_during_verification() {
    let confirmed = block(10, B256::repeat_byte(2));
    let (url, server) = serve(vec![
        Some(json!({"result":"0xb"})),
        Some(json!({"result":confirmed})),
        Some(json!({"result":block(10, B256::repeat_byte(9))})),
    ])
    .await;
    let provider = connect_evm(&[url]).unwrap();
    let observed = confirmations::observation(&provider).await.unwrap();
    assert_eq!(
        serde_json::to_value(confirmations::block_id(&observed)).unwrap(),
        json!({"blockHash":confirmed.header.hash,"requireCanonical":true})
    );
    assert!(
        confirmations::canonical(&provider, &observed)
            .await
            .is_err()
    );
    let calls = server.await.unwrap();
    assert_eq!(calls[1]["params"], json!(["0xa", false]));
    assert_eq!(calls[2]["params"], json!(["0xa", false]));
}
