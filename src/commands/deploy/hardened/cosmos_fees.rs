use alloy::signers::k256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use cosmos_sdk_proto::cosmos::tx::v1beta1::{AuthInfo, SignDoc, TxBody, TxRaw};
use cosmrs::crypto::secp256k1::SigningKey;
use eyre::Result;
use prost::Message;
use sha2::{Digest, Sha256};

use super::{cosmos_funding, cosmos_recovery, journal, session, types::Transaction};
use crate::commands::deploy::DeployContext;
use crate::cosmos::{check_axelar_balance, derive_axelar_wallet, rpc::lcd_query_account};
use crate::ui;

pub(super) async fn bump(
    ctx: &DeployContext,
    lcd: &str,
    key: &str,
    saved: &Transaction,
) -> Result<()> {
    if cosmos_recovery::confirmed(lcd, saved, key).await?.is_some() {
        ui::info(&format!(
            "{key}: already confirmed; no fee replacement needed"
        ));
        return Ok(());
    }
    let Transaction::Cosmos {
        sender,
        sequence,
        hash,
        ..
    } = saved
    else {
        eyre::bail!("expected Cosmos action");
    };
    let (account, current) = lcd_query_account(lcd, sender).await?;
    eyre::ensure!(
        current == *sequence,
        "{key}: sequence consumed or unavailable; recover all attempt receipts before replacing"
    );
    let mnemonic = if key == "WaitForVerifierSet/cosmos" {
        ctx.state
            .admin_mnemonic
            .as_deref()
            .ok_or_else(|| eyre::eyre!("missing prover admin credential"))?
    } else {
        &ctx.state.mnemonic
    };
    let (signing_key, _) = derive_axelar_wallet(mnemonic)?;
    let session = session::current()?;
    let amount = session
        .options
        .cosmos_fee
        .ok_or_else(|| eyre::eyre!("supply --cosmos-fee in total base units"))?;
    let replacement = resign(
        saved,
        &signing_key,
        &session.plan.axelar_chain_id,
        account,
        amount,
    )?;
    let Transaction::Cosmos { raw, .. } = &replacement else {
        eyre::bail!("expected replacement");
    };
    let tx = TxRaw::decode(raw.as_slice())?;
    let auth = AuthInfo::decode(tx.auth_info_bytes.as_slice())?;
    let fee = auth.fee.ok_or_else(|| eyre::eyre!("missing fee"))?;
    let denom = &fee.amount[0].denom;
    let body = TxBody::decode(tx.body_bytes.as_slice())?;
    cosmos_funding::check_fee_budget(sender, *sequence, amount).await?;
    let required = cosmos_funding::required(&body.messages, denom)?
        .checked_add(amount)
        .ok_or_else(|| eyre::eyre!("funding overflow"))?;
    check_axelar_balance(
        lcd,
        &session.plan.axelar_chain_id,
        &sender.parse::<cosmrs::AccountId>()?,
        &denom.parse::<cosmrs::Denom>()?,
        required,
    )
    .await?;
    ui::section("Cosmos fee-only replacement");
    ui::kv("action", key);
    ui::tx_hash("original", hash);
    ui::address("signer", sender);
    ui::kv("chain", &session.plan.axelar_chain_id);
    ui::kv("same sequence", &sequence.to_string());
    ui::kv("unchanged gas limit", &fee.gas_limit.to_string());
    ui::kv("new total fee", &format!("{amount} {denom}"));
    for message in &body.messages {
        super::cosmos::preview(message)?;
    }
    if !ui::confirm("Approve this same-sequence, fee-only replacement?").await {
        return Err(session::pause("Cosmos fee replacement declined"));
    }
    journal::replace(key, replacement.clone()).await?;
    cosmos_recovery::resume(lcd, &replacement, key).await?;
    Ok(())
}

/// Authenticate the original signing domain before changing only its fee amount.
/// Keeping the raw body preserves messages, memo, timeout and extension options.
pub(super) fn resign(
    saved: &Transaction,
    signer: &SigningKey,
    chain: &str,
    account: u64,
    amount: u128,
) -> Result<Transaction> {
    let Transaction::Cosmos {
        raw,
        hash,
        sender,
        sequence,
        fee: old_fee,
        intent,
    } = saved
    else {
        eyre::bail!("expected Cosmos action");
    };
    eyre::ensure!(
        hex::encode_upper(Sha256::digest(raw)) == *hash,
        "signed bytes mismatch"
    );
    let mut tx = TxRaw::decode(raw.as_slice())?;
    let mut auth = AuthInfo::decode(tx.auth_info_bytes.as_slice())?;
    cosmos_recovery::validate_sequence(raw, *sequence)?;
    eyre::ensure!(
        signer
            .public_key()
            .account_id("axelar")
            .map_err(|e| eyre::eyre!("invalid signer: {e}"))?
            .as_ref()
            == sender
            && auth.signer_infos[0].public_key
                == Some(
                    signer
                        .public_key()
                        .to_any()
                        .map_err(|e| eyre::eyre!("invalid public key: {e}"))?
                )
            && tx.signatures.len() == 1,
        "Cosmos replacement signer mismatch"
    );
    let mut doc = SignDoc {
        body_bytes: tx.body_bytes.clone(),
        auth_info_bytes: tx.auth_info_bytes.clone(),
        chain_id: chain.into(),
        account_number: account,
    };
    VerifyingKey::from_sec1_bytes(&signer.public_key().to_bytes())?
        .verify(
            &doc.encode_to_vec(),
            &Signature::from_slice(&tx.signatures[0])?,
        )
        .map_err(|_| {
            eyre::eyre!("original Cosmos signature does not match chain, account number or signer")
        })?;
    eyre::ensure!(
        auth.encode_to_vec() == tx.auth_info_bytes,
        "unsupported noncanonical Cosmos auth info"
    );
    let fee = auth
        .fee
        .as_mut()
        .ok_or_else(|| eyre::eyre!("missing fee"))?;
    eyre::ensure!(
        fee.amount.len() == 1
            && fee.amount[0].amount == *old_fee
            && amount > old_fee.parse::<u128>()?,
        "replacement must increase the original single-denomination fee"
    );
    fee.amount[0].amount = amount.to_string();
    tx.auth_info_bytes = auth.encode_to_vec();
    doc.auth_info_bytes.clone_from(&tx.auth_info_bytes);
    tx.signatures = vec![
        signer
            .sign(&doc.encode_to_vec())
            .map_err(|e| eyre::eyre!("replacement signing failed: {e}"))?
            .to_vec(),
    ];
    let raw = tx.encode_to_vec();
    Ok(Transaction::Cosmos {
        hash: hex::encode_upper(Sha256::digest(&raw)),
        raw,
        intent: *intent,
        sender: sender.clone(),
        sequence: *sequence,
        fee: amount.to_string(),
    })
}
