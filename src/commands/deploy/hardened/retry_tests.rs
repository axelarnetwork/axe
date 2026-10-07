use std::sync::Arc;

use alloy::{
    consensus::{SignableTransaction, TxEip1559, TxEnvelope},
    eips::Encodable2718,
    primitives::{Address, B256, TxKind, U256},
    providers::ProviderBuilder,
    rpc::{
        client::RpcClient,
        types::{Block, TransactionReceipt},
    },
    signers::{SignerSync, local::PrivateKeySigner},
    transports::mock::Asserter,
};
use cosmos_sdk_proto::cosmos::tx::v1beta1::{AuthInfo, SignerInfo, TxRaw};
use prost::Message;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    evm_recovery, evm_retry, recovery,
    recovery_tests::session as new_session,
    session,
    types::{Confirmation, EvmConfirmationPolicy, Transaction},
};

fn signed(signer: &PrivateKeySigner, nonce: u64, create: bool) -> TxEnvelope {
    let tx = TxEip1559 {
        chain_id: 1,
        nonce,
        gas_limit: 100_000,
        max_fee_per_gas: 10,
        max_priority_fee_per_gas: 1,
        to: if create {
            TxKind::Create
        } else {
            TxKind::Call(Address::repeat_byte(4))
        },
        ..Default::default()
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    tx.into_signed(signature).into()
}

fn recorded(signer: &PrivateKeySigner, nonce: u64, cost: &str) -> Transaction {
    let tx = signed(signer, nonce, false);
    Transaction::Evm {
        intent: B256::ZERO,
        raw: tx.encoded_2718().into(),
        hash: *tx.tx_hash(),
        sender: signer.address(),
        nonce,
        gas_cost: cost.into(),
    }
}

fn receipt(hash: B256, success: bool) -> TransactionReceipt {
    serde_json::from_value(json!({
        "type":"0x2", "status":if success {"0x1"} else {"0x0"}, "cumulativeGasUsed":"0x5208", "logs":[],
        "logsBloom":format!("0x{}", "00".repeat(256)), "transactionHash":hash,
        "blockHash":B256::repeat_byte(2), "blockNumber":"0xa", "gasUsed":"0x5208",
        "effectiveGasPrice":"0x1", "from":Address::ZERO, "to":null, "contractAddress":null
    })).unwrap()
}

#[tokio::test]
async fn only_a_finalized_failed_receipt_can_authorize_retirement() {
    for (success, height, canonical, expected) in [
        (true, 10, true, false),
        (false, 9, true, false),
        (false, 10, false, false),
        (false, 10, true, true),
    ] {
        let mut session = new_session().await;
        Arc::get_mut(&mut session).unwrap().options.evm_wait_seconds = 0;
        session.journal.lock().await.evm_confirmations =
            EvmConfirmationPolicy::Confirmations(std::num::NonZeroU64::new(1).unwrap());
        let saved = recorded(&PrivateKeySigner::random(), 7, "10");
        let Transaction::Evm { hash, .. } = saved else {
            unreachable!()
        };
        let mock = Asserter::new();
        mock.push_success(&receipt(hash, success));
        if !success {
            let mut block = Block::<alloy::rpc::types::Transaction>::default();
            block.header.inner.number = height;
            mock.push_success(&block);
            if height >= 10 {
                block.header.hash = B256::repeat_byte(if canonical { 2 } else { 3 });
                mock.push_success(&block);
            }
        }
        let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
        let result = session::scope(
            session.clone(),
            evm_retry::prove_with_provider(&provider, "Operators/call", &saved),
        )
        .await;
        assert_eq!(
            result.is_ok(),
            expected,
            "success={success}, height={height}, canonical={canonical}: {result:?}"
        );
        if height == 9 {
            assert!(result.unwrap_err().to_string().contains("finalized"));
        }
        std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
    }
}

#[tokio::test]
async fn retirement_survives_restart_preserves_spending_and_excludes_old_nonce_receipts() {
    let session = new_session().await;
    let signer = PrivateKeySigner::random();
    let key = "Operators/call";
    session
        .record(key.into(), recorded(&signer, 7, "10"))
        .await
        .unwrap();
    session::scope(session.clone(), recovery::retire(key, Some(200_000)))
        .await
        .unwrap();
    let restored = Arc::new(
        session::Session::load(
            session.path.clone(),
            B256::ZERO,
            session.plan.clone(),
            session.rpc.clone(),
        )
        .await
        .unwrap(),
    );
    let next = recorded(&signer, 8, "20");
    session::scope(restored.clone(), async {
        assert!(restored.get(key).await.is_none());
        assert_eq!(restored.journal.lock().await.retry_gas_limits[key], 200_000);
        evm_retry::validate_retry(key, &signed(&signer, 8, false))
            .await
            .unwrap();
        assert!(
            evm_retry::validate_retry(key, &signed(&signer, 7, false))
                .await
                .is_err()
        );
        assert!(
            evm_retry::validate_retry(key, &signed(&PrivateKeySigner::random(), 8, false))
                .await
                .is_err()
        );
        restored.record(key.into(), next.clone()).await.unwrap();
        assert_eq!(
            evm_recovery::competing_attempts(key, &next)
                .await
                .unwrap()
                .len(),
            1
        );
        let journal = restored.journal.lock().await;
        assert_eq!(
            super::evm_funding::liability(&journal, signer.address(), None).unwrap(),
            U256::from(30)
        );
        assert_eq!(
            super::evm_funding::liability(&journal, signer.address(), Some((8, U256::from(25))))
                .unwrap(),
            U256::from(35)
        );
    })
    .await;
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}

#[test]
fn gateway_retry_is_refused_and_other_retries_cannot_change_execution() {
    let signer = PrivateKeySigner::random();
    let saved = recorded(&signer, 7, "10");
    let error = evm_retry::validate_action("AxelarGateway/implementation", &saved).unwrap_err();
    assert!(error.to_string().contains("different address"));
    let old = signed(&signer, 7, false);
    evm_retry::validate_retry_fields(&old, &signed(&signer, 8, false)).unwrap();
    assert!(evm_retry::validate_retry_fields(&old, &signed(&signer, 7, false)).is_err());
    assert!(evm_retry::validate_retry_fields(&old, &signed(&signer, 8, true)).is_err());
}

#[tokio::test]
async fn a_failed_cosmos_submission_is_recoverable_but_not_a_failed_vote() {
    let session = new_session().await;
    let mut state = super::tests::initial_state();
    for step in &mut state.steps {
        if step.name == "InstantiateChainContracts" {
            break;
        }
        step.status = crate::state::StepStatus::Completed;
    }
    let mut ctx = crate::commands::deploy::DeployContext {
        axelar_id: state.axelar_id.to_string(),
        target_json: state.target_json.clone(),
        rpc_url: state.rpc_url.clone(),
        state,
    };
    let key = "InstantiateChainContracts/cosmos";
    recovery::validate_progress(&ctx, key).unwrap();
    ctx.state.proposals.insert("instantiate".into(), 42);
    assert!(recovery::validate_progress(&ctx, key).is_err());
    let raw = TxRaw {
        auth_info_bytes: AuthInfo {
            signer_infos: vec![SignerInfo {
                sequence: 7,
                ..Default::default()
            }],
            ..Default::default()
        }
        .encode_to_vec(),
        ..Default::default()
    }
    .encode_to_vec();
    let hash = hex::encode_upper(Sha256::digest(&raw));
    let tx = Transaction::Cosmos {
        raw,
        hash: hash.clone(),
        sender: "proposer".into(),
        sequence: 7,
        fee: "10".into(),
        intent: B256::ZERO,
    };
    session.record(key.into(), tx.clone()).await.unwrap();
    session::scope(session.clone(), async {
        assert!(recovery::failed_attempt(key, &tx).await.is_err());
        for (height, code, valid) in [(0, 11, false), (1, 0, false), (1, 11, true)] {
            super::journal::confirm(
                hash.clone(),
                Confirmation::Cosmos {
                    height,
                    code,
                    response: json!({}),
                },
            )
            .await
            .unwrap();
            assert_eq!(recovery::failed_attempt(key, &tx).await.is_ok(), valid);
        }
        recovery::retire(key, None).await.unwrap();
        recovery::validate_cosmos_retry(key, B256::ZERO, 8)
            .await
            .unwrap();
        assert!(
            recovery::validate_cosmos_retry(key, B256::ZERO, 7)
                .await
                .is_err()
        );
        assert!(
            recovery::validate_cosmos_retry(key, B256::repeat_byte(2), 8)
                .await
                .is_err()
        );
    })
    .await;
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}
