use std::sync::Arc;

use alloy::primitives::{Address, B256, Bytes, keccak256};
use alloy::providers::ProviderBuilder;
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{Block, TransactionReceipt};
use alloy::transports::mock::Asserter;
use clap::Parser;
use serde_json::{Value, json};

use super::{
    confirmations, evm_recovery, journal, session,
    tests::example_plan,
    types::{Confirmation, EvmConfirmationPolicy, Journal, Transaction},
};
use crate::cli::{Cli, Commands, DeployCommands};

async fn session() -> Arc<session::Session> {
    let path = std::env::temp_dir()
        .join(format!("axe-confirmations-{}", rand::random::<u64>()))
        .join("journal.json");
    Arc::new(
        session::Session::load(path, B256::ZERO, example_plan(), "unused".into())
            .await
            .unwrap(),
    )
}

#[test]
fn cli_accepts_positive_depth_or_finality_only() {
    for value in ["1", "2", "12", "finalized"] {
        let cli =
            Cli::try_parse_from(["axe", "deploy", "run", "--evm-confirmations", value]).unwrap();
        let Commands::Deploy {
            subcommand: DeployCommands::Run {
                deployment_options, ..
            },
        } = cli.command
        else {
            panic!("expected deploy run");
        };
        assert_eq!(
            deployment_options.evm_confirmations.unwrap().to_string(),
            value
        );
    }
    for value in ["0", "-1", "safe", "", "18446744073709551616"] {
        assert!(
            Cli::try_parse_from(["axe", "deploy", "run", "--evm-confirmations", value]).is_err()
        );
    }
}

#[tokio::test]
async fn journal_preserves_policy_and_missing_field_preserves_old_finality() {
    let first = session().await;
    assert_eq!(
        first.journal.lock().await.evm_confirmations.to_string(),
        "1"
    );
    for value in ["12", "finalized", "1"] {
        let policy = value.parse().unwrap();
        confirmations::persist(&first, policy).await.unwrap();
        let restored = session::Session::load(
            first.path.clone(),
            B256::ZERO,
            example_plan(),
            "unused".into(),
        )
        .await
        .unwrap();
        assert_eq!(restored.journal.lock().await.evm_confirmations, policy);
    }
    let mut old: Value = serde_json::from_slice(&std::fs::read(&first.path).unwrap()).unwrap();
    assert!(
        old.as_object_mut()
            .unwrap()
            .remove("evmConfirmations")
            .is_some()
    );
    let restored: Journal = serde_json::from_value(old).unwrap();
    assert_eq!(restored.evm_confirmations, EvmConfirmationPolicy::Finalized);
    std::fs::remove_dir_all(first.path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn gateway_observation_uses_selected_confirmation_depth() {
    let session = session().await;
    for (policy, expected) in [("1", 20), ("3", 18), ("finalized", 20)] {
        confirmations::persist(&session, policy.parse().unwrap())
            .await
            .unwrap();
        let mock = Asserter::new();
        let mut block = Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = 20;
        mock.push_success(&block);
        if expected != 20 {
            block.header.inner.number = expected;
            mock.push_success(&block);
        }
        let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
        let observed = session::scope(session.clone(), confirmations::observation_block(&provider))
            .await
            .unwrap();
        assert_eq!(observed.header.number, expected);
    }
    assert_eq!(
        EvmConfirmationPolicy::default().block_tag(),
        alloy::eips::BlockNumberOrTag::Latest
    );
    assert_eq!(
        EvmConfirmationPolicy::Finalized.block_tag(),
        alloy::eips::BlockNumberOrTag::Finalized
    );
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}

fn receipt(hash: B256) -> TransactionReceipt {
    serde_json::from_value(json!({
        "type":"0x2", "status":"0x1", "cumulativeGasUsed":"0x5208", "logs":[],
        "logsBloom":format!("0x{}", "00".repeat(256)), "transactionHash":hash,
        "blockHash":B256::repeat_byte(2), "blockNumber":"0xa", "gasUsed":"0x5208",
        "effectiveGasPrice":"0x1", "from":Address::ZERO, "to":null, "contractAddress":null
    }))
    .unwrap()
}

#[tokio::test]
async fn cached_receipts_are_rechecked_after_restart_and_reorgs_keep_the_original_nonce() {
    for (reorganized, reappeared) in [(false, false), (true, true), (true, false)] {
        let first = session().await;
        let raw = Bytes::from(vec![1, 2, 3]);
        let hash = keccak256(&raw);
        let saved = Transaction::Evm {
            intent: B256::ZERO,
            raw,
            hash,
            sender: Address::ZERO,
            nonce: 4,
            gas_cost: "1".into(),
        };
        first
            .record("test/call".into(), saved.clone())
            .await
            .unwrap();
        let mut receipt = receipt(hash);
        session::scope(
            first.clone(),
            journal::confirm(
                hash.to_string(),
                Confirmation::Evm {
                    receipt: Box::new(receipt.clone()),
                },
            ),
        )
        .await
        .unwrap();
        let restored = Arc::new(
            session::Session::load(
                first.path.clone(),
                B256::ZERO,
                example_plan(),
                "unused".into(),
            )
            .await
            .unwrap(),
        );
        let mock = Asserter::new();
        let mut block = Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = 10;
        block.header.hash = B256::repeat_byte(if reorganized { 3 } else { 2 });
        mock.push_success(&block); // recheck the cached block, even after restart
        if reorganized {
            if reappeared {
                receipt.block_number = Some(11);
                receipt.block_hash = Some(B256::repeat_byte(4));
                block.header.inner.number = 11;
                block.header.hash = B256::repeat_byte(4);
                mock.push_success(&receipt);
            } else {
                mock.push_success(&Value::Null);
                mock.push_success(&"0x5"); // consumed nonce: fail closed, never send
            }
        }
        if !reorganized || reappeared {
            mock.push_success(&block); // confirmation head
            mock.push_success(&block); // canonical receipt block
        }
        let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
        let result = session::scope(
            restored.clone(),
            evm_recovery::resume(&provider, &saved, B256::ZERO, "test/call"),
        )
        .await;
        if reorganized && !reappeared {
            assert!(result.unwrap_err().to_string().contains("nonce consumed"));
        } else {
            let result = result.unwrap();
            assert_eq!(result.transaction_hash, hash);
            assert_eq!(result.block_number, receipt.block_number);
            let confirmed = restored.journal.lock().await.confirmations[&hash.to_string()].clone();
            let Confirmation::Evm { receipt: cached } = confirmed else {
                panic!("expected EVM receipt")
            };
            assert_eq!(cached.block_hash, receipt.block_hash);
        }
        // No send response exists in the mock. Any unexpected broadcast fails the test.
        std::fs::remove_dir_all(first.path.parent().unwrap()).unwrap();
    }
}
