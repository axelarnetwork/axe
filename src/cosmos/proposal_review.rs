use cosmos_sdk_proto::cosmos::{base::v1beta1::Coin, gov::v1::MsgSubmitProposal};
use cosmos_sdk_proto::cosmwasm::wasm::v1::MsgExecuteContract;
use eyre::{Result, ensure};
use prost::Message;
use serde_json::{Value, json};

/// Decode the prepared bytes for review without rebuilding the submitted proposal.
pub fn decoded(message: &cosmrs::Any) -> Result<Value> {
    ensure!(
        message.type_url == "/cosmos.gov.v1.MsgSubmitProposal",
        "expected MsgSubmitProposal for review"
    );
    let proposal = MsgSubmitProposal::decode(message.value.as_slice())?;
    let messages = proposal
        .messages
        .iter()
        .map(|inner| {
            ensure!(
                inner.type_url == "/cosmwasm.wasm.v1.MsgExecuteContract",
                "unsupported proposal message for review: {}",
                inner.type_url
            );
            let execute = MsgExecuteContract::decode(inner.value.as_slice())?;
            Ok(json!({
                "@type": inner.type_url,
                "sender": execute.sender,
                "contract": execute.contract,
                "msg": serde_json::from_slice::<Value>(&execute.msg)?,
                "funds": coins(&execute.funds),
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "@type": message.type_url,
        "title": proposal.title,
        "summary": proposal.summary,
        "metadata": proposal.metadata,
        "proposer": proposal.proposer,
        "initial_deposit": coins(&proposal.initial_deposit),
        "expedited": proposal.expedited,
        "messages": messages,
    }))
}

fn coins(coins: &[Coin]) -> Vec<Value> {
    coins
        .iter()
        .map(|coin| json!({"denom":coin.denom,"amount":coin.amount}))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosmos::{build_execute_msg_any_with_funds, build_submit_proposal_any};

    #[test]
    fn review_decodes_the_prepared_proposal_and_every_inner_message() {
        let payload = json!({"call_contract":{
            "destination_chain":"robinhood","destination_address":"ASG","payload":"570ca735"
        }});
        let execute = build_execute_msg_any_with_funds(
            "governance",
            "hub",
            &payload,
            vec![Coin {
                denom: "uaxl".into(),
                amount: "123".into(),
            }],
        )
        .unwrap();
        let built = build_submit_proposal_any(
            "proposer",
            vec![execute.clone(), execute],
            "Review title",
            "Review description",
            "3000000000",
            "uaxl",
            true,
        )
        .unwrap();
        let mut prepared = MsgSubmitProposal::decode(built.value.as_slice()).unwrap();
        prepared.metadata = "approved metadata".into();
        let message = cosmrs::Any {
            type_url: built.type_url,
            value: prepared.encode_to_vec(),
        };
        let review = decoded(&message).unwrap();
        assert_eq!(
            review,
            json!({
                "@type":"/cosmos.gov.v1.MsgSubmitProposal", "title":"Review title",
                "summary":"Review description", "metadata":"approved metadata", "proposer":"proposer",
                "initial_deposit":[{"denom":"uaxl","amount":"3000000000"}], "expedited":true,
                "messages":[
                    {"@type":"/cosmwasm.wasm.v1.MsgExecuteContract","sender":"governance","contract":"hub","msg":payload,"funds":[{"denom":"uaxl","amount":"123"}]},
                    {"@type":"/cosmwasm.wasm.v1.MsgExecuteContract","sender":"governance","contract":"hub","msg":payload,"funds":[{"denom":"uaxl","amount":"123"}]}
                ]
            })
        );
        assert_eq!(message.value, prepared.encode_to_vec());
    }

    #[test]
    fn unknown_or_malformed_messages_cannot_be_omitted_from_review() {
        for inner in [
            cosmrs::Any {
                type_url: "/unknown.Message".into(),
                value: vec![],
            },
            cosmrs::Any {
                type_url: "/cosmwasm.wasm.v1.MsgExecuteContract".into(),
                value: vec![0xff],
            },
            cosmrs::Any {
                type_url: "/cosmwasm.wasm.v1.MsgExecuteContract".into(),
                value: MsgExecuteContract {
                    msg: b"invalid JSON".to_vec(),
                    ..Default::default()
                }
                .encode_to_vec(),
            },
        ] {
            let proposal = build_submit_proposal_any(
                "proposer",
                vec![inner],
                "title",
                "summary",
                "1",
                "uaxl",
                false,
            )
            .unwrap();
            assert!(decoded(&proposal).is_err());
        }
    }
}
