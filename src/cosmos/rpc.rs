//! LCD REST + Tendermint RPC queries. Everything here is a read against a
//! cosmos node — no signing, no broadcast. Config readers (`read_axelar_*`)
//! live here too because they're "where do I point the LCD client" plumbing.

use std::fs;
use std::path::Path;

use alloy::{
    hex,
    primitives::{Address, FixedBytes, U256},
};
use base64::Engine;
use eyre::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::evm::pubkey_to_address;
use crate::ui;

/// `account` sub-object inside the LCD `/cosmos/auth/v1beta1/accounts`
/// response. The exact set of fields varies by Cosmos SDK version and signer
/// type (BaseAccount, ModuleAccount, etc.), so only the fields we actually
/// use are captured here.
#[derive(Deserialize)]
struct AccountInner {
    #[serde(default)]
    account_number: Option<String>,
    #[serde(default)]
    sequence: Option<String>,
}

/// LCD `/cosmos/bank/v1beta1/balances/{address}/by_denom` response. The
/// `balance` field is `{ denom, amount }`, both as strings.
#[derive(Deserialize)]
struct BalanceResponse {
    balance: Option<Coin>,
}

#[derive(Deserialize)]
struct Coin {
    #[serde(default)]
    amount: Option<String>,
}

/// LCD `/cosmos/tx/v1beta1/simulate` response. The endpoint returns either a
/// top-level `message` (error) or `gas_info.gas_used` (success).
#[derive(Deserialize, Default)]
struct SimulateResponse {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    gas_info: Option<GasInfo>,
}

#[derive(Deserialize)]
struct GasInfo {
    #[serde(default)]
    gas_used: Option<String>,
}

/// LCD tx-result envelope used by both `lcd_broadcast_tx` and
/// `lcd_wait_for_tx`: `{ "tx_response": { "code": u64, "raw_log": "..." } }`.
/// Only the fields needed for failure detection are captured here; callers
/// receive the raw `Value` to pluck their own fields (`txhash`, `events`,
/// etc.).
#[derive(Deserialize)]
struct TxResultEnvelope {
    tx_response: Option<TxResultBody>,
}

#[derive(Deserialize)]
struct TxResultBody {
    #[serde(default)]
    code: Option<u64>,
    #[serde(default)]
    raw_log: Option<String>,
}

/// LCD `/cosmwasm/wasm/v1/code` paginated listing of code IDs.
#[derive(Deserialize)]
struct CodeListResponse {
    code_infos: Option<Vec<CodeInfo>>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Deserialize)]
struct CodeInfo {
    #[serde(default)]
    code_id: Option<String>,
    #[serde(default)]
    data_hash: Option<String>,
}

#[derive(Deserialize)]
struct Pagination {
    #[serde(default)]
    next_key: Option<String>,
}

/// `target.json` shape, narrowed to the axelar config fields read here. The
/// MultisigProver address lookup is keyed by `chain_axelar_id` at runtime,
/// so it stays a Value lookup inside `fetch_verifier_set`.
#[derive(Deserialize)]
struct AxelarTargetJson {
    axelar: AxelarSection,
}

#[derive(Deserialize)]
struct AxelarSection {
    #[serde(default)]
    rpc: Option<String>,
    #[serde(default)]
    lcd: Option<String>,
    #[serde(default)]
    contracts: Option<Value>,
}

/// Tendermint RPC `block?height=N` response. Only `header.time` is read.
#[derive(Deserialize)]
struct BlockResponse {
    result: Option<BlockResult>,
}

#[derive(Deserialize)]
struct BlockResult {
    block: Option<BlockBody>,
}

#[derive(Deserialize)]
struct BlockBody {
    header: Option<BlockHeader>,
}

#[derive(Deserialize)]
struct BlockHeader {
    time: Option<String>,
}

/// Verifier set returned by `current_verifier_set` smart query. The signers
/// map is keyed by an opaque participant id (e.g. consensus address). The
/// numeric `threshold` and per-signer `weight` are kept as raw `Value` so the
/// existing string-or-u64 polymorphism (and its silent fallback to `1` for
/// off-shape weights) survives unchanged.
#[derive(Deserialize)]
struct VerifierSetResponse {
    data: Option<VerifierSetData>,
}

#[derive(Deserialize)]
struct VerifierSetData {
    #[serde(default)]
    id: Option<String>,
    verifier_set: Option<VerifierSet>,
}

#[derive(Deserialize)]
struct VerifierSet {
    signers: Option<std::collections::BTreeMap<String, Signer>>,
    #[serde(default)]
    threshold: Value,
    created_at: Option<u64>,
}

#[derive(Deserialize)]
struct Signer {
    pub_key: Option<PubKey>,
    #[serde(default)]
    weight: Value,
}

#[derive(Deserialize)]
struct PubKey {
    #[serde(default)]
    ecdsa: Option<String>,
}

/// Parse a numeric LCD response field that comes back as a JSON string,
/// defaulting to zero on missing/unparseable values. Many cosmos LCD endpoints
/// represent u64/u128 as strings (e.g. `"sequence": "42"`).
pub(super) fn parse_or_zero<T: std::str::FromStr + Default>(s: Option<&str>) -> T {
    s.and_then(|x| x.parse().ok()).unwrap_or_default()
}

pub(super) async fn lcd_query_account(lcd: &str, address: &str) -> Result<(u64, u64)> {
    let url = format!("{lcd}/cosmos/auth/v1beta1/accounts/{address}");
    let raw: Value = reqwest::get(&url).await?.json().await?;
    // The original errored only when the `account` field was entirely absent;
    // any other shape (non-object, missing sub-fields) silently fell through
    // to `parse_or_zero` → `(0, 0)`. We replicate that by checking presence
    // first and absorbing typed-parse failures inside the account into the
    // same zero defaults.
    if raw.get("account").is_none() {
        return Err(eyre::eyre!("no account in response: {raw}"));
    }
    let account: AccountInner =
        serde_json::from_value(raw["account"].clone()).unwrap_or(AccountInner {
            account_number: None,
            sequence: None,
        });
    let account_number: u64 = parse_or_zero(account.account_number.as_deref());
    let sequence: u64 = parse_or_zero(account.sequence.as_deref());
    Ok((account_number, sequence))
}

/// Query the bank balance of `address` for a single denom. Returns the amount in base units (e.g. uaxl).
pub(super) async fn lcd_query_balance(lcd: &str, address: &str, denom: &str) -> Result<u128> {
    let url = format!("{lcd}/cosmos/bank/v1beta1/balances/{address}/by_denom?denom={denom}");
    let raw: Value = reqwest::get(&url).await?.json().await?;
    // Permissive: original used `Value::pointer` with `.unwrap_or` defaults,
    // so any off-shape response was silently treated as zero.
    let resp: BalanceResponse =
        serde_json::from_value(raw).unwrap_or(BalanceResponse { balance: None });
    let amount: u128 = parse_or_zero(resp.balance.as_ref().and_then(|b| b.amount.as_deref()));
    Ok(amount)
}

/// Preflight: ensure the Axelar relayer wallet exists on-chain and holds enough fee-denom to relay.
/// Errors with a clear "fund this address" message if the account is missing or balance is below `min_amount`.
pub async fn check_axelar_balance(
    lcd: &str,
    chain_id: &str,
    address: &str,
    fee_denom: &str,
    min_amount: u128,
) -> Result<()> {
    let account_exists = lcd_query_account(lcd, address).await.is_ok();
    let balance = lcd_query_balance(lcd, address, fee_denom)
        .await
        .unwrap_or(0);

    let display = balance as f64 / 1_000_000.0;
    let min_display = min_amount as f64 / 1_000_000.0;

    if !account_exists || balance < min_amount {
        ui::error(&format!("axelar relayer wallet underfunded on {chain_id}:"));
        ui::error(&format!("  address: {address}"));
        ui::error(&format!(
            "  balance: {display:.6} {fee_denom} (need >= {min_display:.6})"
        ));
        if !account_exists {
            ui::error("  account does not exist on-chain — send any amount to create it");
        }
        return Err(eyre::eyre!(
            "fund {address} with at least {min_display:.6} {fee_denom} on {chain_id} and retry"
        ));
    }

    ui::kv(
        "relayer balance",
        &format!("{display:.6} {fee_denom} (>= {min_display:.6})"),
    );
    Ok(())
}

pub(super) async fn lcd_simulate_tx(lcd: &str, tx_bytes: &[u8]) -> Result<u64> {
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);
    let body = serde_json::json!({
        "tx_bytes": tx_b64,
        "mode": "BROADCAST_MODE_UNSPECIFIED"
    });
    let client = reqwest::Client::new();
    let raw: Value = client
        .post(format!("{lcd}/cosmos/tx/v1beta1/simulate"))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    // The original used `Value::pointer` lookups, which silently mapped any
    // off-shape response to "gas_used = 0" → the existing "0 gas" error. We
    // preserve that fallthrough by treating a deserialize failure as an
    // empty `SimulateResponse` rather than introducing a new error path.
    let resp: SimulateResponse = serde_json::from_value(raw.clone()).unwrap_or_default();
    if let Some(err) = resp.message.as_deref()
        && !err.is_empty()
    {
        return Err(eyre::eyre!("simulation failed: {err}"));
    }
    let gas_used: u64 = parse_or_zero(resp.gas_info.as_ref().and_then(|g| g.gas_used.as_deref()));
    if gas_used == 0 {
        return Err(eyre::eyre!(
            "simulation returned 0 gas — response: {}",
            serde_json::to_string_pretty(&raw)?
        ));
    }
    Ok(gas_used)
}

pub(super) async fn lcd_broadcast_tx(lcd: &str, tx_bytes: &[u8]) -> Result<Value> {
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);
    let body = serde_json::json!({
        "tx_bytes": tx_b64,
        "mode": "BROADCAST_MODE_SYNC"
    });
    let client = reqwest::Client::new();
    let resp: Value = client
        .post(format!("{lcd}/cosmos/tx/v1beta1/txs"))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let envelope: TxResultEnvelope =
        serde_json::from_value(resp.clone()).unwrap_or(TxResultEnvelope { tx_response: None });
    let tx_response = envelope.tx_response;
    let code = tx_response.as_ref().and_then(|b| b.code).unwrap_or(1);
    if code != 0 {
        let raw_log = tx_response
            .as_ref()
            .and_then(|b| b.raw_log.as_deref())
            .unwrap_or("unknown error");
        return Err(eyre::eyre!("broadcast failed (code {code}): {raw_log}"));
    }
    Ok(resp)
}

/// Wait for a tx to be included in a block and return the full tx response with events.
pub(super) async fn lcd_wait_for_tx(lcd: &str, tx_hash: &str) -> Result<Value> {
    for _ in 0..crate::timing::LCD_WAIT_MAX_ATTEMPTS {
        tokio::time::sleep(crate::timing::LCD_WAIT_RETRY_INTERVAL).await;
        let url = format!("{lcd}/cosmos/tx/v1beta1/txs/{tx_hash}");
        let resp: Value = match reqwest::get(&url).await {
            Ok(r) => r.json().await.unwrap_or(serde_json::json!({})),
            Err(_) => continue,
        };
        if resp.get("tx_response").is_some() {
            let envelope: TxResultEnvelope = serde_json::from_value(resp.clone())
                .unwrap_or(TxResultEnvelope { tx_response: None });
            let tx_response = envelope.tx_response;
            let code = tx_response.as_ref().and_then(|b| b.code).unwrap_or(1);
            if code != 0 {
                let raw_log = tx_response
                    .as_ref()
                    .and_then(|b| b.raw_log.as_deref())
                    .unwrap_or("unknown");
                return Err(eyre::eyre!("tx failed (code {code}): {raw_log}"));
            }
            return Ok(resp);
        }
    }
    Err(eyre::eyre!("timeout waiting for tx {tx_hash}"))
}

pub async fn lcd_query_proposal(lcd: &str, proposal_id: u64) -> Result<Value> {
    let url = format!("{lcd}/cosmos/gov/v1/proposals/{proposal_id}");
    let resp: Value = reqwest::get(&url).await?.json().await?;
    resp.get("proposal")
        .cloned()
        .ok_or_else(|| eyre::eyre!("no 'proposal' field in response"))
}

/// Public Axelar LCD endpoints used as silent fallbacks when the primary
/// (chain-config or env-override) endpoint is unreachable. Imperator's
/// mainnet LCD has been flapping with 502s; lavenderfive and polkachu are
/// healthy alternatives at the time of writing.
const LCD_FALLBACKS_MAINNET: &[&str] = &[
    "https://rest.lavenderfive.com/axelar",
    "https://axelar-rest.publicnode.com",
];

/// `OnceLock` flag for the LCD fallback warning. We emit one ui::warn the
/// first time the primary LCD goes unhealthy in a process, then stay quiet
/// — repeating per-call would flood the load-test report log with the same
/// message hundreds of times.
static LCD_FALLBACK_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Print a one-time warning when an LCD response came from a fallback
/// endpoint instead of the user-configured primary. `idx` is the position
/// in the candidate list — `0` is the primary (no warning), anything ≥ 1 is
/// a fallback.
fn note_lcd_fallback_use(idx: usize, used: &str, last_err: Option<&eyre::Report>) {
    if idx == 0 {
        return;
    }
    if LCD_FALLBACK_WARNED.set(()).is_err() {
        return;
    }
    let cause = last_err
        .map(|e| {
            let s = e.to_string();
            s.lines().next().unwrap_or("").to_string()
        })
        .unwrap_or_else(|| "primary unreachable".to_string());
    ui::warn(&format!(
        "Axelar LCD primary unhealthy ({cause}); using fallback {used} for the rest of this run"
    ));
}

pub async fn lcd_cosmwasm_smart_query(
    lcd: &str,
    contract: &str,
    query_msg: &Value,
) -> Result<Value> {
    let user_override = std::env::var("AXELAR_LCD_URL").ok();
    let primary = user_override
        .clone()
        .unwrap_or_else(|| lcd.to_string())
        .trim_end_matches('/')
        .to_string();

    // Try the primary endpoint first; on transient failures (HTTP 5xx, network
    // error, non-JSON body) silently fall through to known-good public
    // endpoints. Only the user-set AXELAR_LCD_URL skips fallback — we honor
    // their explicit choice and surface the error directly.
    let mut candidates: Vec<String> = vec![primary.clone()];
    if user_override.is_none() {
        for fb in LCD_FALLBACKS_MAINNET {
            if *fb != primary {
                candidates.push((*fb).to_string());
            }
        }
    }

    let query_json = serde_json::to_string(query_msg)?;
    let query_b64 = base64::engine::general_purpose::STANDARD.encode(query_json.as_bytes());
    let mut last_err: Option<eyre::Report> = None;

    for (idx, endpoint) in candidates.iter().enumerate() {
        let url = format!("{endpoint}/cosmwasm/wasm/v1/contract/{contract}/smart/{query_b64}");
        match reqwest::get(&url).await {
            Ok(response) => {
                let status = response.status();
                match response.text().await {
                    Ok(body) => {
                        if !status.is_success() {
                            last_err = Some(eyre::eyre!(
                                "LCD {endpoint} returned HTTP {status}. \
                                 First 200 chars of body: {}",
                                body.chars().take(200).collect::<String>()
                            ));
                            continue;
                        }
                        match serde_json::from_str::<Value>(&body) {
                            Ok(resp) => {
                                note_lcd_fallback_use(idx, endpoint, last_err.as_ref());
                                return Ok(resp["data"].clone());
                            }
                            Err(e) => {
                                last_err = Some(eyre::eyre!(
                                    "LCD {endpoint} returned non-JSON body. \
                                     First 200 chars: {}\nParse error: {e}",
                                    body.chars().take(200).collect::<String>()
                                ));
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        last_err = Some(eyre::eyre!("LCD {endpoint} body read failed: {e}"));
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = Some(eyre::eyre!("LCD request to {endpoint} failed: {e}"));
                continue;
            }
        }
    }

    Err(last_err.unwrap_or_else(|| eyre::eyre!("LCD query exhausted all endpoints"))).map_err(|e| {
        eyre::eyre!(
            "{e}\nTip: set AXELAR_LCD_URL to a working endpoint (e.g. \
                 `https://rest.lavenderfive.com/axelar` for mainnet)."
        )
    })
}

/// Fetch code IDs by matching storeCodeProposalCodeHash against on-chain checksums.
pub async fn lcd_fetch_code_id(lcd: &str, expected_checksum: &str) -> Result<u64> {
    let expected = expected_checksum.to_uppercase();
    let mut next_key: Option<String> = None;
    loop {
        let mut url =
            format!("{lcd}/cosmwasm/wasm/v1/code?pagination.limit=100&pagination.reverse=true");
        if let Some(ref key) = next_key {
            url.push_str(&format!("&pagination.key={key}"));
        }
        let raw: Value = reqwest::get(&url).await?.json().await?;
        // Permissive: original errored on `code_infos` missing/non-array via
        // `as_array().ok_or(...)`; any other malformation (e.g. non-string
        // `code_id`) was silently mapped to "0". We mirror that by trying a
        // typed parse and falling back to an empty `CodeListResponse` so the
        // existing "no code_infos" error fires for the missing-field case.
        let resp: CodeListResponse = serde_json::from_value(raw).unwrap_or(CodeListResponse {
            code_infos: None,
            pagination: None,
        });
        let codes = resp
            .code_infos
            .ok_or_else(|| eyre::eyre!("no code_infos in response"))?;
        for code in &codes {
            let checksum = code.data_hash.as_deref().unwrap_or("").to_uppercase();
            if checksum == expected {
                let code_id: u64 = code.code_id.as_deref().unwrap_or("0").parse().unwrap_or(0);
                return Ok(code_id);
            }
        }
        let nk = resp.pagination.and_then(|p| p.next_key).unwrap_or_default();
        if nk.is_empty() {
            break;
        }
        next_key = Some(nk);
    }
    Err(eyre::eyre!(
        "code not found for checksum {expected_checksum}"
    ))
}

/// Read Axelar LCD url, chain ID, fee denom, and gas price from target json.
/// Errors if any of the fields are missing — silent defaults previously
/// masked config drift (e.g. a missing `gasPrice` falling back to
/// `0.007uaxl`), so callers should always hit a real on-disk config.
pub fn read_axelar_config(target_json: &Path) -> Result<(String, String, String, f64)> {
    crate::config::ChainsConfig::load(target_json)?
        .axelar
        .cosmos_tx_params()
}

/// Read a string field from axelar contracts config
pub fn read_axelar_contract_field(target_json: &Path, pointer: &str) -> Result<String> {
    let content = fs::read_to_string(target_json)?;
    let root: Value = serde_json::from_str(&content)?;
    root.pointer(pointer)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("field not found: {pointer}"))
}

/// Read Axelar RPC url from target json (`/axelar/rpc`).
pub fn read_axelar_rpc(target_json: &Path) -> Result<String> {
    let content = fs::read_to_string(target_json)?;
    // Parse to `Value` first so a malformed-JSON file surfaces serde's parse
    // error verbatim, matching the original `from_str::<Value>(&content)?`.
    let raw: Value = serde_json::from_str(&content)?;
    let root: AxelarTargetJson =
        serde_json::from_value(raw).map_err(|_| eyre::eyre!("no axelar.rpc in target json"))?;
    root.axelar
        .rpc
        .ok_or_else(|| eyre::eyre!("no axelar.rpc in target json"))
}

/// Public Axelar Tendermint RPC endpoints used as silent fallbacks when the
/// primary endpoint (from chain config or `AXELAR_RPC_URL`) is unreachable.
/// Imperator's mainnet RPC has been flapping with 502s; these are healthy
/// alternatives at the time of writing.
const RPC_FALLBACKS_MAINNET: &[&str] = &[
    "https://axelar-rpc.publicnode.com",
    "https://rpc.cosmos.directory/axelar",
];

/// `OnceLock` flag matching `LCD_FALLBACK_WARNED` for the Tendermint RPC
/// side — same once-per-process semantics so a flapping primary doesn't
/// flood the report log.
static RPC_FALLBACK_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn note_rpc_fallback_use(idx: usize, used: &str, last_err: Option<&eyre::Report>) {
    if idx == 0 {
        return;
    }
    if RPC_FALLBACK_WARNED.set(()).is_err() {
        return;
    }
    let cause = last_err
        .map(|e| {
            let s = e.to_string();
            s.lines().next().unwrap_or("").to_string()
        })
        .unwrap_or_else(|| "primary unreachable".to_string());
    ui::warn(&format!(
        "Axelar Tendermint RPC primary unhealthy ({cause}); using fallback {used} for the rest of this run"
    ));
}

/// Query Tendermint RPC `tx_search` for a single event key/value pair.
/// Returns the parsed JSON response. Silently falls back to public RPCs if
/// the primary endpoint errors and `AXELAR_RPC_URL` is not explicitly set.
pub async fn rpc_tx_search_event(rpc: &str, event_key: &str, event_value: &str) -> Result<Value> {
    let user_override = std::env::var("AXELAR_RPC_URL").ok();
    let primary = user_override
        .clone()
        .unwrap_or_else(|| rpc.to_string())
        .trim_end_matches('/')
        .to_string();

    let mut candidates: Vec<String> = vec![primary.clone()];
    if user_override.is_none() {
        for fb in RPC_FALLBACKS_MAINNET {
            if *fb != primary {
                candidates.push((*fb).to_string());
            }
        }
    }

    let query = format!("\"{event_key}='{event_value}'\"");
    let encoded = url_encode_query(&query);
    let mut last_err: Option<eyre::Report> = None;

    for (idx, endpoint) in candidates.iter().enumerate() {
        let url = format!("{endpoint}/tx_search?query={encoded}&per_page=1");
        match reqwest::get(&url).await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    last_err = Some(eyre::eyre!(
                        "Tendermint RPC {endpoint} returned HTTP {status}"
                    ));
                    continue;
                }
                match response.json::<Value>().await {
                    Ok(v) => {
                        note_rpc_fallback_use(idx, endpoint, last_err.as_ref());
                        return Ok(v);
                    }
                    Err(e) => {
                        last_err = Some(eyre::eyre!("RPC {endpoint} JSON decode failed: {e}"));
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = Some(eyre::eyre!("RPC request to {endpoint} failed: {e}"));
                continue;
            }
        }
    }

    Err(last_err.unwrap_or_else(|| eyre::eyre!("Tendermint RPC query exhausted all endpoints")))
        .map_err(|e| {
            eyre::eyre!(
                "{e}\nTip: set AXELAR_RPC_URL to a working endpoint (e.g. \
                 `https://axelar-rpc.publicnode.com` for mainnet)."
            )
        })
}

fn url_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Query Tendermint RPC `tx_search` with a raw query string (e.g.
/// `key1='value' AND key2='value'`). Returns the parsed `result` payload (with
/// keys `total_count`, `txs`).
pub async fn rpc_tx_search(
    rpc: &str,
    query: &str,
    per_page: u32,
    page: u32,
    order_desc: bool,
) -> Result<Value> {
    let json_quoted = serde_json::to_string(query)?;
    let order = if order_desc { "\"desc\"" } else { "\"asc\"" };
    let resp = reqwest::Client::new()
        .get(format!("{rpc}/tx_search"))
        .query(&[
            ("query", json_quoted.as_str()),
            ("per_page", &per_page.to_string()),
            ("page", &page.to_string()),
            ("order_by", order),
        ])
        .send()
        .await?
        .json::<Value>()
        .await?;
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

/// Query Tendermint RPC `block` endpoint for a given height. Returns block.header.time.
pub async fn rpc_block_time(rpc: &str, height: u64) -> Result<String> {
    let url = format!("{rpc}/block?height={height}");
    let resp: BlockResponse = reqwest::get(&url).await?.json().await?;
    resp.result
        .and_then(|r| r.block)
        .and_then(|b| b.header)
        .and_then(|h| h.time)
        .ok_or_else(|| {
            eyre::eyre!("RPC response missing /result/block/header/time at height {height}")
        })
}

/// Fetch the current verifier set from Axelar chain via LCD REST endpoint.
/// Returns (signers sorted by address, threshold, nonce, verifierSetId)
pub async fn fetch_verifier_set(
    target_json: &Path,
    chain_axelar_id: &str,
) -> Result<(Vec<(Address, u128)>, u128, FixedBytes<32>, String)> {
    let content = fs::read_to_string(target_json)?;
    // Parse to `Value` first so a malformed-JSON file surfaces serde's parse
    // error verbatim, matching the original `from_str::<Value>(&content)?`.
    let raw: Value = serde_json::from_str(&content)?;
    let root: AxelarTargetJson =
        serde_json::from_value(raw).map_err(|_| eyre::eyre!("no axelar.lcd in target json"))?;

    let lcd = root
        .axelar
        .lcd
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no axelar.lcd in target json"))?;

    // The MultisigProver address is keyed by `chain_axelar_id` at runtime, so
    // its lookup stays a `Value::pointer` — the contracts map is dynamically
    // shaped (different contracts have different per-chain layouts).
    let contracts = root.axelar.contracts.unwrap_or(Value::Null);
    let prover_addr = contracts
        .pointer(&format!("/MultisigProver/{chain_axelar_id}/address"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no MultisigProver.{chain_axelar_id}.address in target json"))?;

    let query_msg = "\"current_verifier_set\"";
    let query_b64 = base64::engine::general_purpose::STANDARD.encode(query_msg.as_bytes());

    let url = format!("{lcd}/cosmwasm/wasm/v1/contract/{prover_addr}/smart/{query_b64}");
    ui::info(&format!("fetching verifier set from: {url}"));

    let resp: VerifierSetResponse = reqwest::get(&url).await?.json().await?;

    let data = resp
        .data
        .ok_or_else(|| eyre::eyre!("no id in verifier set response"))?;
    let verifier_set_id = data
        .id
        .ok_or_else(|| eyre::eyre!("no id in verifier set response"))?;

    let verifier_set = data
        .verifier_set
        .ok_or_else(|| eyre::eyre!("no signers object in verifier set"))?;
    let signers_obj = verifier_set
        .signers
        .ok_or_else(|| eyre::eyre!("no signers object in verifier set"))?;

    let threshold: u128 = verifier_set
        .threshold
        .as_str()
        .or_else(|| verifier_set.threshold.as_u64().map(|_| ""))
        .ok_or_else(|| eyre::eyre!("no threshold in verifier set"))
        .and_then(|s| {
            if s.is_empty() {
                Ok(verifier_set.threshold.as_u64().unwrap() as u128)
            } else {
                s.parse::<u128>()
                    .map_err(|e| eyre::eyre!("invalid threshold: {e}"))
            }
        })?;

    let created_at = verifier_set
        .created_at
        .ok_or_else(|| eyre::eyre!("no created_at in verifier set"))?;

    let nonce = FixedBytes::<32>::from(U256::from(created_at).to_be_bytes::<32>());

    let mut weighted_signers: Vec<(Address, u128)> = Vec::new();

    for signer in signers_obj.values() {
        let pubkey_hex = signer
            .pub_key
            .as_ref()
            .and_then(|p| p.ecdsa.as_deref())
            .ok_or_else(|| eyre::eyre!("no pub_key.ecdsa for signer"))?;

        let weight: u128 = signer
            .weight
            .as_str()
            .map(|s| s.parse::<u128>())
            .unwrap_or_else(|| Ok(signer.weight.as_u64().unwrap_or(1) as u128))
            .map_err(|e| eyre::eyre!("invalid weight: {e}"))?;

        let pubkey_bytes = hex::decode(pubkey_hex.strip_prefix("0x").unwrap_or(pubkey_hex))?;
        let addr = pubkey_to_address(&pubkey_bytes)?;
        weighted_signers.push((addr, weight));
    }

    weighted_signers.sort_by_key(|(addr, _)| *addr);

    ui::kv(
        "verifier set",
        &format!(
            "{} signers, threshold={}, created_at={}, id={}",
            weighted_signers.len(),
            threshold,
            created_at,
            verifier_set_id
        ),
    );
    for (addr, weight) in &weighted_signers {
        ui::kv(&format!("{addr}"), &format!("weight={weight}"));
    }

    Ok((weighted_signers, threshold, nonce, verifier_set_id))
}
