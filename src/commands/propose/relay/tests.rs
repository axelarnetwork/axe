mod rpc;

use std::time::Duration;

use alloy::{
    primitives::{Address, Bytes, U256, keccak256},
    providers::ProviderBuilder,
    sol_types::SolCall,
};
use serde_json::{Value, json};

use super::relay_with_provider;
use crate::commands::propose::{
    helpers,
    test_support::config,
    types::{ProposalType, RelayPlan},
};
use crate::evm::AxelarServiceGovernance;
use crate::http::tests::{response, serve};

async fn resume(ptype: ProposalType, consumed: bool, pending: bool) -> Vec<Value> {
    let edge = rpc::serve(consumed, pending).await;
    let mut cfg = config(String::new());
    cfg.edge_rpc = edge.url.clone();
    let target = Address::from([4; 20]);
    let calldata: Bytes = vec![1, 2, 3].into();
    let plan = RelayPlan {
        ptype,
        target,
        payload: helpers::encode_governance_payload(ptype.command(), target, calldata.clone(), 0),
        calldata,
    };
    let event = json!({"type":"wasm-contract_called", "attributes":[
        {"key":"_contract_address","value":cfg.axelarnet_gateway},
        {"key":"payload_hash","value":format!("{:x}", alloy::primitives::keccak256(&plan.payload))},
        {"key":"message_id","value":"governance-message-645"},
        {"key":"source_chain","value":"axelar"},
        {"key":"source_address","value":cfg.gov_module},
        {"key":"destination_chain","value":cfg.edge_axelar_id},
        {"key":"destination_address","value":cfg.asg_address}
    ]});
    let (hub, hub_task) = serve(
        [
            json!({"result":{"sync_info":{"latest_block_height":"2"}}}),
            json!({"result":{"block":{"header":{"time":"2026-09-29T16:03:25Z"}}}}),
            json!({"result":{"finalize_block_events":[event]}}),
        ]
        .iter()
        .map(|body| response("200 OK", &body.to_string()))
        .collect(),
    )
    .await;
    cfg.axelar_rpc = hub.to_string();
    let operator = Address::from([1; 20]);
    let asg = helpers::AsgInfo {
        operator,
        governance_chain: "axelar".into(),
        governance_address: cfg.gov_module.clone(),
        minimum_time_lock_delay: 300,
    };
    let time = chrono::DateTime::parse_from_rfc3339("2026-09-29T16:03:24Z").unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        relay_with_provider(&cfg, &asg, &plan, time, || {
            // No signer or environment keys. Record real RPC requests against the local fixture.
            let provider = ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect_http(edge.url.parse().unwrap());
            Ok((provider, operator))
        }),
    )
    .await;
    let requests = tokio::time::timeout(Duration::from_secs(2), edge.finish())
        .await
        .unwrap();
    let hub_requests = tokio::time::timeout(Duration::from_secs(2), hub_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hub_requests.len(), 3);
    result.unwrap().unwrap();
    requests
}

fn sent_calls(requests: &[Value]) -> Vec<Vec<u8>> {
    assert!(
        requests
            .iter()
            .all(|request| request["method"] != "eth_sendRawTransaction")
    );
    requests
        .iter()
        .filter(|request| request["method"] == "eth_sendTransaction")
        .map(|request| {
            let transaction = &request["params"][0];
            assert_eq!(
                transaction["to"]
                    .as_str()
                    .unwrap()
                    .parse::<Address>()
                    .unwrap(),
                Address::from([2; 20])
            );
            rpc::input(transaction)
        })
        .collect()
}

#[tokio::test]
async fn approved_message_skips_proof_then_consumes_and_executes() {
    let requests = resume(ProposalType::Operator, false, false).await;
    let calls = sent_calls(&requests);
    assert_eq!(calls.len(), 2);
    let consume = AxelarServiceGovernance::executeCall::abi_decode(&calls[0]).unwrap();
    assert_eq!(consume.sourceChain, "axelar");
    assert_eq!(consume.sourceAddress, "axelar1gov");
    assert_eq!(
        consume.commandId,
        keccak256("axelar_governance-message-645")
    );
    assert_eq!(
        consume.payload,
        helpers::encode_governance_payload(2, Address::from([4; 20]), vec![1, 2, 3].into(), 0)
    );
    let execution =
        AxelarServiceGovernance::executeOperatorProposalCall::abi_decode(&calls[1]).unwrap();
    assert_eq!(execution.target, Address::from([4; 20]));
    assert_eq!(execution.callData.as_ref(), &[1, 2, 3]);
    assert_eq!(execution.nativeValue, U256::ZERO);
}

#[tokio::test]
async fn consumed_operator_message_only_executes_the_pending_call() {
    let requests = resume(ProposalType::Operator, true, true).await;
    let calls = sent_calls(&requests);
    assert_eq!(calls.len(), 1);
    let execution =
        AxelarServiceGovernance::executeOperatorProposalCall::abi_decode(&calls[0]).unwrap();
    assert_eq!(execution.target, Address::from([4; 20]));
    assert_eq!(execution.callData.as_ref(), &[1, 2, 3]);
    assert_eq!(execution.nativeValue, U256::ZERO);
}

#[tokio::test]
async fn consumed_timelock_only_executes_the_existing_schedule() {
    let requests = resume(ProposalType::Timelock, true, true).await;
    let calls = sent_calls(&requests);
    assert_eq!(calls.len(), 1);
    let execution = AxelarServiceGovernance::executeProposalCall::abi_decode(&calls[0]).unwrap();
    assert_eq!(execution.target, Address::from([4; 20]));
    assert_eq!(execution.callData.as_ref(), &[1, 2, 3]);
    assert_eq!(execution.nativeValue, U256::ZERO);
}

#[tokio::test]
async fn completed_timelock_sends_no_transactions() {
    let requests = resume(ProposalType::Timelock, true, false).await;
    assert!(sent_calls(&requests).is_empty());
    assert!(
        requests
            .iter()
            .all(|request| request["method"] == "eth_call")
    );
}
