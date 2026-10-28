use alloy::{
    primitives::{Bytes, U256},
    sol_types::SolValue,
};
use serde_json::json;

use super::{messages::destination_payload, verify_balance};
use crate::commands::test_its::{encoding, resolve_config_route};
use crate::config::ChainsConfig;
use crate::types::ChainType;

#[test]
fn forwards_exact_deploy_and_transfer_payloads_for_selected_evm_pair() {
    let source = "unichain-sepolia-2".to_owned().into();
    let destination = "robinhood".to_owned().into();
    let inner_payloads = [
        encoding::encode_inner_deploy(&[7; 32], "Token", "TKN", 18, &[9; 20]),
        encoding::encode_inner_transfer(&[7; 32], &[8; 20], &[9; 20], 1_000_000, &[]),
    ];
    for inner in inner_payloads {
        let outbound = (
            U256::from(3),
            "robinhood".to_owned(),
            Bytes::from(inner.clone()),
        )
            .abi_encode_params();
        let forwarded = destination_payload(&outbound, &source, &destination).unwrap();
        let (kind, original_source, actual_inner) =
            <(U256, String, Bytes)>::abi_decode_params(&forwarded).unwrap();
        assert_eq!(kind, U256::from(4));
        assert_eq!(original_source, "unichain-sepolia-2");
        assert_eq!(actual_inner.as_ref(), inner);
    }
}

#[test]
fn rejects_wrong_hub_envelope_and_wrong_destination() {
    let source = "unichain-sepolia-2".to_owned().into();
    let destination = "robinhood".to_owned().into();
    for (kind, target) in [(4, "robinhood"), (3, "hedera")] {
        let outbound =
            (U256::from(kind), target.to_owned(), Bytes::from(vec![1])).abi_encode_params();
        assert!(destination_payload(&outbound, &source, &destination).is_err());
    }
    assert!(destination_payload(&[0; 31], &source, &destination).is_err());
}

fn config(source_type: &str, destination_type: &str) -> ChainsConfig {
    let address = "0x1111111111111111111111111111111111111111";
    ChainsConfig::from_json_str(&json!({
        "chains": {
            "source-key": {"axelarId":"source-id", "chainType":source_type,"rpc":"http://localhost:1"},
            "destination-key": {"axelarId":"destination-id", "chainType":destination_type,"tokenSymbol":"ETH","rpc":"http://localhost:2", "contracts":{
                "InterchainTokenService":{"address":address}, "AxelarGateway":{"address":address}
            }}
        },
        "axelar":{"contracts":{
            "Gateway":{"source-id":{"address":"source-gateway"},"destination-id":{"address":"destination-gateway"}},
            "VotingVerifier":{"source-id":{"address":"verifier"}},
            "MultisigProver":{"destination-id":{"address":"prover"}},
            "AxelarnetGateway":{"address":"hub-gateway"},
            "InterchainTokenService":{"address":"hub"}
        }}
    }).to_string()).unwrap()
}

#[test]
fn routes_evm_pairs_by_config_and_preserves_solana_support() {
    for (source, expected) in [("evm", ChainType::Evm), ("svm", ChainType::Svm)] {
        let route = resolve_config_route(
            &config(source, "evm"),
            Some("source-key".into()),
            Some("destination-key".into()),
        )
        .unwrap();
        assert_eq!(route.source_type, expected);
        assert_eq!(route.source_axelar_id.as_str(), "source-id");
        assert_eq!(route.destination_axelar_id.as_str(), "destination-id");
        assert_eq!(route.destination_cosm_gateway, "destination-gateway");
        assert_eq!(route.destination_multisig_prover, "prover");
    }
    assert!(
        resolve_config_route(
            &config("evm", "svm"),
            Some("source-key".into()),
            Some("destination-key".into())
        )
        .is_err()
    );
    assert!(
        resolve_config_route(
            &config("sui", "evm"),
            Some("source-key".into()),
            Some("destination-key".into())
        )
        .is_err()
    );
}

#[test]
fn requires_exact_transfer_delta_without_saturating_underflow() {
    assert!(verify_balance(U256::from(10), U256::from(17), U256::from(7)).is_ok());
    for after in [0, 10, 16, 18] {
        assert!(verify_balance(U256::from(10), U256::from(after), U256::from(7)).is_err());
    }
    assert!(verify_balance(U256::MAX, U256::ZERO, U256::from(1)).is_err());
}

#[test]
fn receipt_checks_reject_reverts_and_missing_confirmation_blocks() {
    let mut receipt: alloy::rpc::types::TransactionReceipt = serde_json::from_value(json!({
        "status":"0x1","cumulativeGasUsed":"0x5208","logs":[],
        "logsBloom":format!("0x{}", "00".repeat(256)),
        "transactionHash":format!("0x{}", "11".repeat(32)),
        "transactionIndex":"0x0","blockHash":format!("0x{}", "22".repeat(32)),
        "blockNumber":"0x2a", "gasUsed":"0x5208", "effectiveGasPrice":"0x1",
        "from":"0x1111111111111111111111111111111111111111",
        "to":"0x2222222222222222222222222222222222222222","type":"0x2"
    }))
    .unwrap();
    assert_eq!(
        crate::commands::test_its::relay::confirmed_block(&receipt).unwrap(),
        42
    );
    receipt.block_number = None;
    assert!(crate::commands::test_its::relay::confirmed_block(&receipt).is_err());
    let mut reverted = serde_json::to_value(&receipt).unwrap();
    reverted["blockNumber"] = json!("0x2a");
    reverted["status"] = json!("0x0");
    let reverted = serde_json::from_value(reverted).unwrap();
    assert!(crate::commands::test_its::relay::confirmed_block(&reverted).is_err());
}
