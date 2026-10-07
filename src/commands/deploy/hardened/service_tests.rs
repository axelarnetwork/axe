use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use base64::Engine;
use serde_json::{Value, json};

use super::{tests::initial_state, verifiers};
use crate::commands::deploy::DeployContext;
use crate::types::Network;

fn service_server(
    service: &str,
    chain: &str,
    response: &Value,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let query = json!({"service":{"service_name":service,"chain_name":chain}});
    let encoded = base64::engine::general_purpose::STANDARD.encode(query.to_string());
    let expected = format!("GET /cosmwasm/wasm/v1/contract/registry/smart/{encoded} HTTP/1.1");
    let body = json!({"data":response}).to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let size = stream.read(&mut request).unwrap();
        assert!(String::from_utf8_lossy(&request[..size]).starts_with(&expected));
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn every_network_writes_and_queries_the_same_correct_service() {
    for (env, expected) in [
        (Network::DevnetAmplifier, "validators"),
        (Network::Mainnet, "amplifier"),
        (Network::Testnet, "amplifier"),
        (Network::Stagenet, "amplifier"),
    ] {
        let directory = std::env::temp_dir().join(format!("axe-service-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        let mut state = initial_state();
        state.env = env;
        state.target_json = path.clone();
        state.predicted_gateway_address = Some(alloy::primitives::Address::repeat_byte(7));
        let chain = state.axelar_id.to_string();
        let config = json!({"chains":{&chain:{"axelarId":chain}}, "axelar":{
            "governanceAddress":"governance", "contracts":{"VotingVerifier":{},"MultisigProver":{}}
        }});
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let ctx = DeployContext {
            axelar_id: chain.clone(),
            rpc_url: state.rpc_url.clone(),
            target_json: path.clone(),
            state,
        };
        crate::steps::config_edit::run(&ctx).await.unwrap();
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for contract in ["VotingVerifier", "MultisigProver"] {
            assert_eq!(
                written["axelar"]["contracts"][contract][&chain]["serviceName"],
                expected
            );
        }
        let (lcd, server) = service_server(
            expected,
            &chain,
            &json!({"min_num_verifiers":1,"max_num_verifiers":100}),
        );
        let service = verifiers::service(&ctx.state, &lcd, "registry")
            .await
            .unwrap();
        assert_eq!(service.min_num_verifiers, 1);
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn missing_devnet_service_is_rejected_instead_of_falling_back_to_amplifier() {
    let mut state = initial_state();
    state.env = Network::DevnetAmplifier;
    let (lcd, server) = service_server("validators", state.axelar_id.as_str(), &Value::Null);
    let error = verifiers::service(&state, &lcd, "registry")
        .await
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("missing ServiceRegistry service validators")
    );
    server.join().unwrap();
}
