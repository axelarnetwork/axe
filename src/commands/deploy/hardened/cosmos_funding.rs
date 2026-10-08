use crate::{cosmos::check_axelar_balance, ui};
use cosmos_sdk_proto::cosmos::gov::v1::MsgSubmitProposal;
use cosmos_sdk_proto::cosmwasm::wasm::v1::MsgExecuteContract;
use eyre::Result;
use prost::Message;

pub async fn approve(
    lcd: &str,
    chain: &str,
    denom: &str,
    address: &str,
    messages: &[cosmrs::Any],
    fee: u128,
) -> Result<bool> {
    let amount = required(messages, denom)?
        .checked_add(fee)
        .ok_or_else(|| eyre::eyre!("submission funding overflow"))?;
    check_axelar_balance(
        lcd,
        chain,
        &address.parse::<cosmrs::AccountId>()?,
        &denom.parse::<cosmrs::Denom>()?,
        amount,
    )
    .await?;
    ui::section("Cosmos transaction");
    ui::kv("chain", chain);
    ui::address("signer", address);
    ui::kv("fee", &format!("{fee} {denom}"));
    if messages
        .iter()
        .any(|m| m.type_url == "/cosmos.gov.v1.MsgSubmitProposal")
    {
        ui::kv(
            "expedited voting period",
            &super::governance::parameters(lcd)
                .await?
                .expedited_voting_period,
        );
    }
    for message in messages {
        super::cosmos::preview(message)?;
    }
    Ok(ui::confirm("Submit this transaction?").await)
}

pub(super) fn required(messages: &[cosmrs::Any], denom: &str) -> Result<u128> {
    let mut amount = 0u128;
    for message in messages {
        let coins = match message.type_url.as_str() {
            "/cosmos.gov.v1.MsgSubmitProposal" => {
                MsgSubmitProposal::decode(message.value.as_slice())?.initial_deposit
            }
            "/cosmwasm.wasm.v1.MsgExecuteContract" => {
                MsgExecuteContract::decode(message.value.as_slice())?.funds
            }
            _ => {
                eyre::bail!("unsupported deployment message");
            }
        };
        for coin in coins {
            eyre::ensure!(
                coin.denom == denom,
                "unsupported deployment payment denomination"
            );
            amount = amount
                .checked_add(coin.amount.parse()?)
                .ok_or_else(|| eyre::eyre!("submission funding overflow"))?;
        }
    }
    Ok(amount)
}

/// Fee replacements at one sequence are mutually exclusive. Failed included
/// attempts retried at later sequences still count separately.
pub(super) fn fee_liability(
    journal: &super::types::Journal,
    sender: &str,
    candidate: Option<(u64, u128)>,
) -> Result<u128> {
    let mut sequences = std::collections::BTreeMap::<u64, u128>::new();
    for tx in journal
        .actions
        .values()
        .chain(journal.attempts.values().flatten())
    {
        if let super::types::Transaction::Cosmos {
            sender: from,
            sequence,
            fee,
            ..
        } = tx
            && from == sender
        {
            let amount = fee.parse::<u128>()?;
            let maximum = sequences.entry(*sequence).or_default();
            *maximum = (*maximum).max(amount);
        }
    }
    if let Some((sequence, fee)) = candidate {
        let maximum = sequences.entry(sequence).or_default();
        *maximum = (*maximum).max(fee);
    }
    sequences.values().try_fold(0u128, |sum, fee| {
        sum.checked_add(*fee)
            .ok_or_else(|| eyre::eyre!("fee budget overflow"))
    })
}

pub(super) async fn check_fee_budget(sender: &str, sequence: u64, fee: u128) -> Result<()> {
    let session = super::session::current()?;
    let journal = session.journal.lock().await;
    eyre::ensure!(
        fee_liability(&journal, sender, Some((sequence, fee)))?
            <= session.plan.cosmos_fee_budget.parse::<u128>()?,
        "Cosmos fee budget exceeded"
    );
    Ok(())
}
