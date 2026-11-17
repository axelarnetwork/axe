//! Head-freshness check for the Axelar LCD and Tendermint RPC primaries.
//!
//! The fallback endpoints are tried when the primary errors. A primary whose
//! node has stopped advancing errors on nothing: it answers HTTP 200 with
//! frozen state (testnet qubelabs, LCD and RPC alike, stalled at block
//! 39980700 on 2026-11-17 while reporting `catching_up: false`), so the
//! verifier saw no vote for messages Axelar had already executed. A primary
//! whose head is too old is therefore moved behind its fallbacks.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::ui;

/// A head older than this is stale. Axelar produces a block every few seconds.
const MAX_HEAD_AGE: Duration = Duration::from_secs(120);

/// How long a freshness verdict is reused before the head is read again, so
/// a primary that stalls mid-run, or recovers, is noticed within a minute.
const RECHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AxelarEndpoint {
    Lcd,
    TendermintRpc,
}

impl AxelarEndpoint {
    fn head_path(self) -> &'static str {
        match self {
            Self::Lcd => "/cosmos/base/tendermint/v1beta1/blocks/latest",
            Self::TendermintRpc => "/status",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Lcd => "LCD",
            Self::TendermintRpc => "Tendermint RPC",
        }
    }
}

#[derive(Deserialize)]
struct LcdLatestBlock {
    block: LcdBlock,
}

#[derive(Deserialize)]
struct LcdBlock {
    header: LcdHeader,
}

#[derive(Deserialize)]
struct LcdHeader {
    time: String,
}

#[derive(Deserialize)]
struct RpcStatus {
    result: RpcStatusResult,
}

#[derive(Deserialize)]
struct RpcStatusResult {
    sync_info: SyncInfo,
}

#[derive(Deserialize)]
struct SyncInfo {
    latest_block_time: String,
}

/// Move a stale primary (`candidates[0]`) behind its fallbacks. It stays in
/// the list as a last resort, and nothing changes when there is no fallback
/// or the primary's head cannot be read: an unreachable primary is already
/// handled by the error-driven fallback.
pub(super) async fn demote_stale_primary(endpoint: AxelarEndpoint, candidates: &mut Vec<String>) {
    if candidates.len() < 2 {
        return;
    }
    let Some(age) = stale_head_age(endpoint, &candidates[0]).await else {
        return;
    };
    warn_once(endpoint, age);
    let primary = candidates.remove(0);
    candidates.push(primary);
}

/// How far the endpoint's head trails the wall clock, when beyond
/// [`MAX_HEAD_AGE`]. Cached per endpoint for [`RECHECK_INTERVAL`].
async fn stale_head_age(endpoint: AxelarEndpoint, url: &str) -> Option<Duration> {
    type Verdicts = Mutex<HashMap<String, (Instant, Option<Duration>)>>;
    static VERDICTS: OnceLock<Verdicts> = OnceLock::new();
    let verdicts = VERDICTS.get_or_init(Verdicts::default);

    let cached = verdicts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(url)
        .copied();
    if let Some((checked, verdict)) = cached
        && checked.elapsed() < RECHECK_INTERVAL
    {
        return verdict;
    }
    let verdict = read_head_time(endpoint, url)
        .await
        .map(|head| head_age(head, Utc::now()))
        .filter(|age| *age > MAX_HEAD_AGE);
    verdicts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(url.to_string(), (Instant::now(), verdict));
    verdict
}

async fn read_head_time(endpoint: AxelarEndpoint, url: &str) -> Option<DateTime<Utc>> {
    let body = crate::http::client()
        .get(format!("{url}{}", endpoint.head_path()))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    parse_head_time(endpoint, &body)
}

fn parse_head_time(endpoint: AxelarEndpoint, body: &str) -> Option<DateTime<Utc>> {
    let time = match endpoint {
        AxelarEndpoint::Lcd => {
            serde_json::from_str::<LcdLatestBlock>(body)
                .ok()?
                .block
                .header
                .time
        }
        AxelarEndpoint::TendermintRpc => {
            serde_json::from_str::<RpcStatus>(body)
                .ok()?
                .result
                .sync_info
                .latest_block_time
        }
    };
    time.parse().ok()
}

fn head_age(head: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - head).to_std().unwrap_or(Duration::ZERO)
}

/// One warning per endpoint kind and process, like the error-driven
/// fallback warning. The URL is not logged: it can be a private endpoint.
fn warn_once(endpoint: AxelarEndpoint, age: Duration) {
    static LCD_WARNED: OnceLock<()> = OnceLock::new();
    static RPC_WARNED: OnceLock<()> = OnceLock::new();
    let warned = match endpoint {
        AxelarEndpoint::Lcd => &LCD_WARNED,
        AxelarEndpoint::TendermintRpc => &RPC_WARNED,
    };
    if warned.set(()).is_ok() {
        ui::warn(&format!(
            "Axelar {} primary has stopped advancing (latest block {}s old); \
             trying the fallbacks first",
            endpoint.label(),
            age.as_secs()
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};

    use super::{AxelarEndpoint, MAX_HEAD_AGE, head_age, parse_head_time};

    fn at(time: &str) -> DateTime<Utc> {
        time.parse().unwrap()
    }

    #[test]
    fn reads_the_head_time_from_both_endpoint_kinds() {
        // Shapes of the frozen testnet qubelabs answers on 2026-11-17.
        let lcd =
            r#"{"block":{"header":{"height":"39980700","time":"2026-11-17T13:05:56.123456789Z"}}}"#;
        let rpc = r#"{"result":{"sync_info":{"latest_block_height":"39980700",
            "latest_block_time":"2026-11-17T13:05:56.123456789Z","catching_up":false}}}"#;
        let expected = at("2026-11-17T13:05:56.123456789Z");

        assert_eq!(parse_head_time(AxelarEndpoint::Lcd, lcd), Some(expected));
        assert_eq!(
            parse_head_time(AxelarEndpoint::TendermintRpc, rpc),
            Some(expected)
        );
        assert_eq!(parse_head_time(AxelarEndpoint::Lcd, rpc), None);
        assert_eq!(parse_head_time(AxelarEndpoint::Lcd, "<html>"), None);
    }

    #[test]
    fn a_head_48_minutes_old_is_stale_and_a_current_one_is_not() {
        let now = at("2026-11-17T13:53:38Z");

        assert!(head_age(at("2026-11-17T13:05:56Z"), now) > MAX_HEAD_AGE);
        assert!(head_age(at("2026-11-17T13:53:34Z"), now) <= MAX_HEAD_AGE);
        // A head slightly ahead of a skewed local clock is fresh.
        assert_eq!(head_age(at("2026-11-17T13:53:40Z"), now), Duration::ZERO);
    }
}
