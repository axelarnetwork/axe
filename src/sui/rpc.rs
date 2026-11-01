//! Sui gRPC client with auto-fallback to public endpoints, plus the helpers
//! that lift proto responses into `sui_sdk_types` values.
//!
//! Sui retired JSON-RPC on its public fullnodes (every method now answers
//! "JSON-RPC on public fullnodes has been deprecated"), so this talks the
//! gRPC API (`sui.rpc.v2`) through Mysten's `sui-rpc` crate instead.

use std::time::Duration;

use eyre::{Result, eyre};
use futures::StreamExt;
use serde_json::{Value, json};
use sui_rpc::field::{FieldMask, FieldMaskUtil};
use sui_rpc::proto::sui::rpc::v2::owner::OwnerKind;
use sui_rpc::proto::sui::rpc::v2::simulate_transaction_request::TransactionChecks;
use sui_rpc::proto::sui::rpc::v2::{
    Bcs, EventFilter, ExecuteTransactionRequest, ExecutedTransaction, GetBalanceRequest,
    GetObjectRequest, GetServiceInfoRequest, ListEventsRequest, ListOwnedObjectsRequest, Object,
    Ordering, QueryOptions, SimulateTransactionRequest, Transaction, TransactionKind,
    UserSignature, filter,
};
use sui_sdk_types::{Address as SuiAddress, Digest, ObjectReference};
use tonic::{Code, Status};

use super::tx::{SubmittedTx, SuiEvent};
use crate::retry::{FALLBACK_ATTEMPTS, retry_async};

/// Public Sui gRPC endpoints used as silent fallbacks if the configured
/// endpoint errors. Mainnet and testnet share keys; we pick by URL hint.
const TESTNET_FALLBACKS: &[&str] = &[
    "https://fullnode.testnet.sui.io:443",
    "https://sui-testnet-grpc.publicnode.com:443",
];
const MAINNET_FALLBACKS: &[&str] = &[
    "https://fullnode.mainnet.sui.io:443",
    "https://sui-grpc.publicnode.com:443",
];

/// How long `execute_transaction` waits for the submitted transaction to land
/// in a checkpoint. The node's indexes (owned objects, events) only reflect
/// checkpointed transactions, so the next PTB from the same wallet would
/// otherwise pick stale gas-coin versions. Checkpoints land every few seconds.
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
enum SuiRpcError {
    #[error("Sui gRPC {endpoint} {method}: {status}")]
    Rpc {
        endpoint: String,
        method: &'static str,
        status: Status,
    },
    #[error("Sui gRPC: no usable endpoints")]
    Exhausted,
}

impl SuiRpcError {
    /// Whether another pass over the endpoints could plausibly succeed.
    /// Transport-level and overload statuses are endpoint flakiness; a
    /// deterministic rejection (bad argument, object not found, Move abort
    /// surfaced as a precondition) answers the same on every pass, so
    /// retrying just delays the real error.
    fn is_transient(&self) -> bool {
        match self {
            Self::Rpc { status, .. } => !matches!(
                status.code(),
                Code::InvalidArgument
                    | Code::NotFound
                    | Code::AlreadyExists
                    | Code::PermissionDenied
                    | Code::Unauthenticated
                    | Code::FailedPrecondition
                    | Code::OutOfRange
                    | Code::Unimplemented
            ),
            Self::Exhausted => false,
        }
    }
}

#[derive(Clone)]
struct Endpoint {
    url: String,
    client: sui_rpc::Client,
}

#[derive(Clone)]
pub struct SuiClient {
    /// Configured endpoint first, then the public fallbacks.
    endpoints: Vec<Endpoint>,
}

impl SuiClient {
    pub fn new(rpc_url: &str) -> Self {
        let primary = rpc_url.trim_end_matches('/').to_string();
        let fallbacks = if primary.contains("mainnet") {
            MAINNET_FALLBACKS
        } else {
            TESTNET_FALLBACKS
        };
        let endpoints = std::iter::once(primary.as_str())
            .chain(fallbacks.iter().copied().filter(|u| *u != primary))
            .filter_map(|url| match sui_rpc::Client::new(url) {
                Ok(client) => Some(Endpoint {
                    url: url.to_string(),
                    client,
                }),
                Err(err) => {
                    crate::ui::warn(&format!("Sui gRPC: skipping endpoint {url}: {err}"));
                    None
                }
            })
            .collect();
        Self { endpoints }
    }

    /// One pass over `[primary, ...fallbacks]`, retried up to
    /// [`FALLBACK_ATTEMPTS`] passes with geometric backoff while the last
    /// error of a pass looks transient ([`SuiRpcError::is_transient`]).
    /// `op` is re-invoked per endpoint with a handle on that endpoint, so
    /// re-sending a signed transaction is safe — Sui dedups by digest.
    async fn call<T, F, Fut>(&self, method: &'static str, op: F) -> Result<T>
    where
        F: Fn(sui_rpc::Client) -> Fut,
        Fut: Future<Output = std::result::Result<T, Status>>,
    {
        retry_async(
            &format!("Sui gRPC {method}"),
            FALLBACK_ATTEMPTS,
            SuiRpcError::is_transient,
            || self.call_each_endpoint(method, &op),
        )
        .await
        .map_err(eyre::Report::new)
    }

    /// Single pass: try each endpoint in order, moving on to the next
    /// whenever one fails; returns the last endpoint's error if all do.
    async fn call_each_endpoint<T, F, Fut>(
        &self,
        method: &'static str,
        op: &F,
    ) -> std::result::Result<T, SuiRpcError>
    where
        F: Fn(sui_rpc::Client) -> Fut,
        Fut: Future<Output = std::result::Result<T, Status>>,
    {
        let mut last_err = None;
        for endpoint in &self.endpoints {
            match op(endpoint.client.clone()).await {
                Ok(value) => return Ok(value),
                Err(status) => {
                    last_err = Some(SuiRpcError::Rpc {
                        endpoint: endpoint.url.clone(),
                        method,
                        status,
                    });
                }
            }
        }
        Err(last_err.unwrap_or(SuiRpcError::Exhausted))
    }

    pub async fn get_chain_identifier(&self) -> Result<String> {
        self.call("GetServiceInfo", |mut client| async move {
            let info = client
                .ledger_client()
                .get_service_info(GetServiceInfoRequest::default())
                .await?
                .into_inner();
            Ok(info.chain_id().to_string())
        })
        .await
    }

    pub async fn get_reference_gas_price(&self) -> Result<u64> {
        self.call("GetEpoch", |mut client| async move {
            client.get_reference_gas_price().await
        })
        .await
    }

    pub async fn get_balance(&self, owner: &SuiAddress) -> Result<u64> {
        let owner = owner_addr_hex(owner);
        self.call("GetBalance", |mut client| {
            let request = GetBalanceRequest::default()
                .with_owner(owner.clone())
                .with_coin_type("0x2::sui::SUI");
            async move {
                let response = client.state_client().get_balance(request).await?;
                Ok(response.into_inner().balance().balance())
            }
        })
        .await
    }

    /// Fetch ALL owned `Coin<T>` objects for `owner`, every page, sorted
    /// largest-first. A single page (100 items) is NOT the whole wallet:
    /// cron refunds fragment it into hundreds of small coins, and picking
    /// only from page one made the largest real coin invisible.
    pub async fn all_coins_of_type(
        &self,
        owner: &SuiAddress,
        coin_type: &str,
    ) -> Result<Vec<(ObjectReference, u128)>> {
        // 20 pages x 100 = 2000 objects — far beyond any sane wallet.
        const MAX_PAGES: usize = 20;
        let owner = owner_addr_hex(owner);
        let object_type = format!("0x2::coin::Coin<{coin_type}>");
        let mut coins: Vec<(ObjectReference, u128)> = Vec::new();
        let mut page_token: Option<Vec<u8>> = None;
        for _ in 0..MAX_PAGES {
            let page = self
                .call("ListOwnedObjects", |mut client| {
                    let mut request = ListOwnedObjectsRequest::default()
                        .with_owner(owner.clone())
                        .with_object_type(object_type.clone())
                        .with_page_size(100)
                        .with_read_mask(FieldMask::from_paths([
                            "object_id",
                            "version",
                            "digest",
                            "balance",
                        ]));
                    if let Some(token) = page_token.clone() {
                        request = request.with_page_token(token);
                    }
                    async move {
                        let response = client.state_client().list_owned_objects(request).await?;
                        Ok(response.into_inner())
                    }
                })
                .await?;
            for object in page.objects() {
                coins.push((object_ref_from_proto(object)?, u128::from(object.balance())));
            }
            match page.next_page_token_opt() {
                Some(token) if !token.is_empty() => page_token = Some(token.to_vec()),
                _ => break,
            }
        }
        coins.sort_by_key(|(_, bal)| std::cmp::Reverse(*bal));
        Ok(coins)
    }

    /// Collect SUI gas coins totalling at least `min_total_mist`, largest
    /// first (capped at 200 — Sui accepts up to 256 gas-payment objects).
    /// Every listed object is "smashed" into one by the runtime before
    /// execution, so this both survives a fragmented wallet (observed live:
    /// 150+ sub-0.2-SUI objects while no single object covered the
    /// cross-chain gas split) and permanently defragments it a little on
    /// every run.
    pub async fn pick_gas_coins(
        &self,
        owner: &SuiAddress,
        min_total_mist: u128,
    ) -> Result<Vec<ObjectReference>> {
        const MAX_GAS_OBJECTS: usize = 200;
        let coins = self.all_coins_of_type(owner, "0x2::sui::SUI").await?;
        if coins.is_empty() {
            return Err(eyre!(
                "wallet {} has no SUI coins — fund it first",
                owner_addr_hex(owner)
            ));
        }
        let total: u128 = coins.iter().map(|(_, bal)| bal).sum();
        let mut picked = Vec::new();
        let mut sum: u128 = 0;
        for (obj, bal) in coins {
            if (sum >= min_total_mist && !picked.is_empty()) || picked.len() >= MAX_GAS_OBJECTS {
                break;
            }
            sum += bal;
            picked.push(obj);
        }
        if sum < min_total_mist {
            return Err(eyre!(
                "wallet {} holds {total} mist of SUI in total — less than the {min_total_mist} \
                 mist needed for gas budget + cross-chain gas",
                owner_addr_hex(owner)
            ));
        }
        Ok(picked)
    }

    /// Fetch the `initial_shared_version` of a shared object, which is what
    /// PTB shared-object inputs need.
    pub async fn get_shared_object_initial_version(&self, object_id: &SuiAddress) -> Result<u64> {
        let object_id = owner_addr_hex(object_id);
        self.call("GetObject", |mut client| {
            let request = GetObjectRequest::default()
                .with_object_id(object_id.clone())
                .with_read_mask(FieldMask::from_paths(["owner"]));
            let object_id = object_id.clone();
            async move {
                let response = client.ledger_client().get_object(request).await?;
                let owner = response.into_inner().object().owner().clone();
                if owner.kind() == OwnerKind::Shared {
                    Ok(owner.version())
                } else {
                    Err(Status::failed_precondition(format!(
                        "object {object_id} is not Shared; owner = {owner:?}"
                    )))
                }
            }
        })
        .await
    }

    /// The address (or parent object id) owning `object_id`; `None` for
    /// shared or immutable objects.
    pub async fn get_object_owner(&self, object_id: &str) -> Result<Option<String>> {
        let object_id = object_id.to_string();
        self.call("GetObject", |mut client| {
            let request = GetObjectRequest::default()
                .with_object_id(object_id.clone())
                .with_read_mask(FieldMask::from_paths(["owner"]));
            async move {
                let response = client.ledger_client().get_object(request).await?;
                let owner = response.into_inner().object().owner().clone();
                Ok(match owner.kind() {
                    OwnerKind::Address | OwnerKind::Object | OwnerKind::ConsensusAddress => {
                        Some(owner.address().to_string())
                    }
                    _ => None,
                })
            }
        })
        .await
    }

    /// Submit a fully-signed transaction and wait for it to be checkpointed,
    /// so the node's indexes reflect it before the next call.
    pub async fn execute_transaction(
        &self,
        tx_bcs: &[u8],
        sigs: &[Vec<u8>],
    ) -> Result<SubmittedTx> {
        let request = ExecuteTransactionRequest::default()
            .with_transaction(Transaction::default().with_bcs(Bcs::from(tx_bcs.to_vec())))
            .with_signatures(
                sigs.iter()
                    .map(|sig| UserSignature::default().with_bcs(Bcs::from(sig.clone())))
                    .collect::<Vec<_>>(),
            )
            .with_read_mask(FieldMask::from_paths([
                "digest",
                "effects.status",
                "events.events.event_type",
                "events.events.json",
            ]));
        let executed = self
            .call("ExecuteTransaction", |mut client| {
                let request = request.clone();
                async move { execute_and_wait(&mut client, request).await }
            })
            .await?;
        Ok(submitted_tx(&executed))
    }

    /// Dry-run `kind` as `sender` (no gas needed, like JSON-RPC's
    /// `devInspect`) and return the BCS bytes of the first value returned by
    /// command `command_index`.
    pub async fn simulate_return_value(
        &self,
        sender: &SuiAddress,
        kind: TransactionKind,
        command_index: usize,
    ) -> Result<Vec<u8>> {
        let request = SimulateTransactionRequest::default()
            .with_transaction(
                Transaction::default()
                    .with_kind(kind)
                    .with_sender(owner_addr_hex(sender)),
            )
            .with_checks(TransactionChecks::Disabled)
            .with_read_mask(FieldMask::from_paths([
                "transaction.effects.status",
                "command_outputs",
            ]));
        self.call("SimulateTransaction", |mut client| {
            let request = request.clone();
            async move {
                let response = client
                    .execution_client()
                    .simulate_transaction(request)
                    .await?
                    .into_inner();
                let status = response.transaction().effects().status();
                if !status.success() {
                    return Err(Status::failed_precondition(format!(
                        "simulation failed: {}",
                        status.error().description()
                    )));
                }
                response
                    .command_outputs()
                    .get(command_index)
                    .and_then(|outputs| outputs.return_values().first())
                    .map(|output| output.value().value().to_vec())
                    .ok_or_else(|| {
                        Status::data_loss(format!(
                            "simulation returned no value for command {command_index}"
                        ))
                    })
            }
        })
        .await
    }

    /// Whether a recent `MessageApproved` event of the gateway events module
    /// matches `(source_chain, message_id)`.
    pub async fn has_message_approved(
        &self,
        gateway_events_type: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<bool> {
        self.has_matching_event(gateway_events_type, source_chain, message_id)
            .await
    }

    /// Whether a recent `MessageExecuted` event matches `(source_chain, message_id)`.
    pub async fn has_message_executed(
        &self,
        gateway_events_type: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<bool> {
        self.has_matching_event(gateway_events_type, source_chain, message_id)
            .await
    }

    async fn has_matching_event(
        &self,
        move_event_type: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<bool> {
        // Walk `ListEvents` newest-first until we find a matching
        // `(source_chain, message_id)` or have scanned `MAX_EVENTS`. The
        // gateway can emit hundreds of MessageApproved events per minute on
        // testnet, so a small newest snapshot isn't enough — our event ages
        // out before we poll.
        const PAGE_SIZE: u32 = 100;
        const MAX_EVENTS: usize = 2000;
        let request = ListEventsRequest::default()
            .with_filter(EventFilter::matching(filter::event::event_type(
                move_event_type,
            )))
            .with_read_mask(FieldMask::from_paths([
                "json",
                "checkpoint",
                "transaction_index",
                "event_index",
            ]))
            .with_options(
                QueryOptions::default()
                    .with_ordering(Ordering::Descending)
                    .with_limit(PAGE_SIZE),
            );
        self.call("ListEvents", |client| {
            let request = request.clone();
            async move {
                let mut events = std::pin::pin!(client.list_events(request).take(MAX_EVENTS));
                while let Some(response) = events.next().await {
                    let response = response?;
                    let Some(event) = response.event_opt() else {
                        continue;
                    };
                    if event_matches(&prost_to_json(event.json_opt()), source_chain, message_id) {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        })
        .await
    }
}

/// Submit and wait for the checkpoint. A transaction that executed but whose
/// checkpoint confirmation failed is still a submitted transaction, so it is
/// returned (with a warning) rather than resubmitted.
async fn execute_and_wait(
    client: &mut sui_rpc::Client,
    request: ExecuteTransactionRequest,
) -> std::result::Result<ExecutedTransaction, Status> {
    use sui_rpc::client::ExecuteAndWaitError;

    let response = match client
        .execute_transaction_and_wait_for_checkpoint(request, CHECKPOINT_TIMEOUT)
        .await
    {
        Ok(response) => response,
        Err(ExecuteAndWaitError::RpcError(status)) => return Err(status),
        Err(ExecuteAndWaitError::CheckpointTimeout(response)) => {
            crate::ui::warn("Sui gRPC: transaction executed, but checkpoint wait timed out");
            response
        }
        Err(ExecuteAndWaitError::CheckpointStreamError { response, error }) => {
            crate::ui::warn(&format!(
                "Sui gRPC: transaction executed, but checkpoint stream failed: {error}"
            ));
            response
        }
        Err(err) => return Err(Status::invalid_argument(err.to_string())),
    };
    Ok(response.into_inner().transaction().clone())
}

fn submitted_tx(executed: &ExecutedTransaction) -> SubmittedTx {
    let status = executed.effects().status();
    let success = status.success();
    let error = (!success).then(|| status.error().description().to_string());
    let events = executed
        .events()
        .events()
        .iter()
        .map(|event| SuiEvent {
            event_type: event.event_type().to_string(),
            fields: prost_to_json(event.json_opt()),
        })
        .collect();
    SubmittedTx {
        digest: executed.digest().to_string(),
        success,
        error,
        events,
    }
}

/// Sui's `MessageApproved` events nest `{source_chain, message_id, ...}`
/// under a `message` key. `MessageExecuted` emits the fields at the top
/// level. Try both shapes.
fn event_matches(parsed: &Value, source_chain: &str, message_id: &str) -> bool {
    let inner = parsed.get("message").unwrap_or(parsed);
    let field = |key: &str| inner.get(key).and_then(Value::as_str).unwrap_or_default();
    field("source_chain") == source_chain && field("message_id") == message_id
}

/// Convert the gRPC JSON rendering (`google.protobuf.Value`) into `serde_json`.
fn prost_to_json(value: Option<&prost_types::Value>) -> Value {
    use prost_types::value::Kind;

    match value.and_then(|v| v.kind.as_ref()) {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(b)) => Value::Bool(*b),
        Some(Kind::NumberValue(n)) => json!(n),
        Some(Kind::StringValue(s)) => Value::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            Value::Array(list.values.iter().map(|v| prost_to_json(Some(v))).collect())
        }
        Some(Kind::StructValue(fields)) => Value::Object(
            fields
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), prost_to_json(Some(v))))
                .collect(),
        ),
    }
}

pub(super) fn owner_addr_hex(a: &SuiAddress) -> String {
    format!("0x{}", hex::encode(a.as_bytes()))
}

fn object_ref_from_proto(object: &Object) -> Result<ObjectReference> {
    let id = SuiAddress::from_hex(object.object_id())
        .map_err(|e| eyre!("objectId hex '{}': {e:?}", object.object_id()))?;
    let digest: Digest = object
        .digest()
        .parse()
        .map_err(|e| eyre!("object digest '{}': {e:?}", object.digest()))?;
    Ok(ObjectReference::new(id, object.version(), digest))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Endpoint, MAINNET_FALLBACKS, SuiClient, TESTNET_FALLBACKS, event_matches};

    #[test]
    fn event_match_handles_both_event_shapes() {
        let approved = json!({"message": {"source_chain": "axelar", "message_id": "0xab-1"}});
        let executed = json!({"source_chain": "axelar", "message_id": "0xab-1"});
        assert!(event_matches(&approved, "axelar", "0xab-1"));
        assert!(event_matches(&executed, "axelar", "0xab-1"));
        assert!(!event_matches(&executed, "axelar", "0xab-2"));
        assert!(!event_matches(&json!(null), "axelar", "0xab-1"));
    }

    /// Network probe for the hardcoded fallback gRPC endpoints. Run on demand
    /// with `cargo test -- --ignored` when a cron run smells like a dead
    /// endpoint - a retired hostname here fails preflight for every route.
    #[tokio::test]
    #[ignore = "hits live public endpoints"]
    async fn fallback_endpoints_are_alive() {
        let mut dead = Vec::new();
        for url in MAINNET_FALLBACKS.iter().chain(TESTNET_FALLBACKS) {
            // One endpoint at a time: `SuiClient::new` would mask a dead
            // endpoint with its fallbacks.
            let grpc = match sui_rpc::Client::new(*url) {
                Ok(grpc) => grpc,
                Err(err) => {
                    dead.push(format!("{url}: {err}"));
                    continue;
                }
            };
            let client = SuiClient {
                endpoints: vec![Endpoint {
                    url: (*url).to_string(),
                    client: grpc,
                }],
            };
            if let Err(err) = client.get_chain_identifier().await {
                dead.push(format!("{url}: {err}"));
            }
        }
        assert!(
            dead.is_empty(),
            "dead Sui fallback endpoints:\n{}",
            dead.join("\n")
        );
    }
}
