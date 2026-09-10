use alloy::primitives::Address;
use serde_json::json;

use super::{chain_salt_key, contract_not_found, deployment_not_found, validate_identity};
use crate::evm::get_salt_from_key;
use crate::steps::cosmos_tx::instantiate::types::{
    ChainCodeIds, Deployment, InstantiatePlan, ProverConfig, VerifierConfig,
};

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

#[test]
fn existing_deployment_must_match_chain_gateway_and_prover() {
    let plan = InstantiatePlan {
        execute_msg: json!({}),
        deployment_name: "robinhood-24-87-85".into(),
        salt_key: "test".into(),
        domain_separator: alloy::hex::encode([7; 32]),
        contract_admin: "admin",
        codes: ChainCodeIds {
            gateway: 24,
            verifier: 87,
            prover: 85,
        },
    };
    let deployment = Deployment {
        chain_name: "robinhood".into(),
        gateway_address: "gateway".into(),
        verifier_address: "verifier".into(),
        prover_address: "prover".into(),
    };
    let mut verifier = VerifierConfig {
        source_chain: "robinhood".into(),
        source_gateway_address: Address::ZERO.to_string(),
    };
    let mut prover = ProverConfig {
        chain_name: "robinhood".into(),
        gateway: "gateway".into(),
        voting_verifier: "verifier".into(),
        domain_separator: [7; 32],
    };
    assert!(
        validate_identity(
            "robinhood",
            Address::ZERO,
            &plan,
            &deployment,
            &verifier,
            &prover
        )
        .is_ok()
    );
    assert!(
        validate_identity(
            "arc-11",
            Address::ZERO,
            &plan,
            &deployment,
            &verifier,
            &prover
        )
        .is_err()
    );
    verifier.source_gateway_address = Address::repeat_byte(1).to_string();
    assert!(
        validate_identity(
            "robinhood",
            Address::ZERO,
            &plan,
            &deployment,
            &verifier,
            &prover
        )
        .is_err()
    );
    verifier.source_gateway_address = Address::ZERO.to_string();
    prover.domain_separator = [8; 32];
    assert!(
        validate_identity(
            "robinhood",
            Address::ZERO,
            &plan,
            &deployment,
            &verifier,
            &prover
        )
        .is_err()
    );
    prover.domain_separator = [7; 32];
    prover.gateway = "another gateway".into();
    assert!(
        validate_identity(
            "robinhood",
            Address::ZERO,
            &plan,
            &deployment,
            &verifier,
            &prover
        )
        .is_err()
    );
}
