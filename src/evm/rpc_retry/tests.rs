use alloy::transports::TransportErrorKind;
use serde_json::json;

use super::{client, retryable};
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
