use alloy::primitives::keccak256;
use cosmrs::crypto::secp256k1::SigningKey;
use eyre::Result;
use prost::Message;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::session::{self, pause};
use super::types::Transaction;
use crate::cosmos::rpc::{lcd_query_account, lcd_simulate_tx};
use crate::cosmos::tx::{CosmosTxSignInput, build_and_sign_cosmos_tx};
use crate::ui;

pub async fn send(
    signing_key: &SigningKey,
    address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    messages: Vec<cosmrs::Any>,
) -> Result<Value> {
    eyre::ensure!(
        gas_price.is_finite() && gas_price > 0.0,
        "invalid gas price"
    );
    let session = session::current()?;
    eyre::ensure!(
        chain_id == session.plan.axelar_chain_id,
        "Axelar network mismatch"
    );
    let intent_bytes: Vec<_> = messages
        .iter()
        .map(|msg| (msg.type_url.clone(), hex::encode(&msg.value)))
        .collect();
    let intent = keccak256(serde_json::to_vec(&(
        address,
        chain_id,
        fee_denom,
        &intent_bytes,
    ))?);
    let key = session::action_key("cosmos")?;
    if let Some(saved) = session.get(&key).await {
        let Transaction::Cosmos {
            intent: expected, ..
        } = saved
        else {
            eyre::bail!("wrong journal transaction type");
        };
        eyre::ensure!(
            expected == intent,
            "Cosmos transaction intent changed; refusing replacement"
        );
        return resume(lcd, &saved, &key).await;
    }
    let (account_number, sequence) = lcd_query_account(lcd, address).await?;
    super::recovery::validate_cosmos_retry(&key, intent, sequence).await?;
    let mut input = CosmosTxSignInput {
        chain_id,
        account_number,
        sequence,
        gas_limit: 10_000_000,
        fee_amount: 0,
        fee_denom,
        messages,
    };
    let simulation = build_and_sign_cosmos_tx(signing_key, &input)?;
    let used = lcd_simulate_tx(lcd, &unsigned_simulation(&simulation)?).await?;
    input.gas_limit = used
        .checked_mul(3)
        .ok_or_else(|| eyre::eyre!("gas overflow"))?;
    input.fee_amount = (input.gas_limit as f64 * gas_price).ceil() as u128;
    super::cosmos_funding::check_fee_budget(address, sequence, input.fee_amount).await?;
    if !super::cosmos_funding::approve(
        lcd,
        chain_id,
        fee_denom,
        address,
        &input.messages,
        input.fee_amount,
    )
    .await?
    {
        return Err(pause(
            "Transaction declined; it was not submitted. Run the continue command when you are ready to review and approve it.",
        ));
    }
    let raw = build_and_sign_cosmos_tx(signing_key, &input)?;
    let hash = hex::encode_upper(Sha256::digest(&raw));
    let saved = Transaction::Cosmos {
        intent,
        raw,
        hash,
        sender: address.into(),
        fee: input.fee_amount.to_string(),
        sequence,
    };
    session.record(key.clone(), saved.clone()).await?;
    resume(lcd, &saved, &key).await
}

pub(super) use super::cosmos_recovery::resume;

pub(super) fn preview(message: &cosmrs::Any) -> Result<()> {
    match message.type_url.as_str() {
        "/cosmos.gov.v1.MsgSubmitProposal" => {
            let proposal = cosmos_sdk_proto::cosmos::gov::v1::MsgSubmitProposal::decode(
                message.value.as_slice(),
            )?;
            ui::kv("proposal", &proposal.title);
            ui::kv("expedited", &proposal.expedited.to_string());
            for coin in proposal.initial_deposit {
                ui::kv("deposit", &format!("{} {}", coin.amount, coin.denom));
            }
            for inner in proposal.messages {
                preview(&cosmrs::Any {
                    type_url: inner.type_url,
                    value: inner.value,
                })?;
            }
        }
        "/cosmwasm.wasm.v1.MsgExecuteContract" => {
            let execute = cosmos_sdk_proto::cosmwasm::wasm::v1::MsgExecuteContract::decode(
                message.value.as_slice(),
            )?;
            ui::address("contract", &execute.contract);
            ui::address("execution authority", &execute.sender);
            for coin in execute.funds {
                ui::kv("payment", &format!("{} {}", coin.amount, coin.denom));
            }
            ui::info(&serde_json::to_string_pretty(&serde_json::from_slice::<
                Value,
            >(&execute.msg)?)?);
        }
        other => {
            eyre::bail!("unsupported deployment message: {other}");
        }
    }
    Ok(())
}

pub async fn reconcile(
    ctx: &crate::commands::deploy::DeployContext,
    key: &str,
    saved: &Transaction,
    lcd: &str,
) -> Result<()> {
    if matches!(saved, Transaction::Cosmos { .. }) {
        if session::current()?.options.bump_fees.as_deref() == Some(key) {
            super::cosmos_fees::bump(ctx, lcd, key, saved).await?;
        } else {
            resume(lcd, saved, key).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "cosmos_tests.rs"]
mod tests;

fn unsigned_simulation(raw: &[u8]) -> Result<Vec<u8>> {
    let mut tx = cosmos_sdk_proto::cosmos::tx::v1beta1::TxRaw::decode(raw)?;
    // Cosmos simulation skips signature verification, but charges signature gas.
    // Never disclose a valid signature before the operator approves submission.
    for signature in &mut tx.signatures {
        signature.fill(0);
    }
    Ok(tx.encode_to_vec())
}
