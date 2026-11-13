use std::time::Duration;

use chrono::Utc;
use eyre::Result;
use serde::Serialize;
use serde_json::json;

use super::routes::RoutePlan;
use super::types::{RfqRoute, RfqSource};
use crate::commands::intents::types::{
    Action, BackendType, Quote, QuoteRequest, StatusResponse, TransferState,
};
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

    pub fn warn(self, message: &str) {
        match self {
            Self::Human => ui::warn(message),
            Self::Json => ui::warn_stderr(message),
        }
    }

    pub fn transition(self, status: &StatusResponse) {
        let context = json!({
            "source": status.source,
            "destination": status.destination,
            "details": status.details,
        });
        self.kv(
            &utc_clock(),
            &format!("{} {context}", state_name(status.state)),
        );
    }

    pub fn poll_failed(self, error: &eyre::Report) {
        self.warn(&format!(
            "{} status poll failed; retrying: {}",
            utc_clock(),
            ui::scrub_urls(&error.to_string())
        ));
    }

    pub fn announce(self, report: &RunReport, plan: &RoutePlan) {
        self.section(&format!("rfq {} route", route_name(report.route)));
        self.kv("network", &report.network.to_string());
        self.kv(
            "from",
            &format!("{} {}", plan.from.chain.label, plan.from.token),
        );
        self.kv("to", &format!("{} {}", plan.to.chain.label, plan.to.token));
        self.kv("amount", &report.request.amount);
        self.kv("sender", &report.request.sender);
        self.kv("recipient", &report.request.recipient);
        if report.dry_run {
            self.kv("mode", "dry run; nothing is sent");
        }
    }

    pub fn summary(self, report: &RunReport) -> Result<()> {
        match self {
            Self::Json => println!("{}", serde_json::to_string_pretty(report)?),
            Self::Human => human_summary(report)?,
        }
        Ok(())
    }
}

fn human_summary(report: &RunReport) -> Result<()> {
    if let Some(actions) = &report.actions {
        ui::section("quote actions");
        println!("{}", serde_json::to_string_pretty(actions)?);
    }
    if let Some(status) = &report.status {
        ui::section("final status");
        println!("{}", serde_json::to_string_pretty(status)?);
    }
    let elapsed = ui::format_duration(Duration::from_secs(report.elapsed_secs));
    match (&report.error, report.dry_run) {
        (Some(error), _) => ui::error(&format!(
            "{} route failed: {error}",
            route_name(report.route)
        )),
        (None, true) => ui::success(&format!("{} route quoted", route_name(report.route))),
        (None, false) => ui::success(&format!(
            "{} route reached DONE in {elapsed}",
            route_name(report.route)
        )),
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

/// What one route check did, for the JSON summary.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub network: Network,
    pub route: RfqRoute,
    pub source: RfqSource,
    pub dry_run: bool,
    pub passed: bool,
    pub request: QuoteRequest,
    pub quote_id: Option<String>,
    pub backend: Option<BackendType>,
    pub output_amount: Option<String>,
    pub wrap_tx: Option<String>,
    pub transactions: Vec<ActionTx>,
    /// The quote's actions, reported by a dry run only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<Action>>,
    /// The terminal status, or the last one read before a timeout.
    pub status: Option<StatusResponse>,
    pub elapsed_secs: u64,
    pub error: Option<String>,
}

impl RunReport {
    pub const fn new(
        network: Network,
        route: RfqRoute,
        source: RfqSource,
        dry_run: bool,
        request: QuoteRequest,
    ) -> Self {
        Self {
            network,
            route,
            source,
            dry_run,
            passed: false,
            request,
            quote_id: None,
            backend: None,
            output_amount: None,
            wrap_tx: None,
            transactions: Vec::new(),
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

    pub fn close(&mut self, result: &Result<()>, elapsed: Duration) {
        self.elapsed_secs = elapsed.as_secs();
        self.passed = result.is_ok();
        self.error = result
            .as_ref()
            .err()
            .map(|error| ui::scrub_urls(&format!("{error:#}")));
    }
}

const fn route_name(route: RfqRoute) -> &'static str {
    match route {
        RfqRoute::Its => "its",
        RfqRoute::NativeWrap => "native-wrap",
        RfqRoute::Gateway => "gateway",
        RfqRoute::Solana => "solana",
    }
}

const fn state_name(state: TransferState) -> &'static str {
    match state {
        TransferState::AwaitingDeposit => "AWAITING_DEPOSIT",
        TransferState::Pending => "PENDING",
        TransferState::Done => "DONE",
        TransferState::Refunded => "REFUNDED",
        TransferState::Failed => "FAILED",
        TransferState::NotFound => "NOT_FOUND",
    }
}

fn utc_clock() -> String {
    Utc::now().format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use eyre::eyre;

    use super::*;
    use crate::commands::intents::types::OrderType;

    fn report() -> RunReport {
        RunReport::new(
            Network::Testnet,
            RfqRoute::Its,
            RfqSource::Avalanche,
            false,
            QuoteRequest {
                from_chain: "eip155:43113".to_owned(),
                from_token: "0xfrom".to_owned(),
                to_chain: "eip155:11155111".to_owned(),
                to_token: "0xto".to_owned(),
                amount: "1".to_owned(),
                order_type: OrderType::ExactInput,
                sender: "0xsender".to_owned(),
                recipient: "0xrecipient".to_owned(),
            },
        )
    }

    #[test]
    fn state_names_match_the_wire_spelling() {
        for state in [
            TransferState::AwaitingDeposit,
            TransferState::Pending,
            TransferState::Done,
            TransferState::Refunded,
            TransferState::Failed,
            TransferState::NotFound,
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), state_name(state));
        }
    }

    #[test]
    fn route_names_match_the_cli_spelling() {
        for route in [
            RfqRoute::Its,
            RfqRoute::NativeWrap,
            RfqRoute::Gateway,
            RfqRoute::Solana,
        ] {
            assert_eq!(serde_json::to_value(route).unwrap(), route_name(route));
        }
    }

    #[test]
    fn closing_records_the_verdict_and_a_scrubbed_error() {
        let mut passed = report();
        passed.close(&Ok(()), Duration::from_secs(90));
        assert!(passed.passed);
        assert_eq!(passed.elapsed_secs, 90);
        assert_eq!(passed.error, None);

        let mut failed = report();
        failed.close(
            &Err(eyre!("rpc https://key.example/abc failed")),
            Duration::from_secs(5),
        );
        assert!(!failed.passed);
        let error = failed.error.unwrap();
        assert!(error.contains("failed"));
        assert!(!error.contains("key.example"));
    }

    #[test]
    fn the_summary_is_camel_case_json_without_dry_run_actions() {
        let summary = serde_json::to_value(report()).unwrap();

        assert_eq!(summary["route"], "its");
        assert_eq!(summary["source"], "avalanche");
        assert_eq!(summary["request"]["orderType"], "EXACT_INPUT");
        assert_eq!(summary["passed"], false);
        assert!(summary.get("actions").is_none());
        assert!(summary.get("quoteId").is_some());
    }
}
