//! The result contract every tool returns.

use eyre::Result;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::Value;

use crate::ui;

/// A structured payload plus a one-line summary.
///
/// The payload is what makes results composable: an agent can feed a field
/// from one call into the next question. The summary saves it from inferring
/// success by walking the payload. Both are needed, so both are mandatory here
/// rather than one being optional.
pub struct Outcome {
    summary: String,
    payload: Value,
}

impl Outcome {
    /// Build an outcome from any serializable result.
    ///
    /// Both halves are scrubbed of URLs for the same reason the load-test
    /// report already scrubs them: RPC endpoints carry provider API keys. The
    /// summary is the part most likely to be quoted back verbatim, and the
    /// payload is where a probe error carrying the endpoint it hit ends up.
    pub fn new<T: Serialize>(summary: impl Into<String>, value: &T) -> Result<Self> {
        let mut payload = serde_json::to_value(value)?;
        scrub_urls_in(&mut payload);
        Ok(Self {
            summary: ui::scrub_urls(&summary.into()),
            payload,
        })
    }

    /// Render into the protocol's tool result: the summary as text content so
    /// a human reading the transcript sees it, the payload as structured
    /// content so the agent can address fields.
    pub fn into_tool_result(self) -> CallToolResult {
        let mut result = CallToolResult::structured(self.payload);
        // `structured` stuffs the whole payload into the text content, which
        // would make an agent read the same data twice. The summary is what a
        // human wants to see in the transcript.
        result.content = vec![ContentBlock::text(self.summary)];
        result
    }
}

/// Redact every URL in every string of a JSON document, in place.
fn scrub_urls_in(value: &mut Value) {
    match value {
        Value::String(text) => *text = ui::scrub_urls(text),
        Value::Array(items) => items.iter_mut().for_each(scrub_urls_in),
        Value::Object(fields) => fields.values_mut().for_each(scrub_urls_in),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Map an internal failure onto a protocol error.
///
/// URLs are scrubbed here too: an error is the most likely place for a raw
/// RPC endpoint to surface, since it often carries the request that failed.
pub fn to_error_data(context: &str, err: &eyre::Report) -> ErrorData {
    ErrorData::internal_error(ui::scrub_urls(&format!("{context}: {err:#}")), None)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Outcome, to_error_data};

    #[test]
    fn result_carries_summary_as_text_and_payload_as_structured_content() {
        let result = Outcome::new("2 rows", &json!({"rows": [1, 2]}))
            .unwrap()
            .into_tool_result();

        let text: Vec<&str> = result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
            .collect();
        assert_eq!(text, ["2 rows"]);
        assert_eq!(result.structured_content, Some(json!({"rows": [1, 2]})));
    }

    #[test]
    fn urls_are_redacted_from_summary_and_every_payload_string() {
        let payload = json!({
            "note": "rpc https://rpc.example.com/v1/secret-key failed",
            "nested": [{"url": "http://a.b/c?token=1"}, "plain", 7],
            "config": "/home/op/.local/share/axe/testnet.json",
        });
        let result = Outcome::new(
            "failed against https://rpc.example.com/v1/secret-key",
            &payload,
        )
        .unwrap()
        .into_tool_result();

        let summary = result.content[0].as_text().unwrap().text.clone();
        assert_eq!(summary, "failed against <redacted-url>");

        let structured = result.structured_content.unwrap();
        assert_eq!(structured["note"], "rpc <redacted-url> failed");
        assert_eq!(structured["nested"][0]["url"], "<redacted-url>");
        assert_eq!(structured["nested"][1], "plain");
        assert_eq!(structured["nested"][2], 7);
        assert_eq!(
            structured["config"],
            "/home/op/.local/share/axe/testnet.json"
        );
    }

    #[test]
    fn errors_are_redacted_too() {
        let err = eyre::eyre!("connect to https://rpc.example.com/v1/secret-key timed out");
        let data = to_error_data("balance check failed", &err);
        assert_eq!(
            data.message,
            "balance check failed: connect to <redacted-url> timed out"
        );
    }
}
