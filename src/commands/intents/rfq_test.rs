mod poll;
mod report;
mod routes;
mod source;
mod types;

use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, eyre};
use futures::future::join_all;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;

use self::poll::{PollEnd, watch_status};
use self::report::{ActionTx, Output, RouteReport, RunSummary};
use self::routes::RoutePlan;
use self::source::SourceChain;
use self::types::Recipient;
use crate::commands::intents::client::RfqClient;
use crate::commands::intents::execution::action_request;
use crate::commands::intents::execution_lock::ExecutionLock;
use crate::commands::intents::types::{
    Quote, QuoteOutcome, TimedQuote, TransferState, parse_amount,
};
use crate::ui;

pub use self::types::{RfqRoute, RfqSource, RfqTestArgs, Wallet};

/// One route's plan and what checking it produced.
struct RouteRun {
    plan: RoutePlan,
    report: RouteReport,
    started: Instant,
}

/// Quote rfq-api routes, send each one's actions on the source chain in turn,
/// then watch the transfers to DONE together. A dry run stops after the
/// quotes.
pub async fn run(args: RfqTestArgs) -> Result<()> {
    if args.json {
        ui::warnings_to_stderr(check_routes(args)).await
    } else {
        check_routes(args).await
    }
}

async fn check_routes(args: RfqTestArgs) -> Result<()> {
    let started = Instant::now();
    let output = Output::new(args.json);
    let client = RfqClient::new(args.network, args.rfq_url.as_deref())?;
    let mut runs = plan_routes(&args).await?;
    let dry_run = matches!(args.wallet, Wallet::DryRun(_));
    output.announce_run(args.network, args.wallet.address(), dry_run);
    match &args.wallet {
        Wallet::DryRun(sender) => quote_routes(&client, *sender, &mut runs, output).await,
        Wallet::Signer(signer) => {
            let sent = send_routes(&args, &client, signer, &mut runs, output).await;
            watch_routes(&client, &sent, args.timeout, &mut runs, output).await;
        }
    }
    let summary = RunSummary {
        network: args.network,
        source: args.source,
        dry_run,
        elapsed_secs: started.elapsed().as_secs(),
        routes: runs.into_iter().map(|run| run.report).collect(),
    };
    output.summary(&summary)?;
    summary.verdict()
}

async fn plan_routes(args: &RfqTestArgs) -> Result<Vec<RouteRun>> {
    if args.amount.is_some() && args.routes.len() > 1 {
        return Err(eyre!("--amount applies to a single --route"));
    }
    let plans = args
        .routes
        .iter()
        .map(|&route| RoutePlan::resolve(args.network, route, args.source))
        .collect::<Result<Vec<_>>>()?;
    let sender = args.wallet.address();
    let recipients = recipients(&plans, args.recipient.as_deref(), sender).await?;
    Ok(plans
        .into_iter()
        .zip(recipients)
        .map(|(plan, recipient)| {
            let amount = args
                .amount
                .unwrap_or_else(|| U256::from(plan.default_amount));
            RouteRun {
                report: RouteReport::new(plan.route, plan.quote_request(amount, sender, recipient)),
                plan,
                started: Instant::now(),
            }
        })
        .collect())
}

/// Each route's recipient: `--recipient` when given, else the route's
/// default, reading the configured Solana key at most once.
async fn recipients(
    plans: &[RoutePlan],
    explicit: Option<&str>,
    sender: Address,
) -> Result<Vec<Recipient>> {
    let configured_solana = if explicit.is_none() && plans.iter().any(RoutePlan::delivers_to_solana)
    {
        configured_solana_key().await?
    } else {
        None
    };
    plans
        .iter()
        .map(|plan| plan.recipient(explicit, sender, configured_solana))
        .collect()
}

/// The configured Solana key's address: `SOLANA_PRIVATE_KEY` as a keypair
/// file path, else the Solana CLI's default keypair when one exists.
async fn configured_solana_key() -> Result<Option<Pubkey>> {
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

async fn quote_routes(client: &RfqClient, sender: Address, runs: &mut [RouteRun], output: Output) {
    for run in runs {
        output.announce_route(&run.plan, &run.report.request);
        run.started = Instant::now();
        let result = quote_route(client, sender, run, output).await;
        run.report
            .finish(result.as_ref().err(), run.started.elapsed());
    }
}

async fn quote_route(
    client: &RfqClient,
    sender: Address,
    run: &mut RouteRun,
    output: Output,
) -> Result<()> {
    let quote = request_quote(client, run, output).await?;
    action_requests(&quote, sender, run.plan.from.chain.caip2)?;
    run.report.actions = Some(quote.actions);
    Ok(())
}

/// Send every route's actions in turn while holding the wallet lock. Returns,
/// per route, the quote ID of a route whose actions were all sent.
async fn send_routes(
    args: &RfqTestArgs,
    client: &RfqClient,
    signer: &PrivateKeySigner,
    runs: &mut [RouteRun],
    output: Output,
) -> Vec<Option<String>> {
    let Some(chain) = runs.first().map(|run| run.plan.from.chain) else {
        return Vec::new();
    };
    let connected = async {
        let lock = ExecutionLock::acquire(signer.address())?;
        let source = SourceChain::connect(args.network, args.config.clone(), chain, output).await?;
        Ok::<_, eyre::Report>((lock, source))
    }
    .await;
    let (_lock, source) = match connected {
        Ok(connected) => connected,
        Err(error) => {
            for run in runs.iter_mut() {
                run.report.finish(Some(&error), Duration::ZERO);
            }
            return vec![None; runs.len()];
        }
    };
    let mut sent = Vec::with_capacity(runs.len());
    for run in runs {
        output.announce_route(&run.plan, &run.report.request);
        run.started = Instant::now();
        match send_route(client, &source, signer, run, output).await {
            Ok(quote_id) => sent.push(Some(quote_id)),
            Err(error) => {
                run.report.finish(Some(&error), run.started.elapsed());
                sent.push(None);
            }
        }
    }
    sent
}

async fn send_route(
    client: &RfqClient,
    source: &SourceChain,
    signer: &PrivateKeySigner,
    run: &mut RouteRun,
    output: Output,
) -> Result<String> {
    if run.plan.wrap_shortfall {
        let amount = parse_amount(&run.report.request.amount)?;
        let wrap = source
            .wrap_shortfall(signer, run.plan.from.token, amount, output)
            .await?;
        run.report.wrap_tx = wrap.map(|hash| hash.to_string());
    }
    let quote = request_quote(client, run, output).await?;
    let requests = action_requests(&quote, signer.address(), run.plan.from.chain.caip2)?;
    for (action, request) in quote.actions.iter().zip(requests) {
        let hash = source
            .send(signer, request, &format!("rfq action '{}'", action.id))
            .await?;
        output.kv(&format!("action {}", action.id), &hash.to_string());
        run.report.transactions.push(ActionTx {
            action: action.id.clone(),
            tx_hash: hash.to_string(),
        });
    }
    Ok(quote.quote_id)
}

/// Watch every sent route's status concurrently, then settle each one.
async fn watch_routes(
    client: &RfqClient,
    sent: &[Option<String>],
    timeout: Duration,
    runs: &mut [RouteRun],
    output: Output,
) {
    let watches: Vec<_> = runs
        .iter()
        .zip(sent)
        .filter_map(|(run, quote_id)| {
            let quote_id = quote_id.as_deref()?;
            let route = run.plan.route;
            Some(watch_status(
                move || client.status(quote_id),
                quote_id,
                route,
                timeout,
                output,
            ))
        })
        .collect();
    if watches.is_empty() {
        return;
    }
    output.section(&format!("watching {} transfers", watches.len()));
    let mut ends = join_all(watches).await.into_iter();
    for (run, quote_id) in runs.iter_mut().zip(sent) {
        let Some(quote_id) = quote_id else {
            continue;
        };
        let Some(end) = ends.next() else {
            break;
        };
        let result = end.and_then(|end| settle(end, quote_id, timeout, &mut run.report));
        run.report
            .finish(result.as_ref().err(), run.started.elapsed());
    }
}

async fn request_quote(client: &RfqClient, run: &mut RouteRun, output: Output) -> Result<Quote> {
    let outcome = client
        .quote_from(&run.report.request, run.plan.backends)
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
    output.kv(
        "quote",
        &format!(
            "{} ({}, receives {}, ~{} s)",
            quote.quote_id, quote.backend.kind, quote.output.amount, quote.estimated_time_seconds
        ),
    );
    run.report.record_quote(&quote);
    Ok(quote)
}

/// Every action as a transaction request, each checked to be an EVM
/// transaction on the source chain from the sender before any is sent.
fn action_requests(
    quote: &Quote,
    sender: Address,
    source_chain: &str,
) -> Result<Vec<TransactionRequest>> {
    if quote.actions.is_empty() {
        return Err(eyre!("quote {} has no actions", quote.quote_id));
    }
    quote
        .actions
        .iter()
        .map(|action| action_request(action, sender, source_chain))
        .collect()
}

fn settle(end: PollEnd, quote_id: &str, timeout: Duration, report: &mut RouteReport) -> Result<()> {
    match end {
        PollEnd::Settled(status) => {
            let state = status.state;
            report.status = Some(status);
            if state == TransferState::Done {
                return Ok(());
            }
            Err(eyre!("quote {quote_id} ended {}", state.label()))
        }
        PollEnd::TimedOut(last) => {
            let error = eyre!(
                "quote {quote_id} did not reach DONE within {} s (last state: {})",
                timeout.as_secs(),
                last.as_ref()
                    .map_or("never read", |status| status.state.label())
            );
            report.status = last;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::commands::intents::types::StatusResponse;
    use crate::types::Network;

    const FUJI: &str = "eip155:43113";
    const TIMEOUT: Duration = Duration::from_secs(2700);

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

    fn its_report() -> RouteReport {
        let plan =
            RoutePlan::resolve(Network::Testnet, RfqRoute::Its, RfqSource::Avalanche).unwrap();
        let request = plan.quote_request(
            U256::from(plan.default_amount),
            sender(),
            plan.recipient(None, sender(), None).unwrap(),
        );
        RouteReport::new(RfqRoute::Its, request)
    }

    #[test]
    fn builds_every_action_in_order_on_the_source_chain_from_the_sender() {
        let dead = sender().to_string();
        let quote = quote(&json!([
            evm_action("approve", FUJI, &dead),
            evm_action("transfer", FUJI, &dead)
        ]));

        let requests = action_requests(&quote, sender(), FUJI).unwrap();

        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.chain_id == Some(43_113))
        );
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
            assert!(action_requests(&quote(&actions), sender(), FUJI).is_err());
        }
    }

    #[test]
    fn only_done_settles_as_a_pass() {
        let mut report = its_report();
        settle(
            PollEnd::Settled(status("DONE")),
            "its_quote",
            TIMEOUT,
            &mut report,
        )
        .unwrap();
        assert_eq!(
            report.status.map(|status| status.state),
            Some(TransferState::Done)
        );

        for (state, label) in [
            ("FAILED", "failed"),
            ("NOT_FOUND", "not found"),
            ("REFUNDED", "refunded"),
        ] {
            let mut report = its_report();
            let error = settle(
                PollEnd::Settled(status(state)),
                "its_quote",
                TIMEOUT,
                &mut report,
            )
            .unwrap_err();
            assert_eq!(error.to_string(), format!("quote its_quote ended {label}"));
            assert!(report.status.is_some());
        }
    }

    #[test]
    fn a_timeout_fails_with_the_last_state_read() {
        let mut report = its_report();
        let error = settle(
            PollEnd::TimedOut(Some(status("REFUND_INITIATED"))),
            "its_quote",
            TIMEOUT,
            &mut report,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "quote its_quote did not reach DONE within 2700 s (last state: refund initiated)"
        );
        assert_eq!(
            report.status.map(|status| status.state),
            Some(TransferState::RefundInitiated)
        );

        let error = settle(
            PollEnd::TimedOut(None),
            "its_quote",
            TIMEOUT,
            &mut its_report(),
        )
        .unwrap_err();
        assert!(error.to_string().ends_with("(last state: never read)"));
    }
}
