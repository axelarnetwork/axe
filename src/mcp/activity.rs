//! What the operator sees in the server's terminal.
//!
//! One banner at startup with everything that was fixed for the process
//! lifetime, then one line per request. Everything goes to stderr: on stdio
//! that is the only channel not carrying the protocol, and on HTTP it keeps
//! the two streams distinguishable.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use rmcp::ErrorData;
use rmcp::model::{CallToolResponse, JsonObject};

use crate::mcp::guidance;
use crate::mcp::runs::RunId;
use crate::mcp::server::{AxeMcp, SPEND_TOOLS};
use crate::types::Network;

/// Longer argument dumps are cut, so a calldata blob cannot flood the log.
const MAX_ARGS_CHARS: usize = 160;

/// What the banner reports.
pub struct Startup {
    pub network: Network,
    /// "stdio" or the HTTP URL.
    pub endpoint: String,
    pub caps: String,
    pub reports_dir: PathBuf,
    pub ledger: PathBuf,
}

/// Print the banner: network, endpoint, caps, artifacts, and the catalogue.
pub fn startup(startup: &Startup) {
    line(&format!("axe mcp {} starting", env!("CARGO_PKG_VERSION")));
    line(&format!(
        "network: {} (fixed for this process)",
        startup.network
    ));
    line(&format!("endpoint: {}", startup.endpoint));
    line(&startup.caps);
    line(&format!("run reports: {}", startup.reports_dir.display()));
    line(&format!("spend ledger: {}", startup.ledger.display()));

    let tools = AxeMcp::catalogue();
    let names: Vec<String> = tools
        .iter()
        .map(|tool| {
            if SPEND_TOOLS.contains(&tool.name.as_ref()) {
                format!("{}*", tool.name)
            } else {
                tool.name.to_string()
            }
        })
        .collect();
    line(&format!(
        "tools ({}): {} (* spends funds)",
        tools.len(),
        names.join(", ")
    ));
    line(&format!(
        "resources ({}): {}",
        guidance::doc_resources().len(),
        guidance::doc_resources()
            .iter()
            .map(|r| r.uri.clone())
            .collect::<Vec<_>>()
            .join(", ")
    ));
}

/// One line per tool call: name, outcome, duration, arguments.
pub fn tool_call(
    name: &str,
    arguments: Option<&JsonObject>,
    result: &Result<CallToolResponse, ErrorData>,
    elapsed: Duration,
) {
    line(&tool_call_line(name, arguments, result, elapsed));
}

/// One line per resource read.
pub fn resource_read(uri: &str, found: bool) {
    let outcome = if found { "ok" } else { "not found" };
    line(&format!("resource {uri} {outcome}"));
}

/// The client went away with runs still going; the process stays up for them.
pub fn draining(run_ids: &[RunId]) {
    let names: Vec<String> = run_ids.iter().map(ToString::to_string).collect();
    line(&format!(
        "client disconnected; waiting for {} to finish before exiting",
        names.join(", ")
    ));
}

/// Every run finished; the process can exit.
pub fn drained() {
    line("all runs finished; exiting");
}

/// A finished run could not leave its report behind. The operator is the only
/// one who can act on this: the caller will simply see the run as unknown.
pub fn report_unwritable(run_id: &RunId, path: &std::path::Path, error: &std::io::Error) {
    line(&format!(
        "run {run_id} finished but its report could not be written to {}: {error}",
        path.display()
    ));
}

/// One connection could not be accepted. The listener carries on, so this is
/// a note rather than a failure.
pub fn accept_failed(error: &std::io::Error) {
    line(&format!("could not accept a connection: {error}"));
}

fn tool_call_line(
    name: &str,
    arguments: Option<&JsonObject>,
    result: &Result<CallToolResponse, ErrorData>,
    elapsed: Duration,
) -> String {
    let outcome = match result {
        Ok(CallToolResponse::Complete(result)) if result.is_error == Some(true) => {
            "error".to_string()
        }
        Ok(CallToolResponse::Complete(_)) => "ok".to_string(),
        // Input required or a task handle: the call is not finished yet.
        Ok(_) => "pending".to_string(),
        // Error text can span lines; the log is one line per request.
        Err(e) => format!("refused: {}", fold(&e.message)),
    };
    let args = arguments
        .filter(|a| !a.is_empty())
        .and_then(|a| serde_json::to_string(a).ok())
        .map(|a| format!(" {}", truncate(&a)))
        .unwrap_or_default();

    format!("tool {name} {}ms {outcome}{args}", elapsed.as_millis())
}

/// Collapse line breaks and runs of spaces into single spaces.
fn fold(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_ARGS_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_ARGS_CHARS).collect();
    format!("{kept}...")
}

fn line(text: &str) {
    eprintln!("{} {text}", Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rmcp::ErrorData;
    use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock};
    use serde_json::json;

    use super::{MAX_ARGS_CHARS, tool_call_line};

    fn args(value: &serde_json::Value) -> Option<rmcp::model::JsonObject> {
        value.as_object().cloned()
    }

    #[test]
    fn successful_call_logs_name_duration_and_arguments() {
        let result = Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text("fine"),
        ])));
        let arguments = args(&json!({"chain": "flow"}));
        assert_eq!(
            tool_call_line(
                "verifiers",
                arguments.as_ref(),
                &result,
                Duration::from_millis(1234)
            ),
            r#"tool verifiers 1234ms ok {"chain":"flow"}"#
        );
    }

    #[test]
    fn refusal_logs_the_reason_and_omits_empty_arguments() {
        let result = Err(ErrorData::invalid_params(
            "11 transactions exceed the cap",
            None,
        ));
        let arguments = args(&json!({}));
        assert_eq!(
            tool_call_line(
                "start_load_test",
                arguments.as_ref(),
                &result,
                Duration::from_millis(2)
            ),
            "tool start_load_test 2ms refused: 11 transactions exceed the cap"
        );
    }

    #[test]
    fn multi_line_errors_stay_on_one_line() {
        let result = Err(ErrorData::internal_error(
            "block lookup failed: rpc said\n  status 500\n  retry later",
            None,
        ));
        let logged = tool_call_line("info_block", None, &result, Duration::from_millis(7));
        assert!(!logged.contains('\n'), "{logged}");
        assert_eq!(
            logged,
            "tool info_block 7ms refused: block lookup failed: rpc said status 500 retry later"
        );
    }

    #[test]
    fn long_arguments_are_cut() {
        let calldata = "ab".repeat(400);
        let arguments = args(&json!({"calldata": calldata}));
        let logged = tool_call_line(
            "decode_calldata",
            arguments.as_ref(),
            &Ok(CallToolResponse::Complete(CallToolResult::success(vec![]))),
            Duration::ZERO,
        );
        let dumped = logged.split_once("0ms ok ").map(|(_, rest)| rest).unwrap();
        assert!(dumped.ends_with("..."));
        assert_eq!(dumped.chars().count(), MAX_ARGS_CHARS + 3);
    }
}
