use serde_json::json;

use super::{allows_coordinator, validate_permissions};
use crate::steps::cosmos_tx::instantiate::types::InstantiatePermission;

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
