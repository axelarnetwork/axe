use base64::Engine;
use cosmos_sdk_proto::cosmos::tx::v1beta1::{TxBody, TxRaw};
use prost::Message;
use serde_json::{Value, json};

use super::validate_payload;
use crate::cosmos::{build_execute_msg_any, build_submit_proposal_any};

fn proposal() -> (Vec<u8>, Value) {
    let payload = json!({"register_deployment":{"deployment_name":"example-24-87-85"}});
    let execute = build_execute_msg_any("governance", "coordinator", &payload).unwrap();
    let proposal = build_submit_proposal_any(
        "proposer",
        vec![execute],
        "Register",
        "summary",
        "400",
        "uaxl",
        true,
    )
    .unwrap();
    let body = TxBody {
        messages: vec![proposal],
        ..Default::default()
    };
    let raw = TxRaw {
        body_bytes: body.encode_to_vec(),
        ..Default::default()
    }
    .encode_to_vec();
    (
        raw,
        json!({"title":"Register","expedited":true,"messages":[{
            "@type":"/cosmwasm.wasm.v1.MsgExecuteContract", "sender":"governance", "contract":"coordinator", "funds":[],
            "msg":base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&payload).unwrap())
        }]}),
    )
}

#[test]
fn proposal_must_match_approved_messages_and_expedited_mode() {
    let (raw, actual) = proposal();
    validate_payload(&raw, &actual).unwrap();
    for changed in [
        json!({"contract":"another-coordinator"}),
        json!({"sender":"another-authority"}),
        json!({"funds":[{"denom":"uaxl","amount":"1"}]}),
        json!({"msg":{"register_deployment":{"deployment_name":"another-deployment"}}}),
    ] {
        let mut invalid = actual.clone();
        for (key, value) in changed.as_object().unwrap() {
            invalid["messages"][0][key] = value.clone();
        }
        assert!(validate_payload(&raw, &invalid).is_err());
    }
    let mut invalid = actual.clone();
    invalid["expedited"] = json!(false);
    assert!(validate_payload(&raw, &invalid).is_err());
    invalid = actual;
    invalid["messages"].as_array_mut().unwrap().clear();
    assert!(validate_payload(&raw, &invalid).is_err());
}

#[test]
fn proposal_accepts_equivalent_decoded_json_payload() {
    let (raw, mut actual) = proposal();
    actual["messages"][0]["msg"] =
        json!({"register_deployment":{"deployment_name":"example-24-87-85"}});
    validate_payload(&raw, &actual).unwrap();
}
