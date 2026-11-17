use std::str::FromStr;

use alloy::primitives::{Address, TxHash, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use eyre::{Result, bail, eyre};

use super::routes::RoutePlan;
use crate::commands::intents::execution::action_request;
use crate::commands::intents::types::{
    Action, ActionKind, ActionPayload, Fees, PaymentMethod, Quote, QuoteRequest, StatusResponse,
    parse_amount,
};
use crate::evm::ERC20;

/// The quote's actions as transaction requests, in order. Refused unless
/// they move exactly the requested input through the route's AxelarApp, from
/// the sender, for no more native gas than the route's fee cap.
pub fn checked_requests(
    quote: &Quote,
    plan: &RoutePlan,
    request: &QuoteRequest,
    sender: Address,
) -> Result<Vec<TransactionRequest>> {
    check_quoted_route(quote, plan, request)?;
    let amount = parse_amount(&request.amount)?;
    let (approval, transfer) = split_actions(quote, plan)?;
    let transfer_request = sendable(transfer, sender, plan)?;
    check_transfer(&transfer_request, transfer, plan, amount, &quote.fees)?;
    let mut requests = Vec::with_capacity(2);
    if let Some(approval) = approval {
        let approval_request = sendable(approval, sender, plan)?;
        check_approval(&approval_request, approval, plan, amount)?;
        requests.push(approval_request);
    }
    requests.push(transfer_request);
    Ok(requests)
}

/// A DONE status passes only when it reports the route's destination asset,
/// at least the quoted output, and a source transaction this run sent.
pub fn check_delivery(
    status: &StatusResponse,
    plan: &RoutePlan,
    quote: &Quote,
    sent: &[TxHash],
) -> Result<()> {
    if let Some(source) = &status.source {
        let ours = TxHash::from_str(&source.tx_hash).is_ok_and(|hash| sent.contains(&hash));
        if !ours {
            bail!(
                "rfq-api matched source transaction {}, which this run did not send",
                source.tx_hash
            );
        }
    }
    if let Some(destination) = &status.destination
        && destination.chain != plan.to.chain.caip2
    {
        bail!(
            "quote {} was delivered on {}, not {}",
            quote.quote_id,
            destination.chain,
            plan.to.chain.caip2
        );
    }
    let output = status
        .output
        .as_ref()
        .ok_or_else(|| eyre!("DONE status for quote {} reports no output", quote.quote_id))?;
    if output.chain != plan.to.chain.caip2 || !is_destination_token(plan, &output.token) {
        bail!(
            "quote {} delivered {} on {}, not {} on {}",
            quote.quote_id,
            output.token,
            output.chain,
            plan.to.token,
            plan.to.chain.caip2
        );
    }
    let promised = quote
        .output
        .minimum_amount
        .as_deref()
        .unwrap_or(&quote.output.amount);
    if parse_amount(&output.amount)? < parse_amount(promised)? {
        bail!(
            "quote {} delivered {}, below the quoted {promised}",
            quote.quote_id,
            output.amount
        );
    }
    Ok(())
}

fn check_quoted_route(quote: &Quote, plan: &RoutePlan, request: &QuoteRequest) -> Result<()> {
    let input = &quote.input;
    if input.chain != request.from_chain
        || !same_evm_address(&input.token, &request.from_token)
        || parse_amount(&input.amount)? != parse_amount(&request.amount)?
    {
        bail!(
            "quote {} takes {} of {} on {}, not the requested {} of {} on {}",
            quote.quote_id,
            input.amount,
            input.token,
            input.chain,
            request.amount,
            request.from_token,
            request.from_chain
        );
    }
    let output = &quote.output;
    if output.chain != plan.to.chain.caip2 || !is_destination_token(plan, &output.token) {
        bail!(
            "quote {} delivers {} on {}, not {} on {}",
            quote.quote_id,
            output.token,
            output.chain,
            plan.to.token,
            plan.to.chain.caip2
        );
    }
    Ok(())
}

/// An optional approval followed by exactly one transaction.
fn split_actions<'a>(
    quote: &'a Quote,
    plan: &RoutePlan,
) -> Result<(Option<&'a Action>, &'a Action)> {
    let (approval, transfer) = match quote.actions.as_slice() {
        [transfer] if transfer.kind == ActionKind::Transaction => (None, transfer),
        [approval, transfer]
            if approval.kind == ActionKind::Approval
                && transfer.kind == ActionKind::Transaction =>
        {
            (Some(approval), transfer)
        }
        actions => {
            return Err(eyre!(
                "quote {} has actions [{}], expected an optional approval and then one transaction",
                quote.quote_id,
                actions
                    .iter()
                    .map(|action| action.kind.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    };
    if plan.spends_native() && approval.is_some() {
        bail!(
            "quote {} spends native gas but asks for a token approval",
            quote.quote_id
        );
    }
    Ok((approval, transfer))
}

/// The action as a transaction request from the sender on the source chain.
fn sendable(action: &Action, sender: Address, plan: &RoutePlan) -> Result<TransactionRequest> {
    if let ActionPayload::EvmTransaction(payload) = &action.payload
        && payload.from.is_none()
    {
        bail!("action '{}' names no sender", action.id);
    }
    action_request(action, sender, plan.from.chain.caip2)
}

fn check_transfer(
    request: &TransactionRequest,
    action: &Action,
    plan: &RoutePlan,
    amount: U256,
    fees: &Fees,
) -> Result<()> {
    let target = request.to.and_then(|to| to.to().copied());
    if target != Some(plan.axelar_app) {
        bail!(
            "transaction '{}' does not call the AxelarApp {}",
            action.id,
            plan.axelar_app
        );
    }
    let fee = transaction_value_fee(fees)?;
    if fee > plan.max_fee {
        bail!(
            "the quote charges {fee} wei of native gas, above the {} wei cap",
            plan.max_fee
        );
    }
    let deposit = if plan.spends_native() {
        amount
    } else {
        U256::ZERO
    };
    let value = request.value.unwrap_or_default();
    if value > deposit.saturating_add(fee) {
        bail!(
            "transaction '{}' sends {value} wei, more than the {deposit} wei deposit plus the {fee} wei fee",
            action.id
        );
    }
    Ok(())
}

fn check_approval(
    request: &TransactionRequest,
    action: &Action,
    plan: &RoutePlan,
    amount: U256,
) -> Result<()> {
    let token: Address = plan.from.token.parse()?;
    let data = request
        .input
        .input()
        .map(|data| data.as_ref())
        .unwrap_or_default();
    let approve = (data.len() == 68)
        .then(|| ERC20::approveCall::abi_decode(data).ok())
        .flatten();
    let exact =
        approve.is_some_and(|call| call.spender == plan.axelar_app && call.amount == amount);
    if !exact
        || request.to.and_then(|to| to.to().copied()) != Some(token)
        || !request.value.unwrap_or_default().is_zero()
    {
        bail!(
            "approval '{}' is not an approval of exactly {amount} of {token} to the AxelarApp {}",
            action.id,
            plan.axelar_app
        );
    }
    Ok(())
}

/// The fees the quote charges as transaction value.
fn transaction_value_fee(fees: &Fees) -> Result<U256> {
    [&fees.gas, &fees.user, &fees.integrator]
        .into_iter()
        .flatten()
        .filter(|fee| fee.payment_method == PaymentMethod::TxValue)
        .try_fold(U256::ZERO, |total, fee| {
            Ok(total.saturating_add(parse_amount(&fee.amount)?))
        })
}

fn is_destination_token(plan: &RoutePlan, token: &str) -> bool {
    if plan.delivers_to_solana() {
        token == plan.to.token
    } else {
        same_evm_address(token, plan.to.token)
    }
}

fn same_evm_address(left: &str, right: &str) -> bool {
    matches!(
        (left.parse::<Address>(), right.parse::<Address>()),
        (Ok(left), Ok(right)) if left == right
    )
}

#[cfg(test)]
mod tests {
    use alloy::hex;
    use alloy::primitives::address;
    use serde_json::{Value, json};

    use super::*;
    use crate::commands::intents::rfq_test::types::{Recipient, RfqRoute, RfqSource};
    use crate::types::Network;

    const FUJI: &str = "eip155:43113";
    const APP: Address = address!("0xe4f05a0D5541C03d07f5175147E92D796Cae8db6");
    const SENDER: Address = address!("0x000000000000000000000000000000000000dEaD");
    const FEE: u64 = 1_659_274_949_722_628;

    fn plan(route: RfqRoute) -> RoutePlan {
        RoutePlan::resolve(Network::Testnet, route, RfqSource::Avalanche).unwrap()
    }

    fn request(plan: &RoutePlan) -> QuoteRequest {
        let recipient = if plan.delivers_to_solana() {
            Recipient::Solana(
                "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"
                    .parse()
                    .unwrap(),
            )
        } else {
            Recipient::Evm(SENDER)
        };
        plan.quote_request(plan.amount(None), SENDER, recipient)
    }

    fn approve_data(spender: Address, amount: U256) -> String {
        format!(
            "0x{}",
            hex::encode(ERC20::approveCall { spender, amount }.abi_encode())
        )
    }

    fn action(id: &str, kind: &str, to: &str, data: &str, value: &str) -> Value {
        json!({
            "id": id,
            "label": id,
            "type": kind,
            "chain": FUJI,
            "payload": {
                "type": "evm_transaction",
                "from": SENDER.to_string(),
                "to": to,
                "data": data,
                "value": value
            }
        })
    }

    /// The quote rfq-api returns for the route: an approval when the source
    /// is a token, then the AxelarApp transaction paying the fee as value.
    fn quote_json(plan: &RoutePlan) -> Value {
        let request = request(plan);
        let amount = parse_amount(&request.amount).unwrap();
        let mut actions = Vec::new();
        let mut value = U256::from(FEE);
        if plan.spends_native() {
            value += amount;
        } else {
            actions.push(action(
                "approve",
                "approval",
                plan.from.token,
                &approve_data(APP, amount),
                "0",
            ));
        }
        actions.push(action(
            "transfer",
            "transaction",
            &APP.to_string(),
            "0x9a985d99",
            &value.to_string(),
        ));
        json!({
            "quoteId": "its_quote",
            "selectionReason": "only_route",
            "backend": { "type": "its", "name": "axelar", "tracking": {}, "metadata": {} },
            "estimatedTimeSeconds": 60,
            "validity": { "type": "expires_at", "quoteExpiresAt": "2026-11-13T23:02:45Z" },
            "input": { "chain": FUJI, "token": plan.from.token, "amount": request.amount },
            "output": {
                "chain": plan.to.chain.caip2,
                "token": plan.to.token,
                "amount": request.amount,
                "minimumAmount": request.amount
            },
            "fees": {
                "integrator": null,
                "user": {
                    "amount": FEE.to_string(),
                    "paymentMethod": "tx_value",
                    "quoteTreatment": "outside_quote",
                    "token": { "chain": FUJI, "symbol": "AVAX", "type": "native" }
                }
            },
            "actions": actions
        })
    }

    fn check(plan: &RoutePlan, quote: &Value) -> Result<Vec<TransactionRequest>> {
        let quote: Quote = serde_json::from_value(quote.clone()).unwrap();
        checked_requests(&quote, plan, &request(plan), SENDER)
    }

    fn rejected(plan: &RoutePlan, edit: impl FnOnce(&mut Value)) -> String {
        let mut quote = quote_json(plan);
        edit(&mut quote);
        check(plan, &quote).unwrap_err().to_string()
    }

    #[test]
    fn accepts_the_quotes_rfq_api_returns_for_every_route() {
        for route in [
            RfqRoute::Its,
            RfqRoute::NativeWrap,
            RfqRoute::Gateway,
            RfqRoute::Solana,
        ] {
            let plan = plan(route);
            let requests = check(&plan, &quote_json(&plan)).unwrap();
            assert_eq!(requests.len(), if plan.spends_native() { 1 } else { 2 });
            assert_eq!(
                requests
                    .last()
                    .and_then(|request| request.to?.to().copied()),
                Some(APP)
            );
        }
    }

    #[test]
    fn refuses_a_quote_for_another_input_or_destination() {
        let its = plan(RfqRoute::Its);
        assert!(rejected(&its, |quote| quote["input"]["amount"] = json!("1")).contains("takes"));
        assert!(
            rejected(&its, |quote| quote["input"]["token"] =
                json!(APP.to_string()))
            .contains("takes")
        );
        assert!(
            rejected(&its, |quote| quote["output"]["token"] =
                json!(APP.to_string()))
            .contains("delivers")
        );
        let solana = plan(RfqRoute::Solana);
        assert!(
            rejected(&solana, |quote| {
                quote["output"]["token"] = json!(solana.to.token.to_lowercase());
            })
            .contains("delivers")
        );
    }

    #[test]
    fn refuses_any_other_action_shape() {
        let its = plan(RfqRoute::Its);
        assert!(
            rejected(&its, |quote| {
                let actions = quote["actions"].as_array_mut().unwrap();
                actions.reverse();
            })
            .contains("expected an optional approval")
        );
        assert!(
            rejected(&its, |quote| {
                let transfer = quote["actions"][1].clone();
                quote["actions"].as_array_mut().unwrap().push(transfer);
            })
            .contains("expected an optional approval")
        );
        assert!(
            rejected(&its, |quote| quote["actions"] = json!([])).contains("expected an optional")
        );
        let native = plan(RfqRoute::NativeWrap);
        assert!(
            rejected(&native, |quote| {
                let approval = action(
                    "approve",
                    "approval",
                    its.from.token,
                    &approve_data(APP, U256::from(1u8)),
                    "0",
                );
                quote["actions"].as_array_mut().unwrap().insert(0, approval);
            })
            .contains("native gas")
        );
    }

    #[test]
    fn every_action_must_come_from_the_sender() {
        let its = plan(RfqRoute::Its);
        assert!(
            rejected(&its, |quote| {
                quote["actions"][0]["payload"]
                    .as_object_mut()
                    .unwrap()
                    .remove("from");
            })
            .contains("names no sender")
        );
        assert!(
            rejected(&its, |quote| {
                quote["actions"][1]["payload"]["from"] = json!(APP.to_string());
            })
            .contains("does not match")
        );
    }

    #[test]
    fn the_transaction_must_call_the_axelar_app_within_the_fee_cap() {
        let its = plan(RfqRoute::Its);
        assert!(
            rejected(&its, |quote| {
                quote["actions"][1]["payload"]["to"] = json!(its.from.token);
            })
            .contains("AxelarApp")
        );
        assert!(
            rejected(&its, |quote| {
                quote["actions"][1]["payload"]["value"] = json!((FEE + 1).to_string());
            })
            .contains("more than")
        );
        assert!(
            rejected(&its, |quote| {
                quote["fees"]["user"]["amount"] = json!("250000000000000001");
                quote["actions"][1]["payload"]["value"] = json!("250000000000000001");
            })
            .contains("above the")
        );
        let native = plan(RfqRoute::NativeWrap);
        let deposit = native.amount(None);
        assert!(
            rejected(&native, |quote| {
                quote["actions"][0]["payload"]["value"] =
                    json!((deposit + U256::from(FEE + 1)).to_string());
            })
            .contains("more than")
        );
    }

    #[test]
    fn the_approval_must_be_exactly_the_input_to_the_axelar_app() {
        let its = plan(RfqRoute::Its);
        let amount = its.amount(None);
        for edit in [
            json!({ "data": approve_data(APP, amount + U256::from(1u8)) }),
            json!({ "data": approve_data(Address::from([7u8; 20]), amount) }),
            json!({ "to": APP.to_string() }),
            json!({ "value": "1" }),
            json!({ "data": "0x095ea7b3" }),
        ] {
            let message = rejected(&its, |quote| {
                for (key, value) in edit.as_object().unwrap() {
                    quote["actions"][0]["payload"][key] = value.clone();
                }
            });
            assert!(
                message.contains("is not an approval of exactly"),
                "{message}"
            );
        }
    }

    fn status(output: &Value, source_tx: &str) -> StatusResponse {
        serde_json::from_value(json!({
            "quoteId": "its_quote",
            "state": "DONE",
            "backend": { "type": "its", "name": "axelar", "tracking": {}, "metadata": {} },
            "source": {
                "chain": FUJI,
                "txHash": source_tx,
                "messageId": "0xmessage-1",
                "timestamp": "2026-11-13T23:02:45Z"
            },
            "destination": null,
            "output": output,
            "refund": null,
            "details": {}
        }))
        .unwrap()
    }

    #[test]
    fn done_must_report_our_transaction_and_the_quoted_output() {
        let solana = plan(RfqRoute::Solana);
        let quote: Quote = serde_json::from_value(quote_json(&solana)).unwrap();
        let sent = [TxHash::from([3u8; 32])];
        let ours = sent[0].to_string();
        let delivered = |token: &str, amount: &str| json!({ "chain": solana.to.chain.caip2, "token": token, "amount": amount });
        let amount = quote.output.amount.clone();

        let check = |status: &StatusResponse| check_delivery(status, &solana, &quote, &sent);
        assert!(check(&status(&delivered(solana.to.token, &amount), &ours)).is_ok());
        assert!(
            check(&status(
                &delivered(solana.to.token, &amount),
                &TxHash::ZERO.to_string()
            ))
            .is_err()
        );
        assert!(
            check(&status(
                &delivered(&solana.to.token.to_lowercase(), &amount),
                &ours
            ))
            .is_err()
        );
        assert!(check(&status(&delivered(solana.to.token, "1"), &ours)).is_err());
        assert!(check(&status(&Value::Null, &ours)).is_err());
    }
}
