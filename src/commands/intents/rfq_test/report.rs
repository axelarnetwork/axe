use std::time::Duration;

use alloy::primitives::Address;
use chrono::Utc;
use eyre::{Result, eyre};
use serde::Serialize;
use serde_json::json;

use super::routes::RoutePlan;
use super::types::{RfqRoute, RfqSource};
use crate::commands::intents::read::render_status;
use crate::commands::intents::types::{Action, BackendType, Quote, QuoteRequest, StatusResponse};
use crate::types::Network;
use crate::ui;

/// Where progress goes: stdout for a person, stderr when stdout carries the
/// JSON summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    Human,
    Json,
}

impl Output {
    pub const fn new(json: bool) -> Self {
        if json { Self::Json } else { Self::Human }
    }

    pub fn section(self, title: &str) {
        match self {
            Self::Human => ui::section(title),
            Self::Json => eprintln!("\n-- {title} --"),
        }
    }

    pub fn kv(self, key: &str, value: &str) {
        match self {
            Self::Human => ui::kv(key, value),
            Self::Json => eprintln!("  {key}: {value}"),
        }
    }

    pub fn announce_run(self, network: Network, sender: Address, dry_run: bool) {
        self.section("rfq route check");
        self.kv("network", &network.to_string());
        self.kv("sender", &sender.to_string());
        if dry_run {
            self.kv("mode", "dry run, nothing is sent");
        }
    }

    pub fn announce_route(self, plan: &RoutePlan, request: &QuoteRequest) {
        self.section(&format!("{} route", plan.route));
        self.kv(
            "from",
            &format!("{} {}", plan.from.chain.label, plan.from.token),
        );
        self.kv("to", &format!("{} {}", plan.to.chain.label, plan.to.token));
        self.kv("amount", &request.amount);
        self.kv("recipient", &request.recipient);
    }

    pub fn transition(self, route: RfqRoute, status: &StatusResponse) {
        let context = json!({
            "source": status.source,
            "destination": status.destination,
            "details": status.details,
        });
        self.kv(
            &format!("{} {route}", utc_clock()),
            &format!("{} {context}", status.state.label()),
        );
    }

    pub fn poll_failed(self, route: RfqRoute, error: &eyre::Report) {
        ui::warn(&format!(
            "{} {route} status poll failed; retrying: {}",
            utc_clock(),
            ui::scrub_urls(&error.to_string())
        ));
    }

    pub fn summary(self, summary: &RunSummary) -> Result<()> {
        match self {
            Self::Json => println!("{}", serde_json::to_string_pretty(summary)?),
            Self::Human => {
                for route in &summary.routes {
                    human_route_summary(route, summary.dry_run)?;
                }
            }
        }
        Ok(())
    }
}

fn human_route_summary(route: &RouteReport, dry_run: bool) -> Result<()> {
    ui::section(&format!("{} route result", route.route));
    if let Some(actions) = &route.actions {
        println!("{}", serde_json::to_string_pretty(actions)?);
    }
    if let Some(status) = &route.status {
        render_status(status, false)?;
    }
    if let Some(uncertain) = &route.uncertain_submission {
        ui::warn(&format!(
            "{} was broadcast without a receipt: {} ({})",
            uncertain.action,
            uncertain.tx_hashes.join(", "),
            uncertain.error
        ));
    }
    let elapsed = ui::format_duration(Duration::from_secs(route.elapsed_secs));
    match (&route.error, dry_run) {
        (Some(error), _) => ui::error(&format!("{} route failed: {error}", route.route)),
        (None, true) => ui::success(&format!("{} route quoted", route.route)),
        (None, false) => ui::success(&format!("{} route reached DONE in {elapsed}", route.route)),
    }
    Ok(())
}

/// One quote action sent on the source chain.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionTx {
    pub action: String,
    pub tx_hash: String,
}

/// An action whose broadcasts ran out without a receipt. Any of them may
/// still be mined.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UncertainSubmission {
    pub action: String,
    pub tx_hashes: Vec<String>,
    pub error: String,
}

/// What checking one route did.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteReport {
    pub route: RfqRoute,
    /// The quote request, or `None` when the route could not form one.
    pub request: Option<QuoteRequest>,
    pub quote_id: Option<String>,
    pub backend: Option<BackendType>,
    pub output_amount: Option<String>,
    pub wrap_tx: Option<String>,
    pub transactions: Vec<ActionTx>,
    pub uncertain_submission: Option<UncertainSubmission>,
    /// The quote's actions, reported by a dry run only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<Action>>,
    /// The settled status, or the last one read before a timeout.
    pub status: Option<StatusResponse>,
    pub elapsed_secs: u64,
    /// Why the route failed. A route without one passed.
    pub error: Option<String>,
}

impl RouteReport {
    pub const fn new(route: RfqRoute, request: Option<QuoteRequest>) -> Self {
        Self {
            route,
            request,
            quote_id: None,
            backend: None,
            output_amount: None,
            wrap_tx: None,
            transactions: Vec::new(),
            uncertain_submission: None,
            actions: None,
            status: None,
            elapsed_secs: 0,
            error: None,
        }
    }

    pub fn record_quote(&mut self, quote: &Quote) {
        self.quote_id = Some(quote.quote_id.clone());
        self.backend = Some(quote.backend.kind);
        self.output_amount = Some(quote.output.amount.clone());
    }

    pub fn finish(&mut self, error: Option<&eyre::Report>, elapsed: Duration) {
        self.elapsed_secs = elapsed.as_secs();
        self.error = error.map(|error| ui::scrub_urls(&format!("{error:#}")));
    }
}

/// What one run did, across its routes.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub network: Network,
    pub source: RfqSource,
    pub dry_run: bool,
    pub elapsed_secs: u64,
    pub routes: Vec<RouteReport>,
}

impl RunSummary {
    /// The run fails when any route failed.
    pub fn verdict(&self) -> Result<()> {
        let failed: Vec<String> = self
            .routes
            .iter()
            .filter(|route| route.error.is_some())
            .map(|route| route.route.to_string())
            .collect();
        if failed.is_empty() {
            return Ok(());
        }
        Err(eyre!(
            "{} of {} rfq routes failed: {}",
            failed.len(),
            self.routes.len(),
            failed.join(", ")
        ))
    }
}

fn utc_clock() -> String {
    Utc::now().format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::intents::types::OrderType;

    fn report(route: RfqRoute) -> RouteReport {
        RouteReport::new(
            route,
            Some(QuoteRequest {
                from_chain: "eip155:43113".to_owned(),
                from_token: "0xfrom".to_owned(),
                to_chain: "eip155:11155111".to_owned(),
                to_token: "0xto".to_owned(),
                amount: "1".to_owned(),
                order_type: OrderType::ExactInput,
                sender: "0xsender".to_owned(),
                recipient: "0xrecipient".to_owned(),
            }),
        )
    }

    #[test]
    fn finishing_records_the_elapsed_time_and_a_scrubbed_error() {
        let mut passed = report(RfqRoute::Its);
        passed.finish(None, Duration::from_secs(90));
        assert_eq!(passed.elapsed_secs, 90);
        assert_eq!(passed.error, None);

        let mut failed = report(RfqRoute::Its);
        failed.finish(
            Some(&eyre!("rpc https://key.example/abc failed")),
            Duration::from_secs(5),
        );
        let error = failed.error.unwrap();
        assert!(error.contains("failed"));
        assert!(!error.contains("key.example"));
    }

    #[test]
    fn the_run_fails_when_any_route_failed_and_names_them() {
        let mut gateway = report(RfqRoute::Gateway);
        gateway.finish(Some(&eyre!("no quote")), Duration::ZERO);
        let mut summary = RunSummary {
            network: Network::Testnet,
            source: RfqSource::Avalanche,
            dry_run: false,
            elapsed_secs: 0,
            routes: vec![report(RfqRoute::Its)],
        };
        assert!(summary.verdict().is_ok());

        summary.routes.push(gateway);
        assert_eq!(
            summary.verdict().unwrap_err().to_string(),
            "1 of 2 rfq routes failed: gateway"
        );
    }

    #[test]
    fn the_summary_is_camel_case_json_without_dry_run_actions() {
        let summary = serde_json::to_value(RunSummary {
            network: Network::Testnet,
            source: RfqSource::Avalanche,
            dry_run: false,
            elapsed_secs: 0,
            routes: vec![report(RfqRoute::NativeWrap)],
        })
        .unwrap();

        assert_eq!(summary["dryRun"], false);
        assert_eq!(summary["source"], "avalanche");
        let route = &summary["routes"][0];
        assert_eq!(route["route"], "native-wrap");
        assert_eq!(route["request"]["orderType"], "EXACT_INPUT");
        assert!(route.get("actions").is_none());
        assert!(route["error"].is_null());
    }
}
