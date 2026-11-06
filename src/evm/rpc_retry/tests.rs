use alloy::transports::TransportErrorKind;
use serde_json::json;

use super::{client, client_with_block_retries, retryable};
use crate::evm::test_rpc::serve;

#[test]
fn retries_transient_http_errors_but_not_invalid_requests_or_authentication() {
    for status in [429, 502, 503, 504] {
        assert!(retryable(&TransportErrorKind::http_error(
            status,
            String::new()
        )));
    }
    for status in [400, 401, 403, 404, 501] {
        assert!(!retryable(&TransportErrorKind::http_error(
            status,
            String::new()
        )));
    }
}

#[tokio::test]
async fn lost_broadcast_response_retries_identical_signed_bytes() {
    let (url, server) = serve(vec![None, Some(json!({"result":"0x1234"}))]).await;
    let result: String = client(&url)
        .unwrap()
        .request("eth_sendRawTransaction", ["0xdeadbeef"])
        .await
        .unwrap();
    assert_eq!(result, "0x1234");
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0]["method"], "eth_sendRawTransaction");
}

#[tokio::test]
async fn deterministic_reverts_are_not_retried() {
    let (url, server) = serve(vec![Some(
        json!({"error":{"code":3,"message":"execution reverted","data":"0x12345678"}}),
    )])
    .await;
    let result = client(&url)
        .unwrap()
        .request::<_, String>("eth_estimateGas", [json!({})])
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.as_error_resp().unwrap().code, 3);
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn rate_limit_retries_are_bounded() {
    let error = Some(json!({"error":{"code":429,"message":"Too Many Requests"}}));
    let (url, server) = serve(vec![error; 4]).await;
    let result = client(&url)
        .unwrap()
        .request::<_, String>("eth_chainId", ())
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Max retries exceeded")
    );
    assert_eq!(server.await.unwrap().len(), 4);
}

#[tokio::test]
async fn unavailable_block_retries_identical_call_at_the_requested_block() {
    let (url, server) = serve(vec![
        Some(json!({"error":{"code":-32000,"message":"[Request ID: example] Error occurred during transaction simulation: Unknown block number"}})),
        Some(json!({"result":"0x1234"})),
    ]).await;
    let result: String = client_with_block_retries(&url)
        .unwrap()
        .request(
            "eth_call",
            (
                json!({"to":"0x1111111111111111111111111111111111111111","data":"0x8da5cb5b"}),
                "0x1234",
            ),
        )
        .await
        .unwrap();
    assert_eq!(result, "0x1234");
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0]["params"][1], "0x1234");
}

#[tokio::test]
async fn unavailable_block_retry_is_opt_in_and_bounded() {
    let error = Some(json!({"error":{"code":-32000,"message":"Unknown block number"}}));
    let (url, server) = serve(vec![error.clone()]).await;
    assert!(
        client(&url)
            .unwrap()
            .request::<_, String>("eth_call", (json!({}), "latest"))
            .await
            .is_err()
    );
    assert_eq!(server.await.unwrap().len(), 1);
    let (url, server) = serve(vec![error; 4]).await;
    let result = client_with_block_retries(&url)
        .unwrap()
        .request::<_, String>("eth_call", (json!({}), "latest"))
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Max retries exceeded")
    );
    assert_eq!(server.await.unwrap().len(), 4);
}

#[tokio::test]
async fn block_retry_does_not_retry_contract_reverts_or_other_rpc_errors() {
    for error in [
        json!({"code":3,"message":"Unknown block number","data":"0x1234"}),
        json!({"code":-32000,"message":"execution reverted"}),
        json!({"code":-32000,"message":"Unknown block number","data":"0x1234"}),
    ] {
        let (url, server) = serve(vec![Some(json!({"error":error}))]).await;
        assert!(
            client_with_block_retries(&url)
                .unwrap()
                .request::<_, String>("eth_call", (json!({}), "latest"))
                .await
                .is_err()
        );
        assert_eq!(server.await.unwrap().len(), 1);
    }
}
