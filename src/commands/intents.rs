mod benchmark;
mod client;
mod execution;
mod execution_lock;
mod inventory;
mod presentation;
mod read;
mod route;
mod stats;
mod stress;
mod traffic;
mod types;

use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, WrapErr, eyre};
use indicatif::ProgressBar;

use self::client::RfqClient;
use self::execution::{ExecutionFeedback, execute_planned_leg, execute_round_trip};
use self::execution_lock::ExecutionLock;
use self::presentation::IntentActivity;
use self::route::{
    DiscoveryFeedback, PlanningFeedback, RouteDiscovery, discover_wallet, plan_roundtrip,
    plan_send, plan_sweep, render_plans,
};
use self::stats::percentile;
use self::types::{LegPlan, LegResult, RoutePlan, RunLimits};
use crate::config::ChainsConfig;
use crate::shutdown::{DrainTarget, Shutdown};
use crate::types::Network;
use crate::ui;

pub use self::types::{AssetSpec, AssetType, HumanAmount, OrderType};
pub use benchmark::{
    QuoteBenchmarkArgs, QuoteBenchmarkLimit, QuoteBenchmarkMode, QuoteBenchmarkTarget,
    benchmark_quotes, benchmark_quotes_data,
};
pub use inventory::{InventoryArgs, inventory, inventory_report};
pub use read::{ApiArgs, CatalogArgs, StatusArgs, catalog, catalog_data, status, status_data};
pub use stress::{StressArgs, run as stress};
pub use traffic::{TrafficArgs, run as traffic};

pub fn resolve_quote_sender(
    sender: Option<Address>,
    private_key: Option<String>,
) -> Result<Address> {
    if let Some(sender) = sender {
        return Ok(sender);
    }
    let Some(private_key) = private_key else {
        return Ok(Address::ZERO);
    };
    let signer: PrivateKeySigner = private_key
        .parse()
        .wrap_err("EVM_PRIVATE_KEY is not a valid hex private key")?;
    Ok(signer.address())
}

pub fn resolve_private_key(
    override_key: Option<String>,
    evm_private_key: Option<String>,
    default_private_key: Option<String>,
) -> Result<String> {
    [override_key, evm_private_key, default_private_key]
        .into_iter()
        .flatten()
        .find(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            eyre!(
                "intent execution needs an EVM signing key; set EVM_PRIVATE_KEY or PRIVATE_KEY, or pass --private-key"
            )
        })
}

#[derive(Clone)]
pub struct IntentRuntimeArgs {
    pub network: Network,
    pub rfq_url: Option<String>,
    pub config: PathBuf,
    pub private_key: String,
    pub poll_interval_secs: u64,
    pub fulfillment_timeout_secs: u64,
    pub yes: bool,
    /// Chains a route may use, by axelar id. Empty means any, which is what
    /// the CLI passes: the person running it is the one choosing the routes.
    pub allowed_chains: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum RouteChoice {
    Random {
        wallet_bps: u16,
        order_type: OrderType,
        asset_type: AssetType,
    },
    Explicit {
        from: AssetSpec,
        to: AssetSpec,
        amount: Option<HumanAmount>,
        wallet_bps: u16,
        order_type: OrderType,
        asset_type: AssetType,
    },
}

impl RouteChoice {
    pub fn new(
        from: Option<AssetSpec>,
        to: Option<AssetSpec>,
        amount: Option<HumanAmount>,
        wallet_bps: u16,
        order_type: OrderType,
        asset_type: AssetType,
    ) -> Result<Self> {
        match (from, to, amount) {
            (None, None, None) => Ok(Self::Random {
                wallet_bps,
                order_type,
                asset_type,
            }),
            (Some(from), Some(to), amount) => Ok(Self::Explicit {
                from,
                to,
                amount,
                wallet_bps,
                order_type,
                asset_type,
            }),
            (None, None, Some(_)) => Err(eyre!("--amount requires --from and --to")),
            _ => Err(eyre!("--from and --to must be provided together")),
        }
    }
}

pub struct SendArgs {
    pub runtime: IntentRuntimeArgs,
    pub route: RouteChoice,
    pub recipient: Option<Address>,
}

pub struct QuoteArgs {
    pub runtime: IntentRuntimeArgs,
    pub route: RouteChoice,
    pub sender: Option<Address>,
    pub recipient: Option<Address>,
    pub json: bool,
}

pub struct RoundtripArgs {
    pub runtime: IntentRuntimeArgs,
    pub route: RouteChoice,
}

pub struct SweepArgs {
    pub runtime: IntentRuntimeArgs,
    pub sweeps: u64,
    pub continuous: bool,
    pub dry_run: bool,
    pub wallet_bps: u16,
    pub order_type: OrderType,
    pub asset_type: AssetType,
    /// Stop before a pass that would take the run past this many intents.
    /// `None` lets the sweep count alone bound the run, which is what the CLI
    /// does. A caller spending against a budget sets it.
    pub max_intents: Option<u64>,
}

struct IntentRuntime {
    signer: PrivateKeySigner,
    config: ChainsConfig,
    client: RfqClient,
    limits: RunLimits,
    auto_confirm: bool,
    /// Kept after narrowing the config, only so a route the caller named
    /// itself can be refused by name rather than by absence.
    allowed_chains: Vec<String>,
}

/// A quoted route, with everything the deposit that may follow needs.
struct QuotedRoute {
    runtime: IntentRuntime,
    discovery: RouteDiscovery,
    plan: LegPlan,
    /// Who the quote was requested for, which is the wallet unless the caller
    /// named someone else.
    sender: Address,
}

/// Load the runtime, find the wallet's funded chains, and quote the route.
///
/// The first half of [`quote`], and all of [`plan_quote`]: asking for a quote
/// spends nothing, so the two share everything up to the deposit.
async fn quoted_route(args: QuoteArgs, show_progress: bool) -> Result<QuotedRoute> {
    let startup = IntentActivity::new("Loading intent configuration…", show_progress);
    let runtime = prepare_runtime(args.runtime).await?;
    runtime.check_named_route(&args.route)?;
    let sender = args.sender.unwrap_or_else(|| runtime.signer.address());
    let recipient = args.recipient.unwrap_or(sender);
    startup.bar.set_message("Checking funded chains…");
    let discovery = discover_wallet(
        &runtime.client,
        &runtime.config,
        sender,
        DiscoveryFeedback::Quiet,
    )
    .await?;
    startup.bar.set_message("Requesting intent quote…");
    let plan = plan_send(
        &runtime.client,
        &discovery,
        sender,
        recipient,
        &args.route,
        PlanningFeedback::Hidden,
    )
    .await?;
    drop(startup);

    Ok(QuotedRoute {
        runtime,
        discovery,
        plan,
        sender,
    })
}

/// Quote a route and return it, without offering to deposit it.
///
/// The quote-only half of [`quote`]. Nothing here spends: it reads the
/// wallet's balances and asks the RFQ API what it would pay.
pub async fn plan_quote(args: QuoteArgs) -> Result<read::PlannedQuote> {
    let quoted = quoted_route(args, false).await?;
    Ok(read::PlannedQuote::from_plan(&quoted.plan))
}

pub async fn quote(args: QuoteArgs) -> Result<()> {
    let json = args.json;
    let QuotedRoute {
        runtime,
        discovery,
        plan,
        sender,
    } = quoted_route(args, !json).await?;
    let wallet = runtime.signer.address();

    read::render_planned_quote(&plan, json)?;
    if json {
        return Ok(());
    }
    if sender != wallet {
        ui::warn(
            "The quote sender differs from the axe wallet, so this quote cannot be deposited.",
        );
        return Ok(());
    }
    if !ui::confirm("Deposit this quote and watch it to fulfillment?").await {
        ui::info("Quote not deposited.");
        return Ok(());
    }

    let quote_id = plan.quote.quote.quote_id.clone();
    let _execution_lock = ExecutionLock::acquire(wallet)?;
    let _shutdown = Shutdown::install(DrainTarget::Intent);
    let result = execute_planned_leg(
        &runtime.client,
        &discovery.chains,
        &runtime.signer,
        plan,
        runtime.limits,
        &ExecutionFeedback::Debugger,
    )
    .await?;
    render_summary(std::slice::from_ref(&result), 1);
    ui::success(&format!("Intent {quote_id} fulfilled successfully."));
    Ok(())
}

pub async fn send(args: SendArgs) -> Result<LegResult> {
    let startup = IntentActivity::new("Loading intent configuration…", true);
    let runtime = prepare_runtime(args.runtime).await?;
    runtime.check_named_route(&args.route)?;
    startup.bar.set_message("Checking funded chains…");
    let discovery = discover_wallet(
        &runtime.client,
        &runtime.config,
        runtime.signer.address(),
        DiscoveryFeedback::Quiet,
    )
    .await?;
    let recipient = args.recipient.unwrap_or_else(|| runtime.signer.address());
    startup.bar.set_message("Finding an available route…");
    let plan = plan_send(
        &runtime.client,
        &discovery,
        runtime.signer.address(),
        recipient,
        &args.route,
        PlanningFeedback::Hidden,
    )
    .await?;
    drop(startup);
    route::render_leg_plan(&plan);
    confirm_execution(runtime.auto_confirm, "Execute this intent?").await?;
    let _execution_lock = ExecutionLock::acquire(runtime.signer.address())?;
    let _shutdown = Shutdown::install(DrainTarget::Intent);

    let activity = IntentActivity {
        bar: presentation::intent_progress_bar(1, "Starting intent…"),
    };
    let result = execute_planned_leg(
        &runtime.client,
        &discovery.chains,
        &runtime.signer,
        plan,
        runtime.limits,
        &ExecutionFeedback::Progress(activity.bar.clone()),
    )
    .await;
    drop(activity);
    let result = result?;
    render_summary(std::slice::from_ref(&result), 1);
    Ok(result)
}

pub async fn roundtrip(args: RoundtripArgs) -> Result<Vec<LegResult>> {
    let startup = IntentActivity::new("Loading intent configuration…", true);
    let runtime = prepare_runtime(args.runtime).await?;
    runtime.check_named_route(&args.route)?;
    startup.bar.set_message("Checking funded chains…");
    let discovery = discover_wallet(
        &runtime.client,
        &runtime.config,
        runtime.signer.address(),
        DiscoveryFeedback::Quiet,
    )
    .await?;
    startup.bar.set_message("Finding a round-trip route…");
    let plan = plan_roundtrip(
        &runtime.client,
        &discovery,
        runtime.signer.address(),
        &args.route,
    )
    .await?;
    drop(startup);
    render_plans(std::slice::from_ref(&plan));
    let _execution_lock = ExecutionLock::acquire(runtime.signer.address())?;
    let _shutdown = Shutdown::install(DrainTarget::RoundTrip);

    let mut results = Vec::new();
    let activity = IntentActivity {
        bar: presentation::intent_progress_bar(2, "Starting round trip…"),
    };
    let executed = execute_round_trip(
        &runtime.client,
        &discovery.chains,
        &runtime.signer,
        &plan,
        runtime.limits,
        &ExecutionFeedback::Progress(activity.bar.clone()),
        &mut results,
    )
    .await;
    drop(activity);
    render_summary(&results, 2);
    executed.map(|()| results)
}

pub async fn sweep(args: SweepArgs) -> Result<Vec<LegResult>> {
    let runtime = prepare_runtime(args.runtime).await?;
    let _execution_lock = (!args.dry_run)
        .then(|| ExecutionLock::acquire(runtime.signer.address()))
        .transpose()?;
    let shutdown = Shutdown::install(DrainTarget::RoundTrip);
    let mut results = Vec::new();
    let mut planned_intents = 0usize;
    let mut sweep = 0u64;

    loop {
        sweep += 1;
        let startup = IntentActivity::new("Checking funded chains…", true);
        let discovery = discover_wallet(
            &runtime.client,
            &runtime.config,
            runtime.signer.address(),
            DiscoveryFeedback::Quiet,
        )
        .await?;
        drop(startup);
        let plans = plan_sweep(
            &runtime.client,
            &discovery,
            runtime.signer.address(),
            args.asset_type,
            args.wallet_bps,
            args.order_type,
            PlanningFeedback::Visible,
        )
        .await;
        if plans.is_empty() {
            render_summary(&results, planned_intents);
            return Err(eyre!(
                "No {}-to-{} round-trip routes are funded and quoted. Fund matching assets or choose a different --asset-type.",
                args.asset_type.label(),
                args.asset_type.label()
            ));
        }
        if args.dry_run {
            render_plans(&plans);
            return Ok(results);
        }
        let pass_intents = plans.len() * 2;

        // A pass is all or nothing: its round trips are planned together and
        // half a round trip leaves funds on the wrong chain. So a pass that
        // would breach the cap is not started at all.
        if would_exceed(args.max_intents, planned_intents, pass_intents) {
            ui::info(&format!(
                "stopping before sweep {sweep}: its {pass_intents} intents would pass the \
                 cap of {} for this run",
                args.max_intents.unwrap_or_default()
            ));
            break;
        }
        planned_intents += pass_intents;
        ui::info(&format!(
            "sweep {sweep}: {} {} round trips, {pass_intents} intents",
            plans.len(),
            args.asset_type.label()
        ));

        let executed =
            execute_sweep_pass(&runtime, &discovery, &plans, &mut results, &shutdown).await;
        match executed {
            Ok(true) => {}
            Ok(false) => {
                render_summary(&results, planned_intents);
                return Ok(results);
            }
            Err(error) => {
                render_summary(&results, planned_intents);
                return Err(error);
            }
        }

        if !args.continuous && sweep >= args.sweeps {
            break;
        }
        if shutdown.requested() {
            break;
        }
    }

    render_summary(&results, planned_intents);
    Ok(results)
}

/// Whether one more pass would take a capped run past its cap.
fn would_exceed(max_intents: Option<u64>, so_far: usize, next: usize) -> bool {
    max_intents.is_some_and(|max| so_far.saturating_add(next) as u64 > max)
}

async fn execute_sweep_pass(
    runtime: &IntentRuntime,
    discovery: &RouteDiscovery,
    plans: &[RoutePlan],
    results: &mut Vec<LegResult>,
    shutdown: &Shutdown,
) -> Result<bool> {
    let progress = sweep_progress(plans.len() * 2);
    let feedback = ExecutionFeedback::Progress(progress.clone());
    for plan in plans {
        if shutdown.requested() {
            progress.finish_and_clear();
            return Ok(false);
        }
        let executed = execute_round_trip(
            &runtime.client,
            &discovery.chains,
            &runtime.signer,
            plan,
            runtime.limits,
            &feedback,
            results,
        )
        .await;
        if let Err(error) = executed {
            progress.finish_and_clear();
            return Err(error).wrap_err_with(|| {
                format!(
                    "round trip {} -> {} did not complete",
                    plan.from.label(),
                    plan.to.label()
                )
            });
        }
    }
    progress.finish_and_clear();
    Ok(true)
}

fn sweep_progress(total: usize) -> ProgressBar {
    presentation::intent_progress_bar(total as u64, "starting intent sweep")
}

async fn prepare_runtime(args: IntentRuntimeArgs) -> Result<IntentRuntime> {
    let signer: PrivateKeySigner = args
        .private_key
        .parse()
        .wrap_err("intent EVM private key is not valid hex")?;
    let mut config = ChainsConfig::load(&args.config).await?;
    // Every flow discovers its routes by resolving the RFQ catalog against
    // this map, so narrowing it here is what keeps a restricted run inside
    // the chains it was allowed -- including the flows that pick their own
    // routes, which have no route to check up front.
    config.retain_chains(&args.allowed_chains);
    let client = RfqClient::new(args.network, args.rfq_url.as_deref())?;
    let limits = RunLimits {
        poll_interval: Duration::from_secs(args.poll_interval_secs),
        fulfillment_timeout: Duration::from_secs(args.fulfillment_timeout_secs),
    };
    Ok(IntentRuntime {
        signer,
        config,
        client,
        limits,
        auto_confirm: args.yes,
        allowed_chains: args.allowed_chains,
    })
}

impl IntentRuntime {
    /// Refuse a route the caller named that leaves the allowed chains.
    ///
    /// The flows that pick their own routes never need this: the narrowed
    /// config means a disallowed chain is not there to be discovered. A
    /// caller that named its assets would otherwise be told the asset is not
    /// in the catalog, which is true but hides the reason.
    fn check_named_route(&self, route: &RouteChoice) -> Result<()> {
        let RouteChoice::Explicit { from, to, .. } = route else {
            return Ok(());
        };
        if self.allowed_chains.is_empty() {
            return Ok(());
        }

        for asset in [from, to] {
            let chain = &asset.id().chain_id;
            if self.knows_evm_chain(chain) == Some(false) {
                return Err(eyre!(
                    "chain {chain} is not one of the chains this server may use: {}",
                    self.allowed_chains.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Whether the chains config carries this CAIP-2 chain, or `None` when
    /// the id is not one this config could describe. Intent routes are EVM
    /// only, so anything without an `eip155:` reference is left to the
    /// catalog lookup to reject on its own terms.
    fn knows_evm_chain(&self, caip2: &str) -> Option<bool> {
        let reference = caip2.strip_prefix("eip155:")?;
        let chain_id = reference.parse::<u64>().ok()?;
        Some(
            self.config
                .chains
                .values()
                .any(|chain| chain.evm_chain_id == Some(chain_id)),
        )
    }
}

async fn confirm_execution(auto_confirm: bool, prompt: &str) -> Result<()> {
    if auto_confirm || ui::confirm(prompt).await {
        return Ok(());
    }
    Err(eyre!(
        "execution not confirmed; pass --yes for non-interactive runs"
    ))
}

fn render_summary(results: &[LegResult], planned: usize) {
    ui::section("intent result");
    ui::kv(
        "fulfilled",
        &format!(
            "{}/{} ({:.1}%)",
            results.len(),
            planned,
            completion_percentage(results.len(), planned)
        ),
    );
    if results.is_empty() {
        return;
    }
    let quote_latencies: Vec<u64> = results
        .iter()
        .map(|result| result.quote_latency_ms)
        .collect();
    let fulfillment_latencies: Vec<u64> = results
        .iter()
        .map(|result| result.fulfillment_latency_ms)
        .collect();
    let deposit_latencies: Vec<u64> = results
        .iter()
        .map(|result| result.deposit_confirmation_latency_ms)
        .collect();
    let end_to_end_latencies: Vec<u64> = results
        .iter()
        .map(|result| result.end_to_end_latency_ms)
        .collect();
    ui::kv(
        "quote latency",
        &format_latency_percentiles(&quote_latencies),
    );
    ui::kv(
        "deposit confirmation",
        &format_latency_percentiles(&deposit_latencies),
    );
    ui::kv(
        "fulfillment latency",
        &format_latency_percentiles(&fulfillment_latencies),
    );
    ui::kv(
        "end-to-end latency",
        &format_latency_percentiles(&end_to_end_latencies),
    );
}

fn format_latency_percentiles(values: &[u64]) -> String {
    format!(
        "p50 {} | p95 {}",
        ui::format_millis(percentile(values, 50)),
        ui::format_millis(percentile(values, 95))
    )
}

fn completion_percentage(completed: usize, planned: usize) -> f64 {
    if planned == 0 {
        return 0.0;
    }
    completed as f64 / planned as f64 * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank_without_floats() {
        let values = [10, 20, 30, 40, 50];
        assert_eq!(percentile(&values, 50), 30);
        assert_eq!(percentile(&values, 95), 50);
    }

    #[test]
    fn completion_percentage_handles_empty_and_partial_runs() {
        assert_eq!(completion_percentage(0, 0), 0.0);
        assert_eq!(completion_percentage(3, 4), 75.0);
    }

    #[test]
    fn latency_percentiles_are_labeled_and_human_readable() {
        assert_eq!(
            format_latency_percentiles(&[181, 2_924, 8_823]),
            "p50 2.92 s | p95 8.82 s"
        );
    }

    #[test]
    fn resolves_quote_sender_from_override_key_or_zero() {
        let signer = PrivateKeySigner::random();
        let address = signer.address();

        assert_eq!(
            resolve_quote_sender(Some(address), Some("ignored".to_owned())).unwrap(),
            address
        );
        assert_eq!(
            resolve_quote_sender(None, Some(signer.to_bytes().to_string())).unwrap(),
            address
        );
        assert_eq!(resolve_quote_sender(None, None).unwrap(), Address::ZERO);
    }

    #[test]
    fn resolves_intent_key_by_precedence() {
        assert_eq!(
            resolve_private_key(
                Some("override".to_owned()),
                Some("evm".to_owned()),
                Some("default".to_owned()),
            )
            .unwrap(),
            "override"
        );
        assert_eq!(
            resolve_private_key(None, Some("evm".to_owned()), Some("default".to_owned())).unwrap(),
            "evm"
        );
        assert_eq!(
            resolve_private_key(None, None, Some("default".to_owned())).unwrap(),
            "default"
        );
        assert!(resolve_private_key(None, None, None).is_err());
    }
}
