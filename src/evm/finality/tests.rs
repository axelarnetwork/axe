use std::time::Duration;

use alloy::{
    primitives::{Address, B256},
    providers::ProviderBuilder,
    rpc::types::{Block, TransactionReceipt},
};
use serde_json::{Value, json};

use super::wait_with_timeout;
use crate::evm::{rpc_retry, test_rpc::serve};

fn receipt() -> TransactionReceipt {
    serde_json::from_value(json!({
        "type":"0x2", "status":"0x1", "cumulativeGasUsed":"0x5208",
        "logsBloom":format!("0x{}", "00".repeat(256)), "logs":[],
        "transactionHash":B256::repeat_byte(1), "blockHash":B256::repeat_byte(2),
        "blockNumber":"0xa", "gasUsed":"0x5208", "effectiveGasPrice":"0x1",
        "from":Address::ZERO, "to":null, "contractAddress":null
    }))
    .unwrap()
}

fn block(number: u64) -> Option<Value> {
    let mut block = Block::<alloy::rpc::types::Transaction>::default();
    block.header.inner.number = number;
    Some(json!({"result":block}))
}

#[tokio::test]
async fn waits_for_finality_then_rechecks_receipt() {
    let receipt = receipt();
    let (url, server) = serve(vec![block(9), block(10), Some(json!({"result":receipt}))]).await;
    let provider = ProviderBuilder::new().connect_client(rpc_retry::client(&url).unwrap());
    wait_with_timeout(
        &provider,
        &receipt,
        Duration::from_secs(5),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests[0]["params"], json!(["finalized", false]));
    assert_eq!(requests[1]["method"], "eth_getBlockByNumber");
    assert_eq!(requests[2]["method"], "eth_getTransactionReceipt");
}

#[tokio::test]
async fn reorged_or_missing_receipt_blocks_verification() {
    let original = receipt();
    let mut moved = original.clone();
    moved.block_hash = Some(B256::repeat_byte(3));
    for observed in [json!(moved), Value::Null] {
        let (url, server) = serve(vec![block(10), Some(json!({"result":observed}))]).await;
        let provider = ProviderBuilder::new().connect_client(rpc_retry::client(&url).unwrap());
        assert!(
            wait_with_timeout(
                &provider,
                &original,
                Duration::from_secs(5),
                Duration::from_millis(1)
            )
            .await
            .is_err()
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn unsupported_finalized_tag_does_not_fall_back_to_latest() {
    let (url, server) = serve(vec![Some(
        json!({"error":{"code":-32602,"message":"unsupported block tag"}}),
    )])
    .await;
    let provider = ProviderBuilder::new().connect_client(rpc_retry::client(&url).unwrap());
    let error = wait_with_timeout(
        &provider,
        &receipt(),
        Duration::from_secs(5),
        Duration::from_millis(1),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("verification has not been requested")
    );
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn slow_finality_has_a_deadline_without_requesting_verification() {
    let original = receipt();
    let (url, server) = serve(vec![block(9)]).await;
    let provider = ProviderBuilder::new().connect_client(rpc_retry::client(&url).unwrap());
    let error = wait_with_timeout(
        &provider,
        &original,
        Duration::from_millis(100),
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&original.transaction_hash.to_string())
    );
    assert!(
        error
            .to_string()
            .contains("Verification has not been requested")
    );
    assert_eq!(server.await.unwrap().len(), 1);
}
