use super::{direct, types::Transaction};
use crate::types::Network;
use alloy::primitives::B256;
use cosmos_sdk_proto::cosmos::tx::v1beta1::{TxBody, TxRaw};
use prost::Message;

#[test]
fn only_devnet_skips_governance_and_requires_the_direct_authority() {
    assert!(!Network::DevnetAmplifier.deployment_uses_governance());
    for network in [Network::Mainnet, Network::Testnet, Network::Stagenet] {
        assert!(network.deployment_uses_governance());
    }
    direct::validate_authority("governor", "governor").unwrap();
    assert!(direct::validate_authority("proposer", "governor").is_err());
}

#[test]
fn devnet_cannot_replay_a_journal_with_governance_messages() {
    for (kind, valid) in [
        ("/cosmwasm.wasm.v1.MsgExecuteContract", true),
        ("/cosmos.gov.v1.MsgSubmitProposal", false),
    ] {
        let raw = TxRaw {
            body_bytes: TxBody {
                messages: vec![cosmos_sdk_proto::Any {
                    type_url: kind.into(),
                    value: vec![],
                }],
                ..Default::default()
            }
            .encode_to_vec(),
            ..Default::default()
        }
        .encode_to_vec();
        let tx = Transaction::Cosmos {
            raw,
            hash: String::new(),
            intent: B256::ZERO,
            sender: "governor".into(),
            fee: "1".into(),
            sequence: 7,
        };
        assert_eq!(direct::validate_submission(&tx).is_ok(), valid);
    }
}
