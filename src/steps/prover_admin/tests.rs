use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use base64::Engine;
use bip32::{Language, Mnemonic};
use serde_json::json;

use super::{default_address, planned_address, query_admin, select_mnemonic, validate};
use crate::cosmos::derive_axelar_wallet;
use crate::state::{State, StepStatus, default_steps};
use crate::types::Network;

fn mnemonic(seed: u8) -> String {
    Mnemonic::from_entropy([seed; 32], Language::English)
        .phrase()
        .to_string()
}

#[test]
fn matching_deployer_mnemonic_supplies_admin_without_a_second_secret() {
    let phrase = mnemonic(42);
    let (_, address) = derive_axelar_wallet(&phrase).unwrap();
    assert_eq!(select_mnemonic(None, &phrase, &address).unwrap(), phrase);
    assert_eq!(
        select_mnemonic(Some(" "), &phrase, &address).unwrap(),
        phrase
    );
}

#[test]
fn explicit_admin_must_match_even_if_deployer_matches() {
    let admin = mnemonic(42);
    let other = mnemonic(43);
    let (_, expected) = derive_axelar_wallet(&admin).unwrap();
    assert_eq!(
        select_mnemonic(Some(&admin), &other, &expected).unwrap(),
        admin
    );
    let error = select_mnemonic(Some(&other), &admin, &expected)
        .unwrap_err()
        .to_string();
    assert!(error.contains("MULTISIG_PROVER_MNEMONIC"));
    assert!(error.contains(&expected));
    assert!(!error.contains(&admin));
    assert!(!error.contains(&other));
}

#[test]
fn missing_admin_with_unrelated_deployer_fails_with_expected_address() {
    let phrase = mnemonic(42);
    let expected = default_address(Network::Testnet);
    let error = select_mnemonic(None, &phrase, expected)
        .unwrap_err()
        .to_string();
    assert!(error.contains(expected));
    assert!(error.contains("Set MULTISIG_PROVER_MNEMONIC"));
    assert!(!error.contains(&phrase));
}

#[test]
fn malformed_mnemonic_fails_without_disclosing_it() {
    let invalid = "sensitive invalid input";
    for admin in [None, Some(invalid)] {
        let error = select_mnemonic(admin, invalid, default_address(Network::Testnet))
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot derive"));
        assert!(!error.contains(invalid));
    }
}

#[test]
fn planned_admin_matches_testnet_override_and_other_network_configs() {
    assert_eq!(
        planned_address(Network::Testnet, Some("old-config-admin")).unwrap(),
        "axelar1w7y7v26rtnrj4vrx6q3qq4hfsmc68hhsxnadlf"
    );
    for network in [
        Network::DevnetAmplifier,
        Network::Mainnet,
        Network::Stagenet,
    ] {
        assert_eq!(
            planned_address(network, Some("configured-admin")).unwrap(),
            "configured-admin"
        );
        assert!(planned_address(network, None).is_err());
        assert!(planned_address(network, Some(" ")).is_err());
    }
}

#[tokio::test]
async fn completed_verifier_step_needs_no_admin_or_network_access() {
    let mut state: State = serde_json::from_value(json!({
        "axelarId": "test-chain", "rpcUrl": "", "targetJson": "/nonexistent/config.json",
        "mnemonic": "", "env": "testnet", "cosmSalt": "test", "steps": default_steps()
    }))
    .unwrap();
    state
        .steps
        .iter_mut()
        .find(|step| step.name == "WaitForVerifierSet")
        .unwrap()
        .status = StepStatus::Completed;
    validate(&mut state).await.unwrap();
    assert!(state.admin_mnemonic.is_none());
}

fn serve_raw_response(status: &str, data: &str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let body = json!({"data": data}).to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let n = stream.read(&mut request).unwrap();
        let key = base64::engine::general_purpose::STANDARD
            .encode("permission_control_contract_admin_addr");
        assert!(String::from_utf8_lossy(&request[..n]).contains(&format!("/raw/{key}")));
        stream.write_all(response.as_bytes()).unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn reads_operational_admin_from_contract_storage() {
    let expected = default_address(Network::Testnet);
    let data = base64::engine::general_purpose::STANDARD.encode(json!(expected).to_string());
    let (lcd, server) = serve_raw_response("200 OK", &data);
    assert_eq!(query_admin(&lcd, "prover").await.unwrap(), expected);
    server.join().unwrap();
}

#[tokio::test]
async fn failed_or_empty_admin_queries_do_not_fall_back_to_config() {
    for (status, data) in [("500 Internal Server Error", ""), ("200 OK", "")] {
        let (lcd, server) = serve_raw_response(status, data);
        assert!(query_admin(&lcd, "prover").await.is_err());
        server.join().unwrap();
    }
}
