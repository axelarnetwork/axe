use alloy::transports::{HttpError, TransportError, TransportErrorKind};
use serde_json::json;

use super::{CheckOutcome, check_sync_status, summarise};

fn rpc_error(code: i64) -> TransportError {
    TransportError::ErrorResp(
        serde_json::from_value(json!({"code": code, "message": "RPC error"})).unwrap(),
    )
}

#[test]
fn unavailable_sync_method_is_a_warning() {
    let check = check_sync_status(Err(rpc_error(-32601)));

    assert!(!check.critical);
    assert!(matches!(check.outcome, CheckOutcome::Warn(_)));
    assert!(summarise(&[check]).is_ok());
}

#[test]
fn synced_node_passes() {
    let check = check_sync_status(Ok(json!(false)));

    assert!(check.critical);
    assert!(matches!(check.outcome, CheckOutcome::Pass(_)));
}

#[test]
fn syncing_node_still_blocks_deployment() {
    let check = check_sync_status(Ok(json!({
        "startingBlock": "0x0",
        "currentBlock": "0x1",
        "highestBlock": "0x2"
    })));

    assert!(check.critical);
    assert!(matches!(check.outcome, CheckOutcome::Fail(_)));
    assert!(summarise(&[check]).is_err());
}

#[test]
fn other_rpc_and_transport_errors_still_block_deployment() {
    for error in [
        rpc_error(-32603),
        TransportErrorKind::custom_str("connection timed out"),
    ] {
        let check = check_sync_status(Err(error));

        assert!(check.critical);
        assert!(matches!(check.outcome, CheckOutcome::Fail(_)));
        assert!(summarise(&[check]).is_err());
    }
}

fn http_error(status: u16, body: &str) -> TransportError {
    TransportError::Transport(TransportErrorKind::HttpError(HttpError {
        status,
        body: body.into(),
    }))
}

#[test]
fn unichain_http_forbidden_method_not_whitelisted_is_a_warning() {
    let body = r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"rpc method is not whitelisted"},"id":1}"#;
    let check = check_sync_status(Err(http_error(403, body)));
    assert!(!check.critical);
    assert!(matches!(check.outcome, CheckOutcome::Warn(_)));
    assert!(summarise(&[check]).is_ok());
}

#[test]
fn other_http_errors_and_malformed_responses_remain_critical() {
    let unavailable =
        r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"method not found"},"id":1}"#;
    for (status, body) in [
        (401, unavailable),
        (429, unavailable),
        (503, unavailable),
        (403, "Forbidden"),
        (403, "rpc method is not whitelisted"),
        (
            403,
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"internal error"},"id":1}"#,
        ),
        (403, r#"{"jsonrpc":"2.0","result":false,"id":1}"#),
    ] {
        let check = check_sync_status(Err(http_error(status, body)));
        assert!(check.critical, "HTTP {status}: {body}");
        assert!(matches!(check.outcome, CheckOutcome::Fail(_)));
        assert!(summarise(&[check]).is_err());
    }
}
