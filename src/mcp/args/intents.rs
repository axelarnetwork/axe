//! The arguments the intent tools accept.
//!
//! Assets arrive as the same `<CAIP-2 chain>/<token address>` text the CLI
//! takes, parsed here rather than typed in the schema: the parse error names
//! the shape it wanted, which is more use to an agent than a schema rejection.
//!
//! As everywhere else in this module, no key, network, RPC or config path
//! appears. The wallet these flows spend from is the operator's.

use std::str::FromStr;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::commands::intents::{
    AssetSpec, AssetType, HumanAmount, IntentRuntimeArgs, OrderType, RouteChoice,
};

/// Basis points of the spendable balance one route may use, when the caller
/// does not say. The CLI's default for a single send.
pub const DEFAULT_WALLET_BPS: u16 = 100;

/// The same, for the traffic simulation, which runs many routes in a row and
/// so takes a smaller bite of each.
pub const DEFAULT_TRAFFIC_WALLET_BPS: u16 = 10;

/// Which route a flow should take.
///
/// Naming neither asset lets the flow pick any executable route, which is
/// what the sweep and traffic flows always do.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RouteArgs {
    /// Source asset as <CAIP-2 chain>/<token address>, for example
    /// eip155:43113/0x5425890298aed601595a70ab815c96711a31bc65. Requires `to`.
    pub from: Option<String>,
    /// Destination asset in the same form. Requires `from`.
    pub to: Option<String>,
    /// Human-readable amount, for example 1.5. Omit to spend `wallet_bps` of
    /// the source balance.
    pub amount: Option<String>,
    /// Fix the input or the output amount. Defaults to exact input.
    pub order_type: Option<OrderType>,
    /// Token-to-token or native-to-native. Defaults to token.
    pub asset_type: Option<AssetType>,
    /// Basis points of the spendable source balance to use when `amount` is
    /// omitted. Defaults to 100, which is one percent.
    #[schemars(range(min = 1, max = 10000))]
    pub wallet_bps: Option<u16>,
}

impl RouteArgs {
    /// The route as the intent flows describe it, or why it could not be read.
    pub fn choice(&self) -> Result<RouteChoice, String> {
        let parts = self.parts()?;
        RouteChoice::new(
            parts.from,
            parts.to,
            parts.amount,
            self.wallet_bps.unwrap_or(DEFAULT_WALLET_BPS),
            parts.order_type,
            parts.asset_type,
        )
        .map_err(|e| format!("{e:#}"))
    }

    /// The same fields parsed, for the benchmark, which takes them
    /// individually rather than as a route choice.
    pub fn parts(&self) -> Result<RouteParts, String> {
        Ok(RouteParts {
            from: parse_asset(self.from.as_deref(), "from")?,
            to: parse_asset(self.to.as_deref(), "to")?,
            amount: parse_amount(self.amount.as_deref())?,
            order_type: self.order_type.unwrap_or_default(),
            asset_type: self.asset_type.unwrap_or_default(),
        })
    }
}

/// A route's arguments, parsed.
pub struct RouteParts {
    pub from: Option<AssetSpec>,
    pub to: Option<AssetSpec>,
    pub amount: Option<HumanAmount>,
    pub order_type: OrderType,
    pub asset_type: AssetType,
}

/// Arguments for the intent catalog.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CatalogArgs {
    /// Show only this CAIP-2 chain id, for example eip155:43113.
    pub chain: Option<String>,
    /// Restrict to token or native assets. Omit for both.
    pub asset_type: Option<AssetType>,
}

/// Arguments for the solver inventory.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InventoryArgs {
    /// Restrict to token or native assets. Omit for both.
    pub asset_type: Option<AssetType>,
}

/// Arguments for a quote.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuoteArgs {
    #[serde(flatten)]
    pub route: RouteArgs,
    /// Destination recipient as a 0x address. Defaults to the operator's
    /// wallet.
    pub recipient: Option<String>,
}

/// Arguments for a quote's status.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusArgs {
    /// Quote id returned by intents_quote or one of the spend flows.
    pub quote_id: String,
}

/// Arguments for the quote benchmark.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuoteBenchArgs {
    #[serde(flatten)]
    pub route: RouteArgs,
    /// Quote requests to measure. Defaults to 100. Cannot be combined with
    /// `duration_secs`.
    #[schemars(range(min = 1, max = 1000))]
    pub requests: Option<u64>,
    /// Measure continuously for this long instead of for a fixed number of
    /// requests. Capped at 300 seconds.
    #[schemars(range(min = 1, max = 300))]
    pub duration_secs: Option<u64>,
    /// Quote requests in flight at once. Defaults to 8.
    #[schemars(range(min = 1, max = 64))]
    pub concurrency: Option<u16>,
    /// Unmeasured requests to run first. Defaults to 10.
    #[schemars(range(max = 100))]
    pub warmup: Option<u64>,
}

/// Arguments for sending one intent.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendArgs {
    #[serde(flatten)]
    pub route: RouteArgs,
    /// Destination recipient as a 0x address. Defaults to the operator's
    /// wallet, which is what keeps the funds recoverable.
    pub recipient: Option<String>,
}

/// Arguments for a round trip: one intent out and one back.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RoundtripArgs {
    #[serde(flatten)]
    pub route: RouteArgs,
}

/// Arguments for a sweep over every executable route.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SweepArgs {
    /// The most intents this run may send. Required: the routes are
    /// discovered while the run goes, so this is what bounds the spend, and
    /// it is reserved against the operator's budget up front.
    #[schemars(range(min = 1))]
    pub max_intents: u64,
    /// Complete passes over every executable route. Defaults to 1. A pass
    /// that would take the run past `max_intents` is not started.
    #[schemars(range(min = 1))]
    pub sweeps: Option<u64>,
    /// Token-to-token or native-to-native. Defaults to token.
    pub asset_type: Option<AssetType>,
    /// Fix the input or the output amount. Defaults to exact input.
    pub order_type: Option<OrderType>,
    /// Basis points of each source balance per route. Defaults to 100.
    #[schemars(range(min = 1, max = 10000))]
    pub wallet_bps: Option<u16>,
}

/// Arguments for the traffic simulation.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrafficArgs {
    /// The most intents this run may send, reserved against the operator's
    /// budget up front.
    #[schemars(range(min = 1))]
    pub max_intents: u64,
    /// Stop after this long, whether or not the intent limit was reached.
    /// Required: traffic otherwise runs until it is interrupted, and nothing
    /// interrupts it here.
    #[schemars(range(min = 1))]
    pub duration_secs: u64,
    /// Restrict to token or native assets. Omit for both.
    pub asset_type: Option<AssetType>,
    /// Basis points of a source balance per route. Defaults to 10.
    #[schemars(range(min = 1, max = 1000))]
    pub wallet_bps: Option<u16>,
}

/// Arguments for the deposit stress run.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StressArgs {
    /// The most deposits this run may broadcast, reserved against the
    /// operator's budget up front.
    #[schemars(range(min = 1))]
    pub max_intents: u64,
    /// Token symbol to deposit from every funded source chain. Defaults to
    /// USDC.
    pub symbol: Option<String>,
    /// Human-readable amount per deposit. Defaults to 0.1.
    pub amount: Option<String>,
    /// Stop after this long. Defaults to 900 seconds.
    #[schemars(range(min = 1))]
    pub duration_secs: Option<u64>,
    /// Deposits in flight at once. Defaults to 32.
    #[schemars(range(min = 1, max = 128))]
    pub max_in_flight: Option<u16>,
    /// Maximum cumulative input volume in token units. Defaults to 20.
    pub max_volume: Option<String>,
    /// Maximum native gas spend per source chain. Defaults to 0.01.
    pub max_native_spend: Option<String>,
    /// Never submit on a chain below this native balance. Defaults to 0.01.
    pub min_native_balance: Option<String>,
}

impl StressArgs {
    /// The run's amounts and limits, parsed.
    ///
    /// Separate from the flow arguments because everything here can be
    /// checked without a key or a config, and so before the operator's budget
    /// is claimed.
    pub fn plan(&self, defaults: StressDefaults) -> Result<StressPlan, String> {
        Ok(StressPlan {
            symbol: self.symbol.clone().unwrap_or_else(|| "USDC".to_string()),
            amount: human_amount(self.amount.as_deref().unwrap_or("0.1"), "amount")?,
            duration: Duration::from_secs(self.duration_secs.unwrap_or(defaults.duration_secs)),
            max_intents: self.max_intents,
            max_in_flight: usize::from(self.max_in_flight.unwrap_or(defaults.in_flight)),
            max_volume: human_amount(self.max_volume.as_deref().unwrap_or("20"), "max_volume")?,
            max_native_spend: human_amount(
                self.max_native_spend.as_deref().unwrap_or("0.01"),
                "max_native_spend",
            )?,
            min_native_balance: human_amount(
                self.min_native_balance.as_deref().unwrap_or("0.01"),
                "min_native_balance",
            )?,
        })
    }
}

/// What a stress run falls back to, from the server's own constants.
#[derive(Debug, Clone, Copy)]
pub struct StressDefaults {
    pub duration_secs: u64,
    pub in_flight: u16,
}

/// A stress run's limits, ready to be joined with a runtime.
#[derive(Debug, Clone)]
pub struct StressPlan {
    pub symbol: String,
    pub amount: HumanAmount,
    pub duration: Duration,
    pub max_intents: u64,
    pub max_in_flight: usize,
    pub max_volume: HumanAmount,
    pub max_native_spend: HumanAmount,
    pub min_native_balance: HumanAmount,
}

impl StressPlan {
    pub fn into_flow(self, runtime: IntentRuntimeArgs) -> crate::commands::intents::StressArgs {
        crate::commands::intents::StressArgs {
            runtime,
            symbol: self.symbol,
            amount: self.amount,
            duration: Some(self.duration),
            max_intents: Some(self.max_intents),
            max_in_flight: self.max_in_flight,
            max_volume: Some(self.max_volume),
            max_native_spend: Some(self.max_native_spend),
            min_native_balance: self.min_native_balance,
            // Data, not a terminal run: the report is written to the run's
            // artifact, not printed for someone to read.
            json: true,
        }
    }
}

fn parse_asset(asset: Option<&str>, field: &str) -> Result<Option<AssetSpec>, String> {
    asset
        .map(|asset| AssetSpec::from_str(asset).map_err(|e| format!("{field}: {e}")))
        .transpose()
}

fn parse_amount(amount: Option<&str>) -> Result<Option<HumanAmount>, String> {
    amount
        .map(|amount| human_amount(amount, "amount"))
        .transpose()
}

fn human_amount(amount: &str, field: &str) -> Result<HumanAmount, String> {
    HumanAmount::from_str(amount).map_err(|e| format!("{field}: {e}"))
}
