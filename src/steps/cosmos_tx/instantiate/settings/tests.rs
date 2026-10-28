use alloy::primitives::Address;
use serde_json::json;

use super::validate;
use crate::commands::deploy::hardened::tests::initial_state;

#[test]
fn generated_settings_must_match_the_plan_before_submission() {
    let mut state = initial_state();
    let gateway = Address::repeat_byte(7);
    state.predicted_gateway_address = Some(gateway);
    let verifier = json!({"governanceAddress":"gov","serviceName":"amplifier",
        "sourceGatewayAddress":gateway.to_string(),"votingThreshold":["2","3"],
        "blockExpiry":50,"confirmationHeight":1,"msgIdFormat":"hex_tx_hash_and_event_index","addressFormat":"eip55"});
    let prover = json!({"governanceAddress":"gov","serviceName":"amplifier",
        "adminAddress":state.hardened_plan.as_ref().unwrap().prover_admin,
        "signingThreshold":["2","3"],"verifierSetDiffThreshold":0,"keyType":"ecdsa"});
    validate(&state, "gov", &verifier, &prover).unwrap();
    for key in [
        "votingThreshold",
        "blockExpiry",
        "confirmationHeight",
        "sourceGatewayAddress",
        "serviceName",
        "governanceAddress",
        "msgIdFormat",
        "addressFormat",
    ] {
        let mut edited = verifier.clone();
        edited[key] = json!("changed");
        assert!(validate(&state, "gov", &edited, &prover).is_err(), "{key}");
    }
    for key in [
        "signingThreshold",
        "adminAddress",
        "serviceName",
        "governanceAddress",
        "keyType",
        "verifierSetDiffThreshold",
    ] {
        let mut edited = prover.clone();
        edited[key] = json!("changed");
        assert!(
            validate(&state, "gov", &verifier, &edited).is_err(),
            "{key}"
        );
    }
}
