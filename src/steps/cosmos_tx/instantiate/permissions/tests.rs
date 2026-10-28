use serde_json::json;

use super::{allows_coordinator, check, validate_permissions};
use crate::http::tests::{response, serve};
use crate::steps::cosmos_tx::instantiate::types::{ChainCodeIds, InstantiatePermission};

fn permission(mode: &str, addresses: &[&str]) -> InstantiatePermission {
    serde_json::from_value(json!({"permission":mode,"addresses":addresses})).unwrap()
}

#[test]
fn coordinator_must_be_allowed_even_when_governance_or_proposer_is_allowed() {
    assert!(allows_coordinator(
        &permission("Everybody", &[]),
        "coordinator"
    ));
    assert!(allows_coordinator(
        &permission("AnyOfAddresses", &["governance", "coordinator"]),
        "coordinator"
    ));
    assert!(!allows_coordinator(
        &permission("AnyOfAddresses", &["governance", "proposer"]),
        "coordinator"
    ));
    for mode in ["Nobody", "Unspecified", "unknown"] {
        assert!(!allows_coordinator(
            &permission(mode, &["coordinator"]),
            "coordinator"
        ));
    }
}

#[test]
fn all_unauthorized_codes_are_reported_together() {
    let error = validate_permissions(
        "coordinator",
        &[
            (
                "Gateway",
                24,
                permission("AnyOfAddresses", &["coordinator"]),
            ),
            (
                "VotingVerifier",
                87,
                permission("AnyOfAddresses", &["deployer"]),
            ),
            (
                "MultisigProver",
                85,
                permission("AnyOfAddresses", &["governance"]),
            ),
        ],
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("VotingVerifier code 87"));
    assert!(error.contains("MultisigProver code 85"));
    assert!(!error.contains("Gateway code 24"));
    assert!(error.contains("Changing MNEMONIC or EVM private keys will not fix this"));
}

#[test]
fn preflight_passes_after_coordinator_is_added_without_replacing_existing_addresses() {
    let permissions = [
        (
            "Gateway",
            24,
            permission("AnyOfAddresses", &["coordinator"]),
        ),
        (
            "VotingVerifier",
            87,
            permission("AnyOfAddresses", &["deployer", "coordinator"]),
        ),
        (
            "MultisigProver",
            85,
            permission("AnyOfAddresses", &["governance", "coordinator"]),
        ),
    ];
    assert!(validate_permissions("coordinator", &permissions).is_ok());
}

#[tokio::test]
async fn reads_metadata_only_and_retries_a_truncated_body() {
    let body =
        r#"{"instantiate_permission":{"permission":"AnyOfAddresses","addresses":["coordinator"]}}"#;
    let (url, task) = serve(vec![
        Some("HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{".into()),
        response("200 OK", body),
        response("200 OK", body),
        response("200 OK", body),
    ])
    .await;
    check(
        url.as_str(),
        "coordinator",
        &ChainCodeIds {
            gateway: 17,
            verifier: 64,
            prover: 55,
        },
    )
    .await
    .unwrap();
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 4);
    for (request, code) in requests.iter().zip([17, 17, 64, 55]) {
        assert!(request.starts_with(&format!("GET /cosmwasm/wasm/v1/code-info/{code} ")));
    }
}

#[tokio::test]
async fn metadata_permissions_still_reject_an_unauthorized_coordinator() {
    let body = r#"{"instantiate_permission":{"permission":"Nobody","addresses":[]}}"#;
    let (url, task) = serve(vec![response("200 OK", body); 3]).await;
    let error = check(
        url.as_str(),
        "coordinator",
        &ChainCodeIds {
            gateway: 17,
            verifier: 64,
            prover: 55,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("not authorized to instantiate"));
    assert_eq!(task.await.unwrap().len(), 3);
}
