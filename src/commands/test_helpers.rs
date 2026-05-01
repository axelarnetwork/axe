use cosmrs::crypto::secp256k1::SigningKey;
use eyre::Result;
use serde_json::json;

use crate::cosmos::{
    build_execute_msg_any, lcd_cosmwasm_smart_query, sign_and_broadcast_cosmos_tx,
};
use crate::timing::{
    AMPLIFIER_POLL_ATTEMPTS_5MIN, AMPLIFIER_POLL_ATTEMPTS_10MIN, AMPLIFIER_POLL_INTERVAL,
};
use crate::ui;

/// Walk wasm event attributes in a cosmos tx response and return the first
/// value whose key matches `attr_name`. None if the response is malformed,
/// has no wasm events, or none of them carries the attribute.
fn find_wasm_attr(tx_resp: &serde_json::Value, attr_name: &str) -> Option<String> {
    let events = tx_resp
        .pointer("/tx_response/events")
        .and_then(|v| v.as_array())?;

    for event in events {
        let event_type = event["type"].as_str().unwrap_or("");
        if (event_type == "wasm" || event_type.starts_with("wasm-"))
            && let Some(attrs) = event["attributes"].as_array()
        {
            for attr in attrs {
                if attr["key"].as_str() == Some(attr_name) {
                    return attr["value"]
                        .as_str()
                        .map(|v| v.trim_matches('"').to_string());
                }
            }
        }
    }
    None
}

/// Extract poll_id from the verify_messages tx response events.
/// Returns None if no poll was created (e.g. message already being verified by active relayers).
pub fn extract_poll_id(tx_resp: &serde_json::Value) -> Option<String> {
    find_wasm_attr(tx_resp, "poll_id")
}

/// Extract a named attribute from wasm events in a tx response.
pub fn extract_event_attr(tx_resp: &serde_json::Value, attr_name: &str) -> Result<String> {
    find_wasm_attr(tx_resp, attr_name)
        .ok_or_else(|| eyre::eyre!("{attr_name} not found in tx events"))
}

/// Poll the MultisigProver for a proof until it's signed.
pub async fn wait_for_proof(
    lcd: &str,
    multisig_prover: &str,
    session_id: &str,
) -> Result<serde_json::Value> {
    let query = json!({ "proof": { "multisig_session_id": session_id } });
    let spinner = ui::wait_spinner("Waiting for proof signing...");

    for i in 0..AMPLIFIER_POLL_ATTEMPTS_10MIN {
        if i > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }

        let resp = lcd_cosmwasm_smart_query(lcd, multisig_prover, &query).await?;
        let resp_str = serde_json::to_string(&resp)?;

        if resp_str.contains("completed") || resp_str.contains("Completed") {
            spinner.finish_and_clear();
            return Ok(resp);
        }

        let status = resp["status"].as_str().unwrap_or("unknown");
        spinner.set_message(format!(
            "Proof status: {status} (attempt {}/{})",
            i + 1,
            AMPLIFIER_POLL_ATTEMPTS_10MIN
        ));
    }

    spinner.finish_and_clear();
    Err(eyre::eyre!(
        "proof for session {session_id} timed out after 10 minutes"
    ))
}

/// Wait until the poll has enough SucceededOnChain votes to meet quorum.
pub async fn wait_for_poll_votes(lcd: &str, voting_verifier: &str, poll_id: &str) -> Result<()> {
    let query = json!({ "poll": { "poll_id": poll_id } });
    let spinner = ui::wait_spinner("Waiting for verifier votes...");

    for i in 0..AMPLIFIER_POLL_ATTEMPTS_10MIN {
        if i > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }

        let resp = lcd_cosmwasm_smart_query(lcd, voting_verifier, &query).await?;

        let poll = &resp["poll"];
        let quorum: u64 = poll["quorum"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let finished = poll["finished"].as_bool().unwrap_or(false);
        let expires_at: u64 = poll["expires_at"].as_u64().unwrap_or(0);

        if let Some(tallies) = poll["tallies"].as_array()
            && let Some(tally) = tallies.first()
        {
            let succeeded: u64 = tally["SucceededOnChain"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let failed: u64 = tally["FailedOnChain"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let not_found: u64 = tally["NotFound"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            spinner.set_message(format!(
                    "votes: ok={succeeded} fail={failed} notfound={not_found} (quorum={quorum}, expires={expires_at}, finished={finished})"
                ));

            if quorum > 0 && failed >= quorum {
                spinner.finish_and_clear();
                return Err(eyre::eyre!("poll failed: {failed} FailedOnChain votes"));
            }
            if quorum > 0 && not_found >= quorum {
                spinner.finish_and_clear();
                return Err(eyre::eyre!("poll failed: {not_found} NotFound votes"));
            }
            if quorum > 0 && succeeded >= quorum {
                spinner.finish_and_clear();
                ui::success(&format!("quorum reached ({succeeded}/{quorum})"));
                return Ok(());
            }
        }
    }

    spinner.finish_and_clear();
    Err(eyre::eyre!("poll {poll_id} timed out after 10 minutes"))
}

/// Submit `verify_messages` on the source-chain Gateway and return the poll_id
/// (or None if the message is already under active verification).
#[allow(clippy::too_many_arguments)]
pub async fn submit_verify_messages_amplifier(
    cosmos_msg: &serde_json::Value,
    signing_key: &SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    cosm_gateway: &str,
) -> Result<Option<String>> {
    let verify_msg = json!({ "verify_messages": [cosmos_msg] });
    let verify_any = build_execute_msg_any(axelar_address, cosm_gateway, &verify_msg)?;
    let verify_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        vec![verify_any],
    )
    .await?;
    Ok(extract_poll_id(&verify_resp))
}

/// End a poll on the VotingVerifier, retrying while the poll is still within
/// its block-expiry window ("cannot tally before poll end").
#[allow(clippy::too_many_arguments)]
pub async fn end_poll_with_retry(
    poll_id: &str,
    signing_key: &SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    voting_verifier: &str,
) -> Result<()> {
    let spinner = ui::wait_spinner("Ending poll (waiting for block expiry)...");
    for attempt in 0..AMPLIFIER_POLL_ATTEMPTS_5MIN {
        if attempt > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        let end_poll_msg = json!({ "end_poll": { "poll_id": poll_id } });
        let end_poll_any = build_execute_msg_any(axelar_address, voting_verifier, &end_poll_msg)?;
        match sign_and_broadcast_cosmos_tx(
            signing_key,
            axelar_address,
            lcd,
            chain_id,
            fee_denom,
            gas_price,
            vec![end_poll_any],
        )
        .await
        {
            Ok(_) => {
                spinner.finish_and_clear();
                ui::success("poll ended");
                return Ok(());
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("cannot tally before poll end") {
                    spinner
                        .set_message(format!("Poll not expired yet (attempt {})...", attempt + 1));
                    continue;
                }
                spinner.finish_and_clear();
                return Err(e);
            }
        }
    }
    spinner.finish_and_clear();
    Err(eyre::eyre!("end_poll did not complete after 60 attempts"))
}

/// Route an already-verified message through the source-chain Gateway,
/// retrying while it shows "not verified" (verifier votes still propagating).
#[allow(clippy::too_many_arguments)]
pub async fn route_messages_with_retry(
    cosmos_msg: &serde_json::Value,
    signing_key: &SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    cosm_gateway: &str,
) -> Result<()> {
    let spinner = ui::wait_spinner("Routing message to hub...");
    for attempt in 0..AMPLIFIER_POLL_ATTEMPTS_5MIN {
        if attempt > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        let route_msg = json!({ "route_messages": [cosmos_msg] });
        let route_any = build_execute_msg_any(axelar_address, cosm_gateway, &route_msg)?;
        match sign_and_broadcast_cosmos_tx(
            signing_key,
            axelar_address,
            lcd,
            chain_id,
            fee_denom,
            gas_price,
            vec![route_any],
        )
        .await
        {
            Ok(_) => {
                spinner.finish_and_clear();
                ui::success("message routed to hub");
                return Ok(());
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("not verified") {
                    spinner.set_message(format!(
                        "Message not yet verified (attempt {}/{})...",
                        attempt + 1,
                        AMPLIFIER_POLL_ATTEMPTS_5MIN
                    ));
                    continue;
                }
                spinner.finish_and_clear();
                return Err(e);
            }
        }
    }
    spinner.finish_and_clear();
    Err(eyre::eyre!(
        "route_messages did not complete after 60 attempts"
    ))
}

/// Wait for AxelarnetGateway to mark the message executable, then submit the
/// `execute` cosmwasm tx. Tolerates `already executed` (relayer raced us).
#[allow(clippy::too_many_arguments)]
pub async fn execute_on_axelarnet_gateway(
    message_id: &str,
    source_chain: &str,
    destination_chain_label: &str,
    payload: &[u8],
    signing_key: &SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    axelarnet_gateway: &str,
) -> Result<()> {
    let exec_query = json!({
        "executable_messages": {
            "cc_ids": [{
                "source_chain": source_chain,
                "message_id": message_id,
            }]
        }
    });
    let spinner = ui::wait_spinner("Waiting for message to be approved on hub...");
    for i in 0..AMPLIFIER_POLL_ATTEMPTS_10MIN {
        if i > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        let status = lcd_cosmwasm_smart_query(lcd, axelarnet_gateway, &exec_query).await?;
        let status_str = serde_json::to_string(&status)?;
        if !status_str.contains("null") && status_str.contains(message_id) {
            spinner.finish_and_clear();
            ui::success("message approved on hub");
            break;
        }
        if i == 119 {
            spinner.finish_and_clear();
            return Err(eyre::eyre!(
                "message not approved on AxelarnetGateway after 10 minutes"
            ));
        }
        spinner.set_message(format!(
            "Not yet approved (attempt {}/{})...",
            i + 1,
            AMPLIFIER_POLL_ATTEMPTS_10MIN
        ));
    }

    let payload_hex = alloy::hex::encode(payload);
    let execute_msg = json!({
        "execute": {
            "cc_id": {
                "message_id": message_id,
                "source_chain": source_chain,
            },
            "payload": payload_hex,
        }
    });
    let execute_any = build_execute_msg_any(axelar_address, axelarnet_gateway, &execute_msg)?;
    match sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        vec![execute_any],
    )
    .await
    {
        Ok(_) => {
            ui::success(&format!(
                "hub executed — message routed to {destination_chain_label} (relayer will handle delivery)"
            ));
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("already executed") {
                ui::success(&format!(
                    "message already executed on hub by relayer — continuing to {destination_chain_label}"
                ));
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wasm_event(attrs: &[(&str, &str)]) -> serde_json::Value {
        json!({
            "tx_response": {
                "events": [
                    {
                        "type": "wasm-routing",
                        "attributes": attrs
                            .iter()
                            .map(|(k, v)| json!({ "key": k, "value": v }))
                            .collect::<Vec<_>>(),
                    }
                ]
            }
        })
    }

    #[test]
    fn extract_poll_id_finds_attr_in_wasm_event() {
        let resp = wasm_event(&[("poll_id", "42"), ("other", "x")]);
        assert_eq!(extract_poll_id(&resp), Some("42".to_string()));
    }

    #[test]
    fn extract_poll_id_strips_surrounding_quotes() {
        // Cosmos LCD sometimes wraps numeric attribute values in quotes; the
        // helper must trim them so callers can parse the raw integer.
        let resp = wasm_event(&[("poll_id", "\"123\"")]);
        assert_eq!(extract_poll_id(&resp), Some("123".to_string()));
    }

    #[test]
    fn extract_poll_id_returns_none_when_no_event_or_attr() {
        assert_eq!(extract_poll_id(&json!({})), None);
        assert_eq!(extract_poll_id(&wasm_event(&[("foo", "bar")])), None);
    }

    #[test]
    fn extract_event_attr_errors_when_missing() {
        let err = extract_event_attr(&wasm_event(&[("foo", "bar")]), "missing").unwrap_err();
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn extract_event_attr_returns_value() {
        let resp = wasm_event(&[("multisig_session_id", "7")]);
        assert_eq!(
            extract_event_attr(&resp, "multisig_session_id").unwrap(),
            "7"
        );
    }
}
