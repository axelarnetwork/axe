mod checks;
mod poll;
mod report;
mod routes;
mod source;
mod types;

use std::future::Future;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, TxHash};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, eyre};
use futures::future::join_all;

use self::checks::{check_delivery, checked_requests};
use self::poll::{PollEnd, watch_status};
use self::report::{ActionTx, Output, RouteReport, RunSummary, UncertainSubmission};
use self::routes::RoutePlan;
use self::source::SourceChain;
use crate::commands::intents::client::RfqClient;
use crate::commands::intents::execution::validate_quote_lifetime;
use crate::commands::intents::execution_lock::ExecutionLock;
use crate::commands::intents::types::{
    ActionKind, Quote, QuoteOutcome, QuoteRequest, TimedQuote, TransferState, parse_amount,
};
use crate::evm::UnconfirmedBroadcast;
use crate::ui;

pub use self::types::{RfqRoute, RfqSource, RfqTestArgs, Wallet};

/// One route's plan and what checking it produced.
struct RouteRun {
    plan: RoutePlan,
    report: RouteReport,
    started: Instant,
}

/// A route whose transaction went out, to be watched.
struct Submitted {
    quote: Quote,
    /// The mined transaction, or every broadcast when none was seen mined.
    transaction_hashes: Vec<TxHash>,
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
    let mut runs = plan_routes(&args)?;
    let dry_run = matches!(args.wallet, Wallet::DryRun(_));
    output.announce_run(args.network, args.wallet.address(), dry_run);
    match &args.wallet {
        Wallet::DryRun(sender) => quote_routes(&client, *sender, &mut runs, output).await,
        Wallet::Signer(signer) => {
            let submitted = send_routes(&args, &client, signer, &mut runs, output).await;
            watch_routes(&client, submitted, args.timeout, &mut runs, output).await;
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

/// Each route's plan and quote request. A route that cannot form a request
/// is reported as failed without stopping the others.
fn plan_routes(args: &RfqTestArgs) -> Result<Vec<RouteRun>> {
    if args.amount.is_some() && args.routes.len() > 1 {
        return Err(eyre!("--amount applies to a single --route"));
    }
    if let Some(route) = args
        .routes
        .iter()
        .enumerate()
        .find_map(|(index, route)| args.routes[..index].contains(route).then_some(route))
    {
        return Err(eyre!("--route {route} is given more than once"));
    }
    let sender = args.wallet.address();
    args.routes
        .iter()
        .map(|&route| {
            let plan = RoutePlan::resolve(args.network, route, args.source)?;
            let request = plan
                .recipient(args.recipient, args.solana_recipient, sender)
                .map(|recipient| plan.quote_request(plan.amount(args.amount), sender, recipient));
            let mut report = RouteReport::new(route, request.as_ref().ok().cloned());
            if let Err(error) = &request {
                report.finish(Some(error), Duration::ZERO);
            }
            Ok(RouteRun {
                plan,
                report,
                started: Instant::now(),
            })
        })
        .collect()
}

async fn quote_routes(client: &RfqClient, sender: Address, runs: &mut [RouteRun], output: Output) {
    for run in runs {
        let Some(request) = run.report.request.clone() else {
            continue;
        };
        output.announce_route(&run.plan, &request);
        run.started = Instant::now();
        let result = quote_route(client, sender, run, &request, output).await;
        run.report
            .finish(result.as_ref().err(), run.started.elapsed());
    }
}

async fn quote_route(
    client: &RfqClient,
    sender: Address,
    run: &mut RouteRun,
    request: &QuoteRequest,
    output: Output,
) -> Result<()> {
    let quote = request_quote(client, run, request, output).await?;
    checked_requests(&quote, &run.plan, request, sender)?;
    run.report.actions = Some(quote.actions);
    Ok(())
}

/// Send every route's actions in turn while holding the wallet lock.
/// Returns, per route, what was submitted.
async fn send_routes(
    args: &RfqTestArgs,
    client: &RfqClient,
    signer: &PrivateKeySigner,
    runs: &mut [RouteRun],
    output: Output,
) -> Vec<Option<Submitted>> {
    let mut submitted: Vec<Option<Submitted>> = runs.iter().map(|_| None).collect();
    let Some(chain) = runs.first().map(|run| run.plan.from.chain) else {
        return submitted;
    };
    let connected = async {
        let lock = ExecutionLock::acquire(signer.address())?;
        let source = SourceChain::connect(args.network, args.config.clone(), chain).await?;
        Ok::<_, eyre::Report>((lock, source))
    }
    .await;
    let (_lock, source) = match connected {
        Ok(connected) => connected,
        Err(error) => {
            for run in runs.iter_mut().filter(|run| run.report.error.is_none()) {
                run.report.finish(Some(&error), Duration::ZERO);
            }
            return submitted;
        }
    };
    for (run, slot) in runs.iter_mut().zip(&mut submitted) {
        let Some(request) = run.report.request.clone() else {
            continue;
        };
        output.announce_route(&run.plan, &request);
        run.started = Instant::now();
        match send_route(client, &source, signer, run, &request, output).await {
            Ok(sent) => *slot = Some(sent),
            Err(error) => run.report.finish(Some(&error), run.started.elapsed()),
        }
    }
    submitted
}

async fn send_route(
    client: &RfqClient,
    source: &SourceChain,
    signer: &PrivateKeySigner,
    run: &mut RouteRun,
    request: &QuoteRequest,
    output: Output,
) -> Result<Submitted> {
    if run.plan.wrap_shortfall {
        let amount = parse_amount(&request.amount)?;
        let wrap = source
            .wrap_shortfall(signer, run.plan.from.token, amount, output)
            .await?;
        run.report.wrap_tx = wrap.map(|hash| hash.to_string());
    }
    let quote = request_quote(client, run, request, output).await?;
    let requests = checked_requests(&quote, &run.plan, request, signer.address())?;
    let send = |request, label: String| async move { source.send(signer, request, &label).await };
    let transaction_hashes = send_actions(send, &quote, requests, &mut run.report, output).await?;
    Ok(Submitted {
        quote,
        transaction_hashes,
    })
}

/// Send the checked actions in order. Returns the transaction's hashes: the
/// mined one, or every broadcast when its rounds ran out without a receipt,
/// since the transfer may still land.
async fn send_actions<F, Fut>(
    mut send: F,
    quote: &Quote,
    requests: Vec<TransactionRequest>,
    report: &mut RouteReport,
    output: Output,
) -> Result<Vec<TxHash>>
where
    F: FnMut(TransactionRequest, String) -> Fut,
    Fut: Future<Output = Result<TxHash>>,
{
    let mut transaction_hashes = Vec::new();
    for (action, request) in quote.actions.iter().zip(requests) {
        validate_quote_lifetime(quote)?;
        match send(request, format!("rfq action '{}'", action.id)).await {
            Ok(hash) => {
                output.kv(&format!("action {}", action.id), &hash.to_string());
                report.transactions.push(ActionTx {
                    action: action.id.clone(),
                    tx_hash: hash.to_string(),
                });
                if action.kind == ActionKind::Transaction {
                    transaction_hashes = vec![hash];
                }
            }
            Err(error) => {
                let Some(unconfirmed) = error.downcast_ref::<UnconfirmedBroadcast>() else {
                    return Err(error);
                };
                report.uncertain_submission = Some(UncertainSubmission {
                    action: action.id.clone(),
                    tx_hashes: unconfirmed.hashes.iter().map(ToString::to_string).collect(),
                    error: ui::scrub_urls(&format!("{error:#}")),
                });
                if action.kind != ActionKind::Transaction {
                    return Err(error);
                }
                ui::warn(&format!(
                    "{}: watching quote {} anyway",
                    ui::scrub_urls(&error.to_string()),
                    quote.quote_id
                ));
                return Ok(unconfirmed.hashes.clone());
            }
        }
    }
    Ok(transaction_hashes)
}

/// Watch every submitted route's status concurrently, then settle each one.
/// An interrupt settles the routes still being watched as failed, so the
/// summary is still written.
async fn watch_routes(
    client: &RfqClient,
    submitted: Vec<Option<Submitted>>,
    timeout: Duration,
    runs: &mut [RouteRun],
    output: Output,
) {
    let watches: Vec<_> = runs
        .iter()
        .zip(&submitted)
        .filter_map(|(run, sent)| {
            let quote_id = sent.as_ref()?.quote.quote_id.as_str();
            let (route, started) = (run.plan.route, run.started);
            Some(async move {
                let end =
                    watch_status(|| client.status(quote_id), quote_id, route, timeout, output)
                        .await;
                (end, started.elapsed())
            })
        })
        .collect();
    if watches.is_empty() {
        return;
    }
    output.section(&format!("watching {} transfers", watches.len()));
    let interrupted = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let ends = tokio::select! {
        ends = join_all(watches) => Some(ends),
        () = interrupted => None,
    };
    let mut ends = ends.map(Vec::into_iter);
    for (run, sent) in runs.iter_mut().zip(submitted) {
        let Some(sent) = sent else {
            continue;
        };
        match ends.as_mut().and_then(Iterator::next) {
            Some((end, elapsed)) => {
                let result =
                    end.and_then(|end| settle(end, &sent, &run.plan, timeout, &mut run.report));
                run.report.finish(result.as_ref().err(), elapsed);
            }
            None => run.report.finish(
                Some(&eyre!(
                    "interrupted before quote {} reached DONE",
                    sent.quote.quote_id
                )),
                run.started.elapsed(),
            ),
        }
    }
}

async fn request_quote(
    client: &RfqClient,
    run: &mut RouteRun,
    request: &QuoteRequest,
    output: Output,
) -> Result<Quote> {
    let outcome = client.quote_from(request, run.plan.backends).await?;
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

fn settle(
    end: PollEnd,
    sent: &Submitted,
    plan: &RoutePlan,
    timeout: Duration,
    report: &mut RouteReport,
) -> Result<()> {
    let quote_id = &sent.quote.quote_id;
    match end {
        PollEnd::Settled(status) => {
            let state = status.state;
            let delivered = (state == TransferState::Done)
                .then(|| check_delivery(&status, plan, &sent.quote, &sent.transaction_hashes));
            report.status = Some(status);
            match delivered {
                Some(delivered) => delivered,
                None => Err(eyre!("quote {quote_id} ended {}", state.label())),
            }
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
    use alloy::primitives::U256;
    use serde_json::json;

    use super::*;
    use crate::commands::intents::types::StatusResponse;
    use crate::types::Network;

    const TIMEOUT: Duration = Duration::from_secs(2700);
    const SENDER: Address =
        alloy::primitives::address!("0x000000000000000000000000000000000000dEaD");

    fn args(routes: Vec<RfqRoute>) -> RfqTestArgs {
        RfqTestArgs {
            network: Network::Testnet,
            routes,
            source: RfqSource::Avalanche,
            amount: None,
            recipient: None,
            solana_recipient: None,
            timeout: TIMEOUT,
            rfq_url: None,
            config: None,
            wallet: Wallet::DryRun(SENDER),
            json: true,
        }
    }

    fn its_plan() -> RoutePlan {
        RoutePlan::resolve(Network::Testnet, RfqRoute::Its, RfqSource::Avalanche).unwrap()
    }

    fn submitted(sent: TxHash) -> Submitted {
        let plan = its_plan();
        Submitted {
            quote: serde_json::from_value(json!({
                "quoteId": "its_quote",
                "selectionReason": "only_route",
                "backend": { "type": "its", "name": "axelar", "tracking": {}, "metadata": {} },
                "estimatedTimeSeconds": 60,
                "validity": { "type": "expires_at", "quoteExpiresAt": "2026-11-13T23:02:45Z" },
                "input": { "chain": plan.from.chain.caip2, "token": plan.from.token, "amount": "100" },
                "output": { "chain": plan.to.chain.caip2, "token": plan.to.token, "amount": "100" },
                "fees": { "integrator": null },
                "actions": []
            }))
            .unwrap(),
            transaction_hashes: vec![sent],
        }
    }

    fn status(state: &str, source_tx: TxHash, amount: &str) -> StatusResponse {
        let plan = its_plan();
        serde_json::from_value(json!({
            "quoteId": "its_quote",
            "state": state,
            "backend": { "type": "its", "name": "axelar", "tracking": {}, "metadata": {} },
            "source": {
                "chain": plan.from.chain.caip2,
                "txHash": source_tx.to_string(),
                "messageId": "0xmessage-1",
                "timestamp": "2026-11-13T23:02:45Z"
            },
            "destination": null,
            "output": { "chain": plan.to.chain.caip2, "token": plan.to.token, "amount": amount },
            "refund": null,
            "details": {}
        }))
        .unwrap()
    }

    fn report() -> RouteReport {
        RouteReport::new(RfqRoute::Its, None)
    }

    fn quote_with_actions(expires_at: &str) -> Quote {
        let action = |id: &str, kind: &str| {
            json!({
                "id": id,
                "label": id,
                "type": kind,
                "chain": "eip155:43113",
                "payload": {
                    "type": "evm_transaction",
                    "from": SENDER.to_string(),
                    "to": SENDER.to_string(),
                    "data": "0x",
                    "value": "0"
                }
            })
        };
        let mut quote = submitted(TxHash::ZERO).quote;
        quote.validity.quote_expires_at = expires_at.parse().unwrap();
        quote.actions = serde_json::from_value(json!([
            action("approve", "approval"),
            action("transfer", "transaction")
        ]))
        .unwrap();
        quote
    }

    fn unconfirmed(hashes: Vec<TxHash>) -> eyre::Report {
        eyre::Report::new(UnconfirmedBroadcast {
            label: "rfq action 'transfer'".to_owned(),
            hashes,
        })
    }

    /// Sends by answering each action with the next scripted result.
    async fn send_scripted(
        quote: &Quote,
        answers: Vec<Result<TxHash>>,
        report: &mut RouteReport,
    ) -> (Result<Vec<TxHash>>, Vec<String>) {
        let mut answers = answers.into_iter();
        let mut labels = Vec::new();
        let send = |_request, label: String| {
            labels.push(label);
            std::future::ready(answers.next().unwrap_or_else(|| Err(eyre!("no answer"))))
        };
        let requests = vec![TransactionRequest::default(); quote.actions.len()];
        let result = send_actions(send, quote, requests, report, Output::Json).await;
        (result, labels)
    }

    #[tokio::test]
    async fn an_unconfirmed_transfer_is_still_watched_with_every_broadcast() {
        let quote = quote_with_actions("2099-01-01T00:00:00Z");
        let broadcasts = vec![TxHash::from([4u8; 32]), TxHash::from([5u8; 32])];
        let mut report = report();

        let (hashes, labels) = send_scripted(
            &quote,
            vec![
                Ok(TxHash::from([3u8; 32])),
                Err(unconfirmed(broadcasts.clone())),
            ],
            &mut report,
        )
        .await;

        assert_eq!(hashes.unwrap(), broadcasts);
        assert_eq!(labels, ["rfq action 'approve'", "rfq action 'transfer'"]);
        assert_eq!(report.transactions.len(), 1);
        let uncertain = report.uncertain_submission.unwrap();
        assert_eq!(uncertain.action, "transfer");
        assert_eq!(uncertain.tx_hashes.len(), 2);
        assert!(uncertain.error.contains("no receipt after"));
    }

    #[tokio::test]
    async fn an_unconfirmed_approval_or_a_failed_send_stops_the_route() {
        let quote = quote_with_actions("2099-01-01T00:00:00Z");

        let mut report_unconfirmed = report();
        let (result, labels) = send_scripted(
            &quote,
            vec![Err(unconfirmed(vec![TxHash::from([4u8; 32])]))],
            &mut report_unconfirmed,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(labels.len(), 1);
        assert_eq!(
            report_unconfirmed
                .uncertain_submission
                .map(|uncertain| uncertain.action),
            Some("approve".to_owned())
        );

        let mut report_failed = report();
        let (result, _) = send_scripted(
            &quote,
            vec![Ok(TxHash::from([3u8; 32])), Err(eyre!("transfer reverted"))],
            &mut report_failed,
        )
        .await;
        assert!(result.is_err());
        assert!(report_failed.uncertain_submission.is_none());
    }

    #[tokio::test]
    async fn an_expired_quote_is_not_sent() {
        let quote = quote_with_actions("2020-01-01T00:00:00Z");
        let (result, labels) = send_scripted(&quote, vec![Ok(TxHash::ZERO)], &mut report()).await;

        assert!(result.is_err());
        assert!(labels.is_empty());
    }

    #[test]
    fn a_route_without_a_request_fails_alone() {
        let runs = plan_routes(&args(vec![RfqRoute::Its, RfqRoute::Solana])).unwrap();

        assert!(runs[0].report.error.is_none());
        assert!(runs[0].report.request.is_some());
        assert!(runs[1].report.request.is_none());
        assert!(
            runs[1]
                .report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("--solana-recipient"))
        );
    }

    #[test]
    fn repeated_routes_and_a_shared_amount_are_refused() {
        assert!(plan_routes(&args(vec![RfqRoute::Its, RfqRoute::Gateway, RfqRoute::Its])).is_err());
        let mut shared = args(vec![RfqRoute::Its, RfqRoute::Gateway]);
        shared.amount = Some(U256::from(1u8));
        assert!(plan_routes(&shared).is_err());
    }

    #[test]
    fn done_passes_only_with_our_transaction_and_the_quoted_output() {
        let ours = TxHash::from([1u8; 32]);
        let sent = submitted(ours);

        let mut passed = report();
        settle(
            PollEnd::Settled(status("DONE", ours, "100")),
            &sent,
            &its_plan(),
            TIMEOUT,
            &mut passed,
        )
        .unwrap();
        assert_eq!(
            passed.status.map(|status| status.state),
            Some(TransferState::Done)
        );

        let mut short = report();
        let error = settle(
            PollEnd::Settled(status("DONE", ours, "99")),
            &sent,
            &its_plan(),
            TIMEOUT,
            &mut short,
        )
        .unwrap_err();
        assert!(error.to_string().contains("below the quoted 100"));
        assert!(short.status.is_some());

        let error = settle(
            PollEnd::Settled(status("DONE", TxHash::from([2u8; 32]), "100")),
            &sent,
            &its_plan(),
            TIMEOUT,
            &mut report(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not send"));
    }

    #[test]
    fn other_settled_states_fail_with_their_label() {
        let ours = TxHash::from([1u8; 32]);
        for (state, label) in [
            ("FAILED", "failed"),
            ("NOT_FOUND", "not found"),
            ("REFUND_PENDING", "refund pending"),
            ("REFUNDED", "refunded"),
        ] {
            let mut report = report();
            let error = settle(
                PollEnd::Settled(status(state, ours, "100")),
                &submitted(ours),
                &its_plan(),
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
        let ours = TxHash::from([1u8; 32]);
        let mut timed_out = report();
        let error = settle(
            PollEnd::TimedOut(Some(status("PENDING", ours, "100"))),
            &submitted(ours),
            &its_plan(),
            TIMEOUT,
            &mut timed_out,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "quote its_quote did not reach DONE within 2700 s (last state: pending)"
        );
        assert!(timed_out.status.is_some());

        let error = settle(
            PollEnd::TimedOut(None),
            &submitted(ours),
            &its_plan(),
            TIMEOUT,
            &mut report(),
        )
        .unwrap_err();
        assert!(error.to_string().ends_with("(last state: never read)"));
    }
}
