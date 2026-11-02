use std::sync::atomic::{AtomicU32, Ordering};

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use serde_json::json;

use super::rpc;

#[tokio::test]
async fn unavailable_state_retries_finalized_without_reading_latest() {
    let (url, server) = crate::evm::test_rpc::serve(vec![
        Some(json!({"error":{"code":-32000,"message":"historical state abc is not available"}})),
        Some(json!({"result":"0x6001"})),
    ])
    .await;
    let provider = crate::evm::connect_evm(&[url]).unwrap();
    let code = rpc::finalized_read("test finalized runtime", || async {
        Ok(provider
            .get_code_at(Address::repeat_byte(1))
            .block_id(BlockId::finalized())
            .await?)
    })
    .await
    .unwrap();
    assert_eq!(code, Bytes::from_static(&[0x60, 0x01]));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request["method"], "eth_getCode");
        assert_eq!(request["params"][1], "finalized");
    }
}

#[tokio::test(start_paused = true)]
async fn missing_historical_state_exhausts_three_attempts_with_rpc_guidance() {
    let attempts = AtomicU32::new(0);
    let result: eyre::Result<()> = rpc::finalized_read("test", || async {
        attempts.fetch_add(1, Ordering::SeqCst);
        Err(eyre::eyre!("historical state abc is not available"))
    })
    .await;
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let error = result.unwrap_err();
    assert!(error.to_string().contains("ASG_RPC_URL"));
    assert!(format!("{error:#}").contains("historical state abc"));
}

#[tokio::test(start_paused = true)]
async fn contract_mismatches_and_reverts_are_not_retried() {
    for message in [
        "ASG runtime mismatch",
        "execution reverted",
        "method not found",
    ] {
        let attempts = AtomicU32::new(0);
        let result: eyre::Result<()> = rpc::finalized_read("test", || async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(eyre::eyre!(message))
        })
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(result.unwrap_err().to_string(), message);
    }
}

#[test]
fn rpc_override_wins_and_invalid_urls_do_not_expose_credentials() {
    assert_eq!(
        rpc::url(Some("https://configured.example"), None).unwrap(),
        "https://configured.example"
    );
    assert_eq!(
        rpc::url(
            Some("https://configured.example"),
            Some("https://selected.example")
        )
        .unwrap(),
        "https://selected.example"
    );
    assert!(rpc::url(None, None).is_err());
    for invalid in [
        "secret-token",
        "file:///secret-token",
        "ftp://example/secret-token",
        "",
    ] {
        let error = rpc::url(None, Some(invalid)).unwrap_err().to_string();
        assert!(!error.contains("secret-token"));
    }
}
