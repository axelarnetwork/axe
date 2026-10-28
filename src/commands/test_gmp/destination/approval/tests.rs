use std::time::Duration;

use alloy::{
    primitives::{Address, B256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionReceipt,
};
use serde_json::{Value, json};

use super::{EvmExecutionRequest, wait_with_timeout};
use crate::evm::test_rpc::serve;

fn receipt(success: bool) -> TransactionReceipt {
    serde_json::from_value(json!({
        "type":"0x2", "status":if success { "0x1" } else { "0x0" },
        "cumulativeGasUsed":"0x5208", "logsBloom":format!("0x{}", "00".repeat(256)),
        "logs":[], "transactionHash":B256::repeat_byte(1),
        "blockHash":B256::repeat_byte(2), "blockNumber":"0xa", "gasUsed":"0x5208",
        "effectiveGasPrice":"0x1", "from":Address::ZERO, "to":Address::ZERO,
        "contractAddress":null
    }))
    .unwrap()
}

fn request<P: Provider>(provider: &P) -> EvmExecutionRequest<'_, P> {
    EvmExecutionRequest {
        provider,
        gateway: Address::repeat_byte(1),
        sender_receiver: Address::repeat_byte(2),
        source_chain: "unichain-sepolia-3",
        source_address: "0x1111111111111111111111111111111111111111",
        message_id: "source-tx-1",
        execute_data_hex: "",
        payload_bytes: &[],
        payload_hash: B256::repeat_byte(3),
        step_idx_approve: 7,
        step_idx_execute: 8,
        total_steps: 8,
    }
}

fn approval(approved: bool) -> Option<Value> {
    Some(json!({"result":format!("0x{:064x}", u8::from(approved))}))
}

#[tokio::test]
async fn stale_false_reads_eventually_succeed_without_resubmitting() {
    let (url, server) = serve(vec![approval(false), approval(false), approval(true)]).await;
    let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
    wait_with_timeout(
        &request(&provider),
        &receipt(true),
        Duration::from_secs(5),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    let calls = server.await.unwrap();
    assert_eq!(calls.len(), 3);
    for call in &calls {
        assert_eq!(call["method"], "eth_call");
        assert_eq!(call["params"], calls[0]["params"]);
    }
}

#[tokio::test]
async fn temporary_rpc_error_is_retried() {
    let (url, server) = serve(vec![
        Some(json!({"error":{"code":-32000,"message":"backend temporarily unavailable"}})),
        approval(true),
    ])
    .await;
    let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
    wait_with_timeout(
        &request(&provider),
        &receipt(true),
        Duration::from_secs(5),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn missing_approval_has_a_deadline_and_retains_transaction_evidence() {
    let (url, server) = serve(vec![approval(false)]).await;
    let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let receipt = receipt(true);
    let error = wait_with_timeout(
        &request(&provider),
        &receipt,
        Duration::from_millis(100),
        Duration::from_secs(10),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains(&receipt.transaction_hash.to_string()));
    assert!(error.contains("source-tx-1"));
    assert!(error.contains("isMessageApproved returned false"));
    assert!(error.contains("has not sent the receiver execution"));
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn reverted_proof_fails_before_any_rpc_read() {
    let (url, server) = serve(vec![]).await;
    let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let error = wait_with_timeout(
        &request(&provider),
        &receipt(false),
        Duration::from_secs(5),
        Duration::from_millis(1),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("reverted"));
    assert!(server.await.unwrap().is_empty());
}
