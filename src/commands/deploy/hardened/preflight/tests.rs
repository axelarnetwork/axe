use serde_json::json;

use super::protocol_inputs;
use crate::http::tests::{response, serve};

const PROTOCOLS: [&str; 8] = [
    "Coordinator",
    "Router",
    "Rewards",
    "Multisig",
    "ServiceRegistry",
    "ChainCodecEvm",
    "ItsAbiTranslator",
    "InterchainTokenService",
];

#[tokio::test]
async fn protocol_checks_use_metadata_only_and_preserve_identity_after_truncated_response() {
    let checksum = "AB".repeat(32);
    let mut config = json!({"axelar":{"contracts":{}}});
    let mut responses = Vec::new();
    for name in PROTOCOLS {
        config["axelar"]["contracts"][name] = json!({"address":name});
        responses.push(response(
            "200 OK",
            &json!({"contract_info":{"code_id":"17","creator":"creator","admin":"governance"}})
                .to_string(),
        ));
        if name == "Coordinator" {
            responses.push(Some(
                "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{".into(),
            ));
        }
        responses.push(response(
            "200 OK",
            &json!({"code_id":"17","checksum":checksum}).to_string(),
        ));
    }
    let (url, task) = serve(responses).await;
    let protocols = protocol_inputs(&config, url.as_str().trim_end_matches('/'))
        .await
        .unwrap();
    assert_eq!(protocols.len(), 8);
    for name in PROTOCOLS {
        let identity = &protocols[name];
        assert_eq!(identity.address, name);
        assert_eq!(identity.code_id, "17");
        assert_eq!(identity.checksum, checksum);
        assert_eq!(identity.creator, "creator");
        assert_eq!(identity.admin, "governance");
    }
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 17);
    assert!(requests.iter().all(
        |request| request.starts_with("GET /cosmwasm/wasm/v1/contract/")
            || request.starts_with("GET /cosmwasm/wasm/v1/code-info/17 ")
    ));
}

#[tokio::test]
async fn invalid_metadata_cannot_be_pinned_as_a_protocol_identity() {
    for (id, checksum, expected) in [
        ("18", "AB".repeat(32), "code metadata ID"),
        ("17", "invalid".into(), "invalid code checksum"),
    ] {
        let (url, task) = serve(vec![
            response(
                "200 OK",
                r#"{"contract_info":{"code_id":"17","creator":"creator","admin":"governance"}}"#,
            ),
            response(
                "200 OK",
                &json!({"code_id":id,"checksum":checksum}).to_string(),
            ),
        ])
        .await;
        let config = json!({"axelar":{"contracts":{"Coordinator":{"address":"coordinator"}}}});
        let error = protocol_inputs(&config, url.as_str().trim_end_matches('/'))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected));
        assert_eq!(task.await.unwrap().len(), 2);
    }
}
