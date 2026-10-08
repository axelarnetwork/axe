use cosmos_sdk_proto::cosmos::tx::v1beta1::{TxBody, TxRaw};
use eyre::Result;
use prost::Message;

use super::{session, types::Transaction};
use crate::commands::deploy::DeployContext;

pub(super) fn validate_authority(signer: &str, governor: &str) -> Result<()> {
    eyre::ensure!(
        signer == governor,
        "devnet direct execution requires MNEMONIC for the configured on-chain governance authority {governor}; supplied signer is {signer}"
    );
    Ok(())
}

pub(super) fn validate_submission(saved: &Transaction) -> Result<()> {
    let Transaction::Cosmos { raw, .. } = saved else {
        eyre::bail!("expected Cosmos transaction");
    };
    let raw = TxRaw::decode(raw.as_slice())?;
    let body = TxBody::decode(raw.body_bytes.as_slice())?;
    eyre::ensure!(
        !body.messages.is_empty()
            && body
                .messages
                .iter()
                .all(|m| m.type_url == "/cosmwasm.wasm.v1.MsgExecuteContract"),
        "this devnet journal contains governance submissions; resume using its original binary and finish those proposals before switching execution modes"
    );
    Ok(())
}

pub(super) async fn check(ctx: &DeployContext, key: &str) -> Result<()> {
    let step = match key {
        "instantiate" => "InstantiateChainContracts",
        "register" => "RegisterDeployment",
        "itsHubRegister" => "RegisterItsOnHub",
        _ => {
            eyre::bail!("unknown direct execution checkpoint");
        }
    };
    let key = format!("{step}/cosmos");
    let saved = session::current()?
        .get(&key)
        .await
        .ok_or_else(|| eyre::eyre!("no journaled direct execution for {step}"))?;
    validate_submission(&saved)?;
    let (lcd, _, _, _) = crate::cosmos::read_axelar_config(&ctx.target_json).await?;
    super::cosmos::resume(&lcd, &saved, &key).await?;
    Ok(())
}
