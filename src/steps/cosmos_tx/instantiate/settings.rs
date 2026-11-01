use eyre::Result;
use serde_json::{Value, json};

use crate::state::State;

#[cfg(test)]
mod tests;

/// Reject edits to the generated config before they become an approved proposal.
pub(super) fn validate(
    state: &State,
    governor: &str,
    verifier: &Value,
    prover: &Value,
) -> Result<()> {
    let plan = state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing deployment plan"))?;
    let gateway = state
        .predicted_gateway_address
        .ok_or_else(|| eyre::eyre!("missing predicted gateway"))?;
    let expected_verifier = json!({
        "governanceAddress": governor,
        "serviceName": state.env.verifier_service_name(),
        "sourceGatewayAddress": gateway.to_string(),
        "votingThreshold": plan.voting_threshold.map(|v| v.to_string()),
        "blockExpiry": plan.block_expiry,
        "confirmationHeight": plan.confirmation_height,
        "msgIdFormat": "hex_tx_hash_and_event_index",
        "addressFormat": "eip55"
    });
    let expected_prover = json!({
        "governanceAddress": governor,
        "adminAddress": plan.prover_admin,
        "signingThreshold": plan.signing_threshold.map(|v| v.to_string()),
        "serviceName": state.env.verifier_service_name(),
        "verifierSetDiffThreshold": 0,
        "keyType": "ecdsa"
    });
    for (name, actual, expected) in [
        ("VotingVerifier", verifier, expected_verifier),
        ("MultisigProver", prover, expected_prover),
    ] {
        let fields = expected
            .as_object()
            .ok_or_else(|| eyre::eyre!("invalid expected settings"))?;
        for (key, value) in fields {
            eyre::ensure!(
                actual.get(key) == Some(value),
                "{name}.{key} differs from the approved deployment plan; restore the generated config before submitting. No proposal was sent"
            );
        }
    }
    Ok(())
}
