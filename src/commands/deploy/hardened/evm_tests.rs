use alloy::primitives::{Address, B256, Bytes, keccak256};
use alloy::providers::ProviderBuilder;
use alloy::rpc::client::RpcClient;
use alloy::transports::mock::Asserter;
use tokio::time::Instant;

use crate::commands::deploy::hardened::{
    confirmations, evm_recovery, types::EvmConfirmationPolicy,
};
use serde_json::{Value, json};

use super::*;

fn saved() -> Transaction {
    let raw = Bytes::from(vec![1, 2, 3]);
    Transaction::Evm {
        intent: B256::ZERO,
        hash: keccak256(&raw),
        raw,
        sender: Address::from([1; 20]),
        nonce: 4,
        gas_cost: "1".into(),
    }
}

#[tokio::test]
async fn changed_intent_is_rejected_before_rpc_or_broadcast() {
    let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(Asserter::new()));
    let error = resume(&provider, &saved(), B256::from([7; 32]), "deploy")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("mismatch"));
}

#[tokio::test]
async fn consumed_nonce_with_missing_receipt_never_allocates_a_new_nonce() {
    let mock = Asserter::new();
    mock.push_success(&Value::Null); // receipt
    mock.push_success(&"0x5"); // nonce already consumed
    let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
    let error = resume(&provider, &saved(), B256::ZERO, "deploy")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("nonce consumed"));
}

#[tokio::test(start_paused = true)]
async fn unresolved_rebroadcast_preserves_the_original_hash() {
    let mock = Asserter::new();
    let tx = saved();
    let Transaction::Evm { hash, .. } = &tx else {
        unreachable!()
    };
    mock.push_success(&Value::Null);
    mock.push_success(&"0x4");
    mock.push_success(&Value::Null);
    mock.push_success(hash);
    for _ in 0..10 {
        mock.push_success(&Value::Null);
    }
    let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
    let error = resume(&provider, &tx, B256::ZERO, "deploy")
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::commands::deploy::hardened::types::Paused>()
            .is_some()
    );
    assert!(error.to_string().contains(&hash.to_string()));
}

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

#[tokio::test]
async fn included_but_unfinalized_transaction_pauses_without_waiting() {
    let mock = Asserter::new();
    let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
    block.header.inner.number = 9;
    mock.push_success(&block);
    let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
    let error = finalized(&provider, receipt(), "deploy").await.unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::commands::deploy::hardened::types::Paused>()
            .is_some()
    );
}

#[tokio::test]
async fn finality_requires_the_receipt_block_to_be_canonical() {
    for hash in [B256::repeat_byte(2), B256::repeat_byte(3)] {
        let mock = Asserter::new();
        let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = 10;
        block.header.hash = hash;
        mock.push_success(&block);
        mock.push_success(&block);
        let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
        assert_eq!(
            finalized(&provider, receipt(), "deploy").await.is_ok(),
            hash == B256::repeat_byte(2)
        );
    }
}

#[tokio::test]
async fn confirmation_depth_counts_inclusion_and_still_requires_canonicality() {
    for (count, head, canonical, success) in [
        (1, 10, true, true),
        (2, 10, true, false),
        (2, 11, true, true),
        (3, 11, true, false),
        (3, 12, true, true),
        (1, 10, false, false),
    ] {
        let mock = Asserter::new();
        let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = head;
        mock.push_success(&block);
        if head >= 10 + count - 1 {
            block.header.inner.number = 10;
            block.header.hash = B256::repeat_byte(if canonical { 2 } else { 3 });
            mock.push_success(&block);
        }
        let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
        let policy = count.to_string().parse().unwrap();
        let result =
            evm_recovery::wait_confirmed(&provider, receipt(), "deploy", Instant::now(), policy)
                .await;
        assert_eq!(
            result.is_ok(),
            success,
            "count={count}, head={head}, canonical={canonical}"
        );
    }
}

#[test]
fn confirmation_height_cannot_wrap() {
    let policy: EvmConfirmationPolicy = "2".parse().unwrap();
    assert!(confirmations::required_height(policy, u64::MAX).is_err());
}
