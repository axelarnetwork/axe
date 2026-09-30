use super::*;
use crate::commands::propose::relay::relay;
use crate::commands::propose::types::RelayPlan;
use crate::commands::propose::{helpers, test_support::config};
use crate::http::tests::{response, serve};
use crate::types::Network;
use serde_json::json;

#[tokio::test]
async fn repeated_completed_relay_is_read_only_and_needs_no_signer() {
    let payload =
        helpers::encode_governance_payload(2, Address::from([4; 20]), vec![1, 2, 3].into(), 0);
    let event = json!({"type":"wasm-contract_called", "attributes":[
        {"key":"_contract_address","value":"axelar1gateway"},
        {"key":"payload_hash","value":format!("{:x}",alloy::primitives::keccak256(&payload))},
        {"key":"message_id","value":"governance-message-645"},
        {"key":"source_chain","value":"axelar"},
        {"key":"source_address","value":"axelar1gov"},
        {"key":"destination_chain","value":"flow"},
        {"key":"destination_address","value":Address::from([2; 20]).to_string()}
    ]});
    // Identical calls can occur in the same block for different chains or senders.
    // A mismatching event must not hide the subsequent correct governance event.
    let mut events = Vec::new();
    for key in [
        "_contract_address",
        "source_chain",
        "source_address",
        "destination_chain",
        "destination_address",
    ] {
        let mut wrong = event.clone();
        let attr = wrong["attributes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|attr| attr["key"] == key)
            .unwrap();
        attr["value"] = json!("wrong");
        events.push(wrong);
    }
    events.push(event);
    let rpc_responses = [
        json!({"result":{"sync_info":{"latest_block_height":"2"}}}),
        json!({"result":{"block":{"header":{"time":"2026-09-29T16:03:25Z"}}}}),
        json!({"result":{"finalize_block_events":events}}),
    ];
    let responses = rpc_responses
        .iter()
        .cycle()
        .take(6)
        .map(|r| response("200 OK", &r.to_string()))
        .collect();
    let (rpc, rpc_task) = serve(responses).await;
    let evm_responses = [
        Some(json!({"result":format!("0x{:064x}",1)})),
        Some(json!({"result":format!("0x{:064x}",0)})),
    ];
    let (evm, evm_task) =
        crate::evm::test_rpc::serve(evm_responses.iter().cycle().take(4).cloned().collect()).await;
    let mut cfg = config(String::new());
    cfg.axelar_rpc = rpc.to_string();
    cfg.edge_rpc = evm;
    let asg = helpers::AsgInfo {
        operator: Address::ZERO,
        governance_chain: "axelar".into(),
        governance_address: cfg.gov_module.clone(),
        minimum_time_lock_delay: 300,
    };
    let plan = RelayPlan {
        ptype: ProposalType::Operator,
        target: Address::from([4; 20]),
        calldata: vec![1, 2, 3].into(),
        payload,
    };
    let time = chrono::DateTime::parse_from_rfc3339("2026-09-29T16:03:24Z").unwrap();
    relay(&cfg, &asg, &plan, time).await.unwrap();
    relay(&cfg, &asg, &plan, time).await.unwrap();
    assert_eq!(rpc_task.await.unwrap().len(), 6);
    let requests = evm_task.await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request["method"] == "eth_call")
    );
}

fn args() -> ProposeArgs {
    ProposeArgs {
        network: Network::Testnet,
        chain: "flow".into(),
        op: None,
        calldata: None,
        target: None,
        its_chain: None,
        proposal_type: ProposalType::Operator,
        relay: true,
        proposal_id: None,
        new_proposal: false,
        standard: false,
        eta: None,
        confirm_mainnet: false,
        yes: false,
    }
}

fn proposal(ptype: ProposalType, eta: u64) -> Value {
    let cfg = config(String::new());
    let payload = helpers::encode_governance_payload(
        ptype.command(),
        Address::from([4; 20]),
        Bytes::from(vec![1, 2, 3]),
        eta,
    );
    json!({"id":"645", "status":"PROPOSAL_STATUS_PASSED", "voting_end_time":"2026-09-29T16:03:24Z", "messages":[{
        "@type":"/cosmwasm.wasm.v1.MsgExecuteContract", "sender":cfg.gov_module, "contract":cfg.axelarnet_gateway,
        "msg":{"call_contract":{"destination_chain":"flow", "destination_address":cfg.asg_address, "payload":alloy::hex::encode(payload)}}, "funds":[]
    }]})
}

#[test]
fn recovery_preserves_original_timelock_payload() {
    let value = proposal(ProposalType::Timelock, 123456);
    let record: ExistingProposal = serde_json::from_value(value).unwrap();
    let payload = record
        .matching_payload(
            &config(String::new()),
            ProposalType::Timelock,
            Address::from([4; 20]),
            &vec![1, 2, 3].into(),
        )
        .unwrap();
    let (_, _, _, _, eta) =
        <(U256, Address, Bytes, U256, U256)>::abi_decode_params(&payload).unwrap();
    assert_eq!(eta, U256::from(123456));
}

#[test]
fn recovery_rejects_wrong_route_sender_type_target_calldata_and_extra_messages() {
    let cfg = config(String::new());
    let original = proposal(ProposalType::Operator, 0);
    let matching = |value| {
        let record: ExistingProposal = serde_json::from_value(value).unwrap();
        record.matching_payload(
            &cfg,
            ProposalType::Operator,
            Address::from([4; 20]),
            &vec![1, 2, 3].into(),
        )
    };
    assert!(matching(original.clone()).is_some());
    for pointer in [
        "/messages/0/sender",
        "/messages/0/contract",
        "/messages/0/@type",
        "/messages/0/msg/call_contract/destination_chain",
        "/messages/0/msg/call_contract/destination_address",
    ] {
        let mut changed = original.clone();
        *changed.pointer_mut(pointer).unwrap() = json!("wrong");
        assert!(matching(changed).is_none(), "{pointer}");
    }
    let mut extra = original.clone();
    extra["messages"]
        .as_array_mut()
        .unwrap()
        .push(original["messages"][0].clone());
    assert!(matching(extra).is_none());
    let record: ExistingProposal = serde_json::from_value(original).unwrap();
    assert!(
        record
            .matching_payload(
                &cfg,
                ProposalType::Timelock,
                Address::from([4; 20]),
                &vec![1, 2, 3].into()
            )
            .is_none()
    );
    assert!(
        record
            .matching_payload(
                &cfg,
                ProposalType::Operator,
                Address::from([5; 20]),
                &vec![1, 2, 3].into()
            )
            .is_none()
    );
    assert!(
        record
            .matching_payload(
                &cfg,
                ProposalType::Operator,
                Address::from([4; 20]),
                &vec![1, 2, 4].into()
            )
            .is_none()
    );
}

#[tokio::test]
async fn rerun_finds_existing_proposal_after_pagination() {
    let first = json!({"proposals":[],"pagination":{"next_key":"next+cursor="}});
    let second =
        json!({"proposals":[proposal(ProposalType::Operator,0)],"pagination":{"next_key":null}});
    let (url, task) = serve(vec![
        response("200 OK", &first.to_string()),
        response("200 OK", &second.to_string()),
    ])
    .await;
    let recovered = find(
        &config(url.to_string().trim_end_matches('/').into()),
        &args(),
        Address::from([4; 20]),
        &vec![1, 2, 3].into(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(recovered.0.number().unwrap(), 645);
    let requests = task.await.unwrap();
    assert!(requests.iter().all(|r| r.starts_with("GET ")));
    assert!(requests[1].contains("pagination.key=next%2Bcursor%3D"));
}

#[tokio::test]
async fn history_outage_does_not_allow_a_new_proposal() {
    let (url, task) = serve(vec![response("403 Forbidden", "{}")]).await;
    let result = find(
        &config(url.to_string().trim_end_matches('/').into()),
        &args(),
        Address::from([4; 20]),
        &vec![1, 2, 3].into(),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(task.await.unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_resume_cannot_fall_through_to_submit_a_mismatched_call() {
    let body = json!({"proposal":proposal(ProposalType::Operator,0)});
    let (url, task) = serve(vec![response("200 OK", &body.to_string())]).await;
    let mut args = args();
    args.proposal_id = Some(645);
    let result = find(
        &config(url.to_string().trim_end_matches('/').into()),
        &args,
        Address::from([5; 20]),
        &vec![1, 2, 3].into(),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(task.await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "read-only live testnet recovery of proposal 645; never signs or broadcasts"]
async fn recover_flow_645_and_locate_historical_message() {
    let source = crate::config_source::resolve(Network::Testnet, None)
        .await
        .unwrap();
    let config = crate::config::ChainsConfig::load(source.path())
        .await
        .unwrap();
    let cfg = helpers::resolve(Network::Testnet, &config, "flow").unwrap();
    let record = load(&cfg.lcd, 645).await.unwrap();
    assert_eq!(record.status, "PROPOSAL_STATUS_PASSED");
    let call = alloy::dyn_abi::DynSolValue::Tuple(vec![
        alloy::dyn_abi::DynSolValue::String("robinhood".into()),
        alloy::dyn_abi::DynSolValue::String("hub".into()),
    ]);
    let mut calldata =
        alloy::primitives::keccak256("setTrustedAddress(string,string)")[..4].to_vec();
    calldata.extend(call.abi_encode_params());
    let target = cfg.its_address.as_ref().unwrap().parse().unwrap();
    let mut args = args();
    args.proposal_id = Some(645);
    let (record, payload) = find(&cfg, &args, target, &calldata.into())
        .await
        .unwrap()
        .unwrap();
    let msg = crate::commands::propose::relay_message::find_governance_message(
        &cfg,
        alloy::primitives::keccak256(payload),
        record.execution_time().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(msg.source_address, cfg.gov_module);
    println!("Recovered proposal 645 message: {}", msg.message_id);
}
