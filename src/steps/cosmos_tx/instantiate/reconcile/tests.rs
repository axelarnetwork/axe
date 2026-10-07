use serde_json::json;

use super::{chain_salt_key, contract_not_found, deployment_not_found};
use crate::evm::get_salt_from_key;

#[test]
fn coordinator_salts_are_stable_and_isolated_between_chains() {
    let robinhood = get_salt_from_key(&chain_salt_key("robinhood", "v1.0.13"));
    assert_eq!(
        robinhood,
        get_salt_from_key(&chain_salt_key("robinhood", "v1.0.13"))
    );
    assert_ne!(
        robinhood,
        get_salt_from_key(&chain_salt_key("arc-11", "v1.0.13"))
    );
    assert_ne!(robinhood, get_salt_from_key("v1.0.13"));
    assert_ne!(
        robinhood,
        get_salt_from_key(&chain_salt_key("robinhood", "v1.0.14"))
    );
}

#[test]
fn only_exact_missing_deployment_errors_allow_creation() {
    let body = json!({"code":2,"message":"deployment robinhood-24-87-85 not found: query wasm contract failed"}).to_string();
    assert!(deployment_not_found(&body, "robinhood-24-87-85"));
    assert!(!deployment_not_found(&body, "arc-11-24-87-85"));
    for body in [
        "gateway timeout",
        r#"{"code":2,"message":"unknown query variant"}"#,
        r#"{"code":5,"message":"deployment robinhood-24-87-85 not found: missing contract"}"#,
    ] {
        assert!(!deployment_not_found(body, "robinhood-24-87-85"));
    }
}

#[test]
fn missing_contract_does_not_hide_server_errors() {
    let status = reqwest::StatusCode::INTERNAL_SERVER_ERROR;
    let body =
        json!({"code":2,"message":"codespace wasm code 22: no such contract: address axelar1test"})
            .to_string();
    assert!(contract_not_found(status, &body, "axelar1test"));
    assert!(!contract_not_found(status, &body, "axelar1different"));
    assert!(!contract_not_found(
        status,
        "upstream timeout",
        "axelar1test"
    ));
    assert!(!contract_not_found(
        status,
        r#"{"code":2,"message":"internal error"}"#,
        "axelar1test"
    ));
}
