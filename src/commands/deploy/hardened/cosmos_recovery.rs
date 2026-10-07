use cosmos_sdk_proto::cosmos::tx::v1beta1::{AuthInfo, TxRaw};
use eyre::Result;
use prost::Message;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    journal, session,
    types::{Confirmation, Transaction},
};
use crate::cosmos::rpc::{lcd_broadcast_tx, lcd_query_account};

pub async fn resume(lcd: &str, saved: &Transaction, key: &str) -> Result<Value> {
    if let Some(response) = confirmed(lcd, saved, key).await? {
        return Ok(response);
    }
    let Transaction::Cosmos {
        raw,
        hash,
        sender,
        sequence,
        ..
    } = saved
    else {
        eyre::bail!("expected Cosmos transaction");
    };
    validate_sequence(raw, *sequence)?;
    let (_, current) = lcd_query_account(lcd, sender).await?;
    eyre::ensure!(
        current == *sequence,
        "Cosmos transaction {hash} is missing and account sequence is {current}, recorded {sequence}; use --recovery-lcd with a same-network archival LCD. A consumed sequence does not prove success; no transaction was resent"
    );
    if let Err(error) = lcd_broadcast_tx(lcd, raw).await {
        return Err(session::pause(format!(
            "{key}: recorded Cosmos transaction {hash} is unresolved: {error}. For insufficient fees use --bump-fees '{key}' --cosmos-fee <total-base-units>; other CheckTx failures require diagnosis before resuming"
        )));
    }
    for _ in 0..10 {
        if let Some(response) = confirmed(lcd, saved, key).await? {
            return Ok(response);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(session::pause(format!(
        "Waiting for confirmation of {key}: Cosmos transaction {hash}. Axe has saved the signed transaction but has not confirmed its outcome. Wait for a block, then run the continue command to check this transaction again"
    )))
}

/// Only attempts with the same account sequence compete. Earlier failed attempts
/// retried with a new sequence must not prevent recovery of the new transaction.
pub(super) async fn confirmed(lcd: &str, saved: &Transaction, key: &str) -> Result<Option<Value>> {
    let Transaction::Cosmos {
        sender, sequence, ..
    } = saved
    else {
        eyre::bail!("expected Cosmos action");
    };
    let mut pending = Vec::new();
    for attempt in journal::attempts(key, saved).await? {
        if let Transaction::Cosmos {
            raw,
            hash,
            sender: from,
            sequence: seq,
            ..
        } = attempt
            && from == *sender
            && seq == *sequence
        {
            eyre::ensure!(
                hex::encode_upper(Sha256::digest(&raw)) == hash,
                "Cosmos signed bytes do not match hash"
            );
            if let Some(Confirmation::Cosmos { response, .. }) =
                journal::confirmation(&hash).await?
            {
                return checked(response).map(Some);
            }
            pending.push(hash);
        }
    }
    for hash in &pending {
        if let Some(response) = lookup(lcd, hash).await? {
            return record(hash, response).await.map(Some);
        }
    }
    if let Some(archive) = recovery_lcd().await? {
        for hash in &pending {
            if let Some(response) = lookup(&archive, hash).await? {
                return record(hash, response).await.map(Some);
            }
        }
    }
    Ok(None)
}

pub(super) fn validate_sequence(raw: &[u8], expected: u64) -> Result<()> {
    let tx = TxRaw::decode(raw)?;
    let auth = AuthInfo::decode(tx.auth_info_bytes.as_slice())?;
    eyre::ensure!(
        auth.signer_infos.len() == 1 && auth.signer_infos[0].sequence == expected,
        "recorded Cosmos sequence differs from signed bytes"
    );
    Ok(())
}

async fn recovery_lcd() -> Result<Option<String>> {
    if !session::active() {
        return Ok(None);
    }
    let session = session::current()?;
    let Some(lcd) = &session.options.recovery_lcd else {
        return Ok(None);
    };
    let node: Value = crate::http::client()
        .get(format!(
            "{}/cosmos/base/tendermint/v1beta1/node_info",
            lcd.trim_end_matches('/')
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    eyre::ensure!(
        node.pointer("/default_node_info/network")
            .and_then(Value::as_str)
            == Some(session.plan.axelar_chain_id.as_str()),
        "recovery LCD is on a different network"
    );
    Ok(Some(lcd.clone()))
}

async fn lookup(lcd: &str, hash: &str) -> Result<Option<Value>> {
    let response = crate::http::client()
        .get(format!(
            "{}/cosmos/tx/v1beta1/txs/{hash}",
            lcd.trim_end_matches('/')
        ))
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    eyre::ensure!(
        response.status().is_success(),
        "cannot establish Cosmos transaction status"
    );
    Ok(Some(response.json().await?))
}

async fn record(hash: &str, response: Value) -> Result<Value> {
    let result = &response["tx_response"];
    eyre::ensure!(
        result["txhash"]
            .as_str()
            .is_some_and(|actual| actual.eq_ignore_ascii_case(hash)),
        "Cosmos receipt hash mismatch"
    );
    let height: u64 = result["height"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("missing confirmed Cosmos height"))?
        .parse()?;
    eyre::ensure!(height > 0, "Cosmos transaction is not included");
    let code = u32::try_from(
        result["code"]
            .as_u64()
            .ok_or_else(|| eyre::eyre!("missing Cosmos result code"))?,
    )?;
    journal::confirm(
        hash.into(),
        Confirmation::Cosmos {
            height,
            code,
            response: response.clone(),
        },
    )
    .await?;
    checked(response)
}

fn checked(response: Value) -> Result<Value> {
    eyre::ensure!(
        response
            .pointer("/tx_response/code")
            .and_then(Value::as_u64)
            == Some(0),
        "recorded Cosmos transaction failed: {}. A proven failed submission can be reviewed with --retry-failed <step/cosmos>; this does not retry a proposal that was created and later rejected",
        response["tx_response"]["raw_log"]
    );
    Ok(response)
}
