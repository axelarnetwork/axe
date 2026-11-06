use chrono::{Duration, TimeZone};
use cosmos_sdk_proto::cosmwasm::wasm::v1::MsgExecuteContract;
use serde_json::json;

use super::*;
use crate::commands::propose::test_support::config;
use crate::cosmos::{build_execute_msg_any, build_submit_proposal_any};
use crate::http::tests::{response, serve};

const SIGNER: &str = "axelar1ykl3n6xjh00nyynknr4yzxvhtwwkd5vhcwrxye";
const DEPOSIT: u128 = 400_000_000_000;

fn grant(expiration: Option<&str>) -> serde_json::Value {
    json!({
        "authorization": {
            "@type": "/cosmos.authz.v1beta1.GenericAuthorization",
            "msg": SUBMIT_PROPOSAL
        },
        "expiration": expiration
    })
}

fn balance(amount: u128) -> Option<String> {
    response(
        "200 OK",
        &json!({"balance": {"denom": "uaxl", "amount": amount.to_string()}}).to_string(),
    )
}

fn grants() -> Option<String> {
    response("200 OK", &json!({"grants": [grant(None)]}).to_string())
}

#[test]
fn only_unexpired_proposal_grants_are_accepted() {
    let now = Utc.with_ymd_and_hms(2026, 11, 2, 0, 0, 0).unwrap();
    for (expiration, expected) in [
        (None, true),
        (Some((now + Duration::seconds(1)).to_rfc3339()), true),
        (Some(now.to_rfc3339()), false),
        (Some((now - Duration::seconds(1)).to_rfc3339()), false),
    ] {
        let parsed =
            serde_json::from_value(json!({"grants": [grant(expiration.as_deref())]})).unwrap();
        assert_eq!(has_active_grant(&parsed, now).unwrap(), expected);
    }
    let mut wrong_message = grant(None);
    wrong_message["authorization"]["msg"] = json!("/cosmos.bank.v1beta1.MsgSend");
    let mut unsupported = grant(None);
    unsupported["authorization"]["@type"] = json!("/cosmos.bank.v1beta1.SendAuthorization");
    for entries in [json!([]), json!([wrong_message]), json!([unsupported])] {
        let parsed = serde_json::from_value(json!({"grants": entries})).unwrap();
        assert!(!has_active_grant(&parsed, now).unwrap());
    }
    let invalid = serde_json::from_value(json!({"grants": [grant(Some("bad-date"))]})).unwrap();
    assert!(has_active_grant(&invalid, now).is_err());
}

#[tokio::test]
async fn funded_signer_uses_own_deposit_without_querying_grants() {
    let (url, task) = serve(vec![balance(DEPOSIT + GAS_BUFFER_UAXL)]).await;
    let cfg = config(url.to_string());
    let funding = resolve(Network::Mainnet, &cfg, SIGNER, DEPOSIT)
        .await
        .unwrap();
    assert!(matches!(funding, ProposalFunding::Direct { .. }));
    assert_eq!(funding.proposer().as_ref(), SIGNER);
    assert_eq!(task.await.unwrap().len(), 1);
}

#[tokio::test]
async fn underfunded_mainnet_signer_uses_cpi_for_deposit_only() {
    let (url, task) = serve(vec![balance(GAS_BUFFER_UAXL), grants(), balance(DEPOSIT)]).await;
    let mut cfg = config(url.to_string());
    cfg.chain_id = "axelar-dojo-1".into();
    let funding = resolve(Network::Mainnet, &cfg, SIGNER, DEPOSIT)
        .await
        .unwrap();
    assert_eq!(funding.signer().as_ref(), SIGNER);
    assert_eq!(funding.proposer().as_ref(), CPI_MULTISIG);
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains(&format!("/balances/{SIGNER}/")));
    let request_url: reqwest::Url = format!(
        "http://localhost{}",
        requests[1].split_whitespace().nth(1).unwrap()
    )
    .parse()
    .unwrap();
    let pairs: std::collections::HashMap<_, _> = request_url.query_pairs().collect();
    assert_eq!(pairs["granter"], CPI_MULTISIG);
    assert_eq!(pairs["grantee"], SIGNER);
    assert_eq!(pairs["msg_type_url"], SUBMIT_PROPOSAL);
    assert!(requests[2].contains(&format!("/balances/{CPI_MULTISIG}/")));
    assert!(requests.iter().all(|r| r.starts_with("GET ")));
}

#[tokio::test]
async fn fallback_requires_mainnet_and_signer_gas() {
    for (network, chain_id, signer_balance) in [
        (Network::Testnet, "axelar-testnet-lisbon-3", GAS_BUFFER_UAXL),
        (Network::Mainnet, "axelar-testnet-lisbon-3", GAS_BUFFER_UAXL),
        (Network::Mainnet, "axelar-dojo-1", GAS_BUFFER_UAXL - 1),
    ] {
        let (url, task) = serve(vec![balance(signer_balance)]).await;
        let mut cfg = config(url.to_string());
        cfg.chain_id = chain_id.into();
        assert!(resolve(network, &cfg, SIGNER, DEPOSIT).await.is_err());
        assert_eq!(task.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn missing_or_unreadable_grant_stops_before_checking_cpi_funds() {
    for grant_response in [
        response("200 OK", r#"{"grants":[]}"#),
        response("200 OK", r#"{"unexpected":"response"}"#),
        response("403 Forbidden", "{}"),
    ] {
        let (url, task) = serve(vec![balance(GAS_BUFFER_UAXL), grant_response]).await;
        let mut cfg = config(url.to_string());
        cfg.chain_id = "axelar-dojo-1".into();
        assert!(
            resolve(Network::Mainnet, &cfg, SIGNER, DEPOSIT)
                .await
                .is_err()
        );
        assert_eq!(task.await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn cpi_must_have_the_full_deposit() {
    let (url, task) = serve(vec![
        balance(GAS_BUFFER_UAXL),
        grants(),
        balance(DEPOSIT - 1),
    ])
    .await;
    let mut cfg = config(url.to_string());
    cfg.chain_id = "axelar-dojo-1".into();
    let error = resolve(Network::Mainnet, &cfg, SIGNER, DEPOSIT)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("CPI multisig"));
    assert_eq!(task.await.unwrap().len(), 3);
}

fn proposal(proposer: &str) -> Any {
    let inner =
        build_execute_msg_any("gov-module", "gateway", &json!({"call_contract": {}})).unwrap();
    build_submit_proposal_any(
        proposer,
        vec![inner],
        "Test proposal",
        "Test summary",
        &DEPOSIT.to_string(),
        "uaxl",
        true,
    )
    .unwrap()
}

#[test]
fn authz_changes_deposit_payer_but_preserves_governance_messages() {
    let funding = ProposalFunding::Authorized {
        signer: SIGNER.parse().unwrap(),
        granter: CPI_MULTISIG.parse().unwrap(),
    };
    let original = proposal(CPI_MULTISIG);
    let original_bytes = original.value.clone();
    let wrapped = submission(&funding, original).unwrap();
    assert_eq!(wrapped.type_url, "/cosmos.authz.v1beta1.MsgExec");
    let exec = MsgExec::decode(wrapped.value.as_slice()).unwrap();
    assert_eq!(exec.grantee, SIGNER);
    assert_eq!(exec.msgs.len(), 1);
    assert_eq!(exec.msgs[0].type_url, SUBMIT_PROPOSAL);
    assert_eq!(exec.msgs[0].value, original_bytes);
    let submit = MsgSubmitProposal::decode(exec.msgs[0].value.as_slice()).unwrap();
    assert_eq!(submit.proposer, CPI_MULTISIG);
    assert_eq!(submit.initial_deposit[0].amount, DEPOSIT.to_string());
    assert!(submit.expedited);
    let inner = MsgExecuteContract::decode(submit.messages[0].value.as_slice()).unwrap();
    assert_eq!(inner.sender, "gov-module");
    assert_eq!(inner.contract, "gateway");
    assert!(inner.funds.is_empty());
}

#[test]
fn direct_submission_is_unchanged_and_mismatched_payers_are_rejected() {
    let funding = ProposalFunding::Direct {
        signer: SIGNER.parse().unwrap(),
    };
    let original = proposal(SIGNER);
    let original_bytes = original.value.clone();
    let submitted = submission(&funding, original).unwrap();
    assert_eq!(submitted.type_url, SUBMIT_PROPOSAL);
    assert_eq!(submitted.value, original_bytes);
    assert!(submission(&funding, proposal(CPI_MULTISIG)).is_err());
    let mut invalid = proposal(SIGNER);
    invalid.type_url = "/cosmos.bank.v1beta1.MsgSend".into();
    assert!(submission(&funding, invalid).is_err());
}
