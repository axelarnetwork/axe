mod funding;
mod poll;
mod report;
mod routes;
mod types;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, eyre};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;

use self::funding::{SourceChain, top_up_wrapped_native};
use self::poll::{PollEnd, PollLimits, watch_status};
use self::report::{ActionTx, Output, RunReport};
use self::routes::{RoutePlan, SourceFunding};
use crate::commands::intents::client::RfqClient;
use crate::commands::intents::execution::{action_request, submit_action};
use crate::commands::intents::execution_lock::ExecutionLock;
use crate::commands::intents::route::{DiscoveryFeedback, resolve_evm_chains};
use crate::commands::intents::types::{ChainInfo, Quote, QuoteOutcome, QuoteRequest, TimedQuote};
use crate::config::ChainsConfig;
use crate::evm::EvmEndpoints;
use crate::types::Network;
use crate::ui;

pub use self::types::{RfqRoute, RfqSource, RfqTestArgs, Wallet};

const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Everything a route check needs once its route and wallet are resolved.
struct RouteCheck {
    network: Network,
    config: Option<PathBuf>,
    client: RfqClient,
    plan: RoutePlan,
    amount: U256,
    request: QuoteRequest,
    timeout: Duration,
    output: Output,
}

/// Quote one rfq-api route, execute its actions on the source chain, and
/// watch the transfer to DONE. A dry run stops after the quote.
pub async fn run(args: RfqTestArgs) -> Result<()> {
    if args.json {
        ui::warnings_to_stderr(check_route(args)).await
    } else {
        check_route(args).await
    }
}

async fn check_route(args: RfqTestArgs) -> Result<()> {
    let plan = RoutePlan::resolve(args.network, args.route, args.source)?;
    let sender = args.wallet.address();
    let configured_solana = configured_solana_key(&plan, args.recipient.as_deref()).await?;
    let recipient = plan.recipient(args.recipient.as_deref(), sender, configured_solana)?;
    let amount = args
        .amount
        .unwrap_or_else(|| U256::from(plan.default_amount));
    let request = plan.quote_request(amount, sender, recipient);
    let check = RouteCheck {
        network: args.network,
        config: args.config,
        client: RfqClient::new(args.network, args.rfq_url.as_deref())?,
        plan,
        amount,
        request: request.clone(),
        timeout: args.timeout,
        output: Output::new(args.json),
    };
    let dry_run = matches!(args.wallet, Wallet::DryRun(_));
    let mut report = RunReport::new(args.network, args.route, args.source, dry_run, request);
    check.output.announce(&report, &check.plan);

    let started = Instant::now();
    let result = match &args.wallet {
        Wallet::DryRun(sender) => quote_only(&check, *sender, &mut report).await,
        Wallet::Signer(signer) => execute(&check, signer, &mut report).await,
    };
    report.close(&result, started.elapsed());
    check.output.summary(&report)?;
    result
}

async fn quote_only(check: &RouteCheck, sender: Address, report: &mut RunReport) -> Result<()> {
    let quote = request_quote(check, report).await?;
    validate_actions(&quote, sender, check.plan.from.chain.caip2)?;
    report.actions = Some(quote.actions);
    Ok(())
}

async fn execute(
    check: &RouteCheck,
    signer: &PrivateKeySigner,
    report: &mut RunReport,
) -> Result<()> {
    let _lock = ExecutionLock::acquire(signer.address())?;
    let source = connect_source(check).await?;
    if check.plan.funding == SourceFunding::WrappedNative {
        let wrap =
            top_up_wrapped_native(&source, signer, check.plan.from, check.amount, check.output)
                .await?;
        report.wrap_tx = wrap.map(|hash| hash.to_string());
    }
    let quote = request_quote(check, report).await?;
    validate_actions(&quote, signer.address(), check.plan.from.chain.caip2)?;
    submit_actions(check, &source.endpoints, signer, &quote, report).await?;
    let end = watch_status(
        || check.client.status(&quote.quote_id),
        PollLimits {
            interval: POLL_INTERVAL,
            timeout: check.timeout,
        },
        check.output,
    )
    .await;
    settle(end, &quote.quote_id, check.timeout, report)
}

/// The configured Solana key's address, read only when a Solana route needs a
/// default recipient: `SOLANA_PRIVATE_KEY` as a keypair file path, else the
/// Solana CLI's default keypair when one exists.
async fn configured_solana_key(
    plan: &RoutePlan,
    recipient: Option<&str>,
) -> Result<Option<Pubkey>> {
    if recipient.is_some() || !plan.delivers_to_solana() {
        return Ok(None);
    }
    let path = std::env::var("SOLANA_PRIVATE_KEY")
        .ok()
        .filter(|path| !path.trim().is_empty());
    let keypair = crate::solana::load_keypair(path.as_deref()).await;
    match (keypair, path) {
        (Ok(keypair), _) => Ok(Some(keypair.pubkey())),
        (Err(_), None) => Ok(None),
        // The variable may hold key material rather than a path, so the
        // loader's message, which quotes it, is not repeated.
        (Err(_), Some(_)) => Err(eyre!(
            "SOLANA_PRIVATE_KEY does not name a readable Solana keypair file"
        )),
    }
}

async fn connect_source(check: &RouteCheck) -> Result<SourceChain> {
    let config_path = crate::config_source::resolve(check.network, check.config.clone())
        .await?
        .into_path();
    let config = ChainsConfig::load(&config_path).await?;
    let chain = check.plan.from.chain;
    let catalog = vec![ChainInfo {
        chain_id: chain.caip2.to_owned(),
        chain_label: chain.label.to_owned(),
        chain_type: "evm".to_owned(),
    }];
    let feedback = match check.output {
        Output::Human => DiscoveryFeedback::Detailed,
        Output::Json => DiscoveryFeedback::Quiet,
    };
    let runtime = resolve_evm_chains(&config, catalog, feedback)
        .await?
        .remove(chain.caip2)
        .ok_or_else(|| eyre!("{} has no working RPC in the chains config", chain.label))?;
    let endpoints = EvmEndpoints::connect(std::slice::from_ref(&runtime.rpc_url))?;
    Ok(SourceChain { runtime, endpoints })
}

async fn request_quote(check: &RouteCheck, report: &mut RunReport) -> Result<Quote> {
    let outcome = check
        .client
        .quote_from(&check.request, check.plan.backends)
        .await?;
    let quote = match outcome {
        QuoteOutcome::Available(timed) => {
            let TimedQuote { quote, .. } = *timed;
            quote
        }
        QuoteOutcome::Unavailable(reason) => {
            return Err(eyre!("rfq-api returned no usable quote: {reason}"));
        }
    };
    check.output.kv(
        "quote",
        &format!(
            "{} ({}, receives {}, ~{} s)",
            quote.quote_id, quote.backend.kind, quote.output.amount, quote.estimated_time_seconds
        ),
    );
    report.record_quote(&quote);
    Ok(quote)
}

/// Every action must be an EVM transaction on the source chain from the
/// sender, checked before the first one is sent.
fn validate_actions(quote: &Quote, sender: Address, source_chain: &str) -> Result<()> {
    if quote.actions.is_empty() {
        return Err(eyre!("quote {} has no actions", quote.quote_id));
    }
    for action in &quote.actions {
        action_request(action, sender, source_chain)?;
    }
    Ok(())
}

async fn submit_actions(
    check: &RouteCheck,
    endpoints: &EvmEndpoints,
    signer: &PrivateKeySigner,
    quote: &Quote,
    report: &mut RunReport,
) -> Result<()> {
    for action in &quote.actions {
        let hash = submit_action(
            endpoints,
            signer,
            check.plan.from.chain.caip2,
            action,
            "rfq action",
            |message| check.output.warn(message),
        )
        .await?;
        check
            .output
            .kv(&format!("action {}", action.id), &hash.to_string());
        report.transactions.push(ActionTx {
            action: action.id.clone(),
            tx_hash: hash.to_string(),
        });
    }
    Ok(())
}

fn settle(end: PollEnd, quote_id: &str, timeout: Duration, report: &mut RunReport) -> Result<()> {
    match end {
        PollEnd::Done(status) => {
            report.status = Some(status);
            Ok(())
        }
        PollEnd::Failed(status) => {
            let state = status.state.label();
            report.status = Some(status);
            Err(eyre!("quote {quote_id} ended {state}"))
        }
        PollEnd::TimedOut(last) => {
            let state = last
                .as_ref()
                .map_or("never read", |status| status.state.label());
            report.status = last;
            Err(eyre!(
                "quote {quote_id} did not reach DONE within {} s (last state: {state})",
                timeout.as_secs()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::commands::intents::types::{StatusResponse, TransferState};

    const FUJI: &str = "eip155:43113";

    fn sender() -> Address {
        "0x000000000000000000000000000000000000dEaD"
            .parse()
            .unwrap()
    }

    fn quote(actions: &serde_json::Value) -> Quote {
        serde_json::from_value(json!({
            "quoteId": "its_quote",
            "selectionReason": "only_route",
            "backend": { "type": "its", "name": "axelar", "tracking": {}, "metadata": {} },
            "estimatedTimeSeconds": 60,
            "validity": { "type": "expires_at", "quoteExpiresAt": "2026-11-13T23:02:45Z" },
            "input": { "chain": FUJI, "token": "0xfrom", "amount": "1" },
            "output": { "chain": "eip155:11155111", "token": "0xto", "amount": "1" },
            "fees": { "integrator": null },
            "actions": actions
        }))
        .unwrap()
    }

    fn evm_action(id: &str, chain: &str, from: &str) -> serde_json::Value {
        json!({
            "id": id,
            "label": id,
            "type": "transaction",
            "chain": chain,
            "payload": {
                "type": "evm_transaction",
                "from": from,
                "to": "0xe4f05a0D5541C03d07f5175147E92D796Cae8db6",
                "data": "0x1234",
                "value": "5"
            }
        })
    }

    fn status(state: &str) -> StatusResponse {
        serde_json::from_value(json!({
            "quoteId": "its_quote",
            "state": state,
            "backend": { "type": "its", "name": "axelar", "tracking": {}, "metadata": {} },
            "source": null,
            "destination": null,
            "output": null,
            "refund": null,
            "details": {}
        }))
        .unwrap()
    }

    fn its_report() -> RunReport {
        let plan =
            RoutePlan::resolve(Network::Testnet, RfqRoute::Its, RfqSource::Avalanche).unwrap();
        let request = plan.quote_request(
            U256::from(plan.default_amount),
            sender(),
            plan.recipient(None, sender(), None).unwrap(),
        );
        RunReport::new(
            Network::Testnet,
            RfqRoute::Its,
            RfqSource::Avalanche,
            false,
            request,
        )
    }

    #[test]
    fn accepts_every_action_on_the_source_chain_from_the_sender() {
        let dead = sender().to_string();
        let quote = quote(&json!([
            evm_action("approve", FUJI, &dead),
            evm_action("transfer", FUJI, &dead)
        ]));

        assert!(validate_actions(&quote, sender(), FUJI).is_ok());
    }

    #[test]
    fn rejects_a_quote_before_sending_when_any_action_is_unsendable() {
        let dead = sender().to_string();
        let other = Address::from([1u8; 20]).to_string();
        for actions in [
            json!([]),
            json!([
                evm_action("approve", FUJI, &dead),
                evm_action("transfer", "eip155:1", &dead)
            ]),
            json!([
                evm_action("approve", FUJI, &dead),
                evm_action("transfer", FUJI, &other)
            ]),
        ] {
            assert!(validate_actions(&quote(&actions), sender(), FUJI).is_err());
        }
    }

    #[test]
    fn settles_done_as_a_pass_and_records_the_status() {
        let mut report = its_report();

        settle(
            PollEnd::Done(status("DONE")),
            "its_quote",
            Duration::from_secs(2700),
            &mut report,
        )
        .unwrap();

        assert_eq!(
            report.status.map(|status| status.state),
            Some(TransferState::Done)
        );
    }

    #[test]
    fn settles_terminal_failures_and_timeouts_as_errors() {
        let mut report = its_report();
        let error = settle(
            PollEnd::Failed(status("NOT_FOUND")),
            "its_quote",
            Duration::from_secs(2700),
            &mut report,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "quote its_quote ended not found");
        assert!(report.status.is_some());

        let mut report = its_report();
        let error = settle(
            PollEnd::TimedOut(Some(status("PENDING"))),
            "its_quote",
            Duration::from_secs(2700),
            &mut report,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "quote its_quote did not reach DONE within 2700 s (last state: pending)"
        );
        assert_eq!(
            report.status.map(|status| status.state),
            Some(TransferState::Pending)
        );
    }
}
