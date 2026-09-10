use alloy::transports::{TransportError, TransportErrorKind};
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
