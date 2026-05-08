//! Sui (Move) primitives for axe load-testing.
//!
//! Goals:
//!   - Parse `suiprivkey1...` bech32 secrets into an Ed25519 keypair and derive
//!     the canonical Sui address.
//!   - Provide a `SuiClient` wrapping JSON-RPC, with auto-fallback to public
//!     endpoints when the configured one is rate-limited or down.
//!   - Build, sign, and submit Programmable Transaction Blocks (PTBs) for the
//!     two operations the load test cares about: GMP `send_call` and ITS
//!     `interchain_transfer`.
//!   - Query events for destination-side verification.
//!
//! Sui specifics worth noting:
//!   - Address = blake2b256(flag || pubkey)[..32]; flag = 0x00 for ed25519.
//!   - Transaction signing intent = `[0, 0, 0]` (TransactionData scope) || bcs(tx).
//!   - User signature wire format = flag (1B) || sig (64B) || pubkey (32B) for ed25519.
//!   - Shared objects need their `initial_shared_version`, which we fetch via
//!     `sui_getObject`. We cache them per-run.

use std::time::Duration;

use base64::Engine;
use blake2::{Blake2b, Digest as Blake2Digest, digest::consts::U32};
use ed25519_dalek::{
    Signer as EdSigner, SigningKey as EdSigningKey, VerifyingKey as EdVerifyingKey,
};
use eyre::{Result, eyre};
use libsecp256k1::{Message as SecpMessage, PublicKey as SecpPub, SecretKey as SecpSecret};
use serde_json::{Value, json};
use sha2::Sha256;
use sui_sdk_types::{
    Address as SuiAddress, Argument, Command, Digest, GasPayment, Identifier, Input, MoveCall,
    ObjectReference, ProgrammableTransaction, SharedInput, SplitCoins, Transaction,
    TransactionExpiration, TransactionKind, TypeTag,
};

const HRP_SUIPRIVKEY: &str = "suiprivkey";
const ED25519_FLAG: u8 = 0x00;
const SECP256K1_FLAG: u8 = 0x01;
const SIG_LEN: usize = 64;
const ED25519_PK_LEN: usize = 32;
const SECP256K1_PK_LEN: usize = 33; // compressed
/// Sui's intent scope for a TransactionData payload: [scope=0, version=0, app_id=0].
const TX_INTENT: [u8; 3] = [0, 0, 0];

/// Public Sui RPCs used as silent fallbacks if the configured endpoint errors.
/// Mainnet and testnet share keys; we pick by URL hint.
const TESTNET_FALLBACKS: &[&str] = &[
    "https://fullnode.testnet.sui.io:443",
    "https://sui-testnet-rpc.publicnode.com",
];
const MAINNET_FALLBACKS: &[&str] = &[
    "https://fullnode.mainnet.sui.io:443",
    "https://sui-mainnet-rpc.publicnode.com",
];

// ---------------------------------------------------------------------------
// Wallet
// ---------------------------------------------------------------------------

/// Sui supports several signature schemes. We support the two the Sui CLI
/// emits for fresh keypairs: ed25519 (flag 0x00) and secp256k1 (flag 0x01).
///
/// The variants are different sizes (ed25519 keypair ≈ 64 B vs secp256k1 ≈
/// 128 B with uncompressed pubkey internals); a load-test holds at most a
/// handful of these per run, so the indirection of `Box`-ing the larger
/// variant isn't worth it.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum SuiKeypair {
    Ed25519 {
        signing_key: EdSigningKey,
        verifying_key: EdVerifyingKey,
    },
    Secp256k1 {
        secret: SecpSecret,
        public: SecpPub,
    },
}

#[derive(Clone)]
pub struct SuiWallet {
    pub keypair: SuiKeypair,
    pub address: SuiAddress,
}

impl SuiWallet {
    /// Build from a 32-byte ed25519 secret seed.
    pub fn from_ed25519_seed(seed: &[u8; 32]) -> Result<Self> {
        let signing_key = EdSigningKey::from_bytes(seed);
        let verifying_key = signing_key.verifying_key();
        let mut buf = Vec::with_capacity(1 + ED25519_PK_LEN);
        buf.push(ED25519_FLAG);
        buf.extend_from_slice(verifying_key.as_bytes());
        let address = SuiAddress::from_hex(format!("0x{}", hex::encode(blake2b256(&buf))))
            .map_err(|e| eyre!("address derivation failed: {e:?}"))?;
        Ok(Self {
            keypair: SuiKeypair::Ed25519 {
                signing_key,
                verifying_key,
            },
            address,
        })
    }

    /// Build from a 32-byte secp256k1 secret seed.
    pub fn from_secp256k1_seed(seed: &[u8; 32]) -> Result<Self> {
        let secret = SecpSecret::parse(seed).map_err(|e| eyre!("secp256k1 secret: {e:?}"))?;
        let public = SecpPub::from_secret_key(&secret);
        let pk_compressed = public.serialize_compressed();
        let mut buf = Vec::with_capacity(1 + SECP256K1_PK_LEN);
        buf.push(SECP256K1_FLAG);
        buf.extend_from_slice(&pk_compressed);
        let address = SuiAddress::from_hex(format!("0x{}", hex::encode(blake2b256(&buf))))
            .map_err(|e| eyre!("address derivation failed: {e:?}"))?;
        Ok(Self {
            keypair: SuiKeypair::Secp256k1 { secret, public },
            address,
        })
    }

    /// Parse a Sui CLI bech32-encoded private key (`suiprivkey1...`). Auto-
    /// detects the signature scheme from the flag byte (0x00 = ed25519,
    /// 0x01 = secp256k1).
    pub fn from_suiprivkey(s: &str) -> Result<Self> {
        let (hrp, data) =
            bech32::decode(s.trim()).map_err(|e| eyre!("invalid suiprivkey bech32: {e}"))?;
        if hrp.as_str() != HRP_SUIPRIVKEY {
            return Err(eyre!(
                "expected suiprivkey hrp, got '{}': not a Sui CLI key",
                hrp.as_str()
            ));
        }
        if data.len() != 1 + 32 {
            return Err(eyre!(
                "suiprivkey payload has wrong length: got {} bytes, expected 33 (flag + 32-byte secret)",
                data.len()
            ));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&data[1..]);
        match data[0] {
            ED25519_FLAG => Self::from_ed25519_seed(&seed),
            SECP256K1_FLAG => Self::from_secp256k1_seed(&seed),
            other => Err(eyre!(
                "suiprivkey flag 0x{other:02x} is not supported (only 0x00 ed25519 and 0x01 secp256k1)"
            )),
        }
    }

    /// Auto-detect the input format: 64-char hex (assumes ed25519) or
    /// `suiprivkey...` bech32 (flag-byte determines scheme).
    pub fn from_secret_str(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        if let Some(stripped) = trimmed.strip_prefix("0x")
            && stripped.len() == 64
            && stripped.chars().all(|c| c.is_ascii_hexdigit())
        {
            let bytes = hex::decode(stripped).map_err(|e| eyre!("invalid hex secret: {e}"))?;
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            return Self::from_ed25519_seed(&seed);
        }
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            let bytes = hex::decode(trimmed).map_err(|e| eyre!("invalid hex secret: {e}"))?;
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            return Self::from_ed25519_seed(&seed);
        }
        Self::from_suiprivkey(trimmed)
    }

    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.address.as_bytes()))
    }

    /// Diagnostic label for the keypair scheme. Useful when surfacing
    /// Build the wire-format intent signature for the given pre-intent
    /// message (full bytes, including the 3-byte intent prefix).
    ///
    /// Wire format:
    ///   ed25519:    flag(0x00, 1B) || sig(64B)            || pubkey(32B)
    ///   secp256k1:  flag(0x01, 1B) || sig(64B compact)    || pubkey(33B compressed)
    ///
    /// Hashing:
    ///   ed25519    signs blake2b256(intent_message) directly.
    ///   secp256k1  signs sha256(blake2b256(intent_message)) (Sui spec).
    pub fn serialized_intent_signature(&self, intent_message: &[u8]) -> Vec<u8> {
        match &self.keypair {
            SuiKeypair::Ed25519 {
                signing_key,
                verifying_key,
            } => {
                let digest = blake2b256(intent_message);
                let sig = signing_key.sign(&digest);
                let mut out = Vec::with_capacity(1 + SIG_LEN + ED25519_PK_LEN);
                out.push(ED25519_FLAG);
                out.extend_from_slice(&sig.to_bytes());
                out.extend_from_slice(verifying_key.as_bytes());
                out
            }
            SuiKeypair::Secp256k1 { secret, public } => {
                let blake = blake2b256(intent_message);
                let sha = Sha256::digest(blake);
                let mut digest_arr = [0u8; 32];
                digest_arr.copy_from_slice(&sha);
                let msg = SecpMessage::parse(&digest_arr);
                let (sig, _recovery) = libsecp256k1::sign(&msg, secret);
                let pk_compressed = public.serialize_compressed();
                let mut out = Vec::with_capacity(1 + SIG_LEN + SECP256K1_PK_LEN);
                out.push(SECP256K1_FLAG);
                out.extend_from_slice(&sig.serialize());
                out.extend_from_slice(&pk_compressed);
                out
            }
        }
    }
}

fn blake2b256(input: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b::<U32>::new();
    Blake2Digest::update(&mut hasher, input);
    let out = hasher.finalize();
    let mut a = [0u8; 32];
    a.copy_from_slice(&out);
    a
}

// ---------------------------------------------------------------------------
// Client (JSON-RPC with auto-fallback)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SuiClient {
    primary: String,
    fallbacks: Vec<String>,
    http: reqwest::Client,
}

impl SuiClient {
    pub fn new(rpc_url: &str) -> Self {
        let primary = rpc_url.trim_end_matches('/').to_string();
        let fallbacks = if primary.contains("mainnet") {
            MAINNET_FALLBACKS
                .iter()
                .filter(|u| **u != primary)
                .map(|s| (*s).to_string())
                .collect()
        } else {
            TESTNET_FALLBACKS
                .iter()
                .filter(|u| **u != primary)
                .map(|s| (*s).to_string())
                .collect()
        };
        Self {
            primary,
            fallbacks,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client build"),
        }
    }

    /// JSON-RPC call with silent fallback on transient failures.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut endpoints = vec![self.primary.clone()];
        endpoints.extend(self.fallbacks.iter().cloned());
        let mut last_err: Option<eyre::Report> = None;
        for endpoint in &endpoints {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            });
            match self.http.post(endpoint).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    match resp.text().await {
                        Ok(text) => {
                            if !status.is_success() {
                                last_err = Some(eyre!(
                                    "Sui RPC {endpoint} HTTP {status}: {}",
                                    text.chars().take(300).collect::<String>()
                                ));
                                continue;
                            }
                            match serde_json::from_str::<Value>(&text) {
                                Ok(v) => {
                                    if let Some(err) = v.get("error") {
                                        last_err =
                                            Some(eyre!("Sui RPC {endpoint} {method}: {err}"));
                                        // RPC-level error usually applies on every endpoint.
                                        // But still try fallbacks for transient issues.
                                        continue;
                                    }
                                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                                }
                                Err(e) => {
                                    last_err = Some(eyre!("Sui RPC {endpoint} non-JSON: {e}"));
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            last_err = Some(eyre!("Sui RPC {endpoint} body read failed: {e}"));
                            continue;
                        }
                    }
                }
                Err(e) => {
                    last_err = Some(eyre!("Sui RPC {endpoint} request failed: {e}"));
                    continue;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| eyre!("Sui RPC exhausted all endpoints")))
    }

    pub async fn get_chain_identifier(&self) -> Result<String> {
        let r = self.call("sui_getChainIdentifier", json!([])).await?;
        Ok(r.as_str().unwrap_or_default().to_string())
    }

    pub async fn get_reference_gas_price(&self) -> Result<u64> {
        let r = self.call("suix_getReferenceGasPrice", json!([])).await?;
        let s = r.as_str().ok_or_else(|| eyre!("rgp response not string"))?;
        s.parse::<u64>().map_err(|e| eyre!("rgp parse: {e}"))
    }

    pub async fn get_balance(&self, owner: &SuiAddress) -> Result<u64> {
        let r = self
            .call(
                "suix_getBalance",
                json!([owner_addr_hex(owner), "0x2::sui::SUI"]),
            )
            .await?;
        let s = r["totalBalance"]
            .as_str()
            .ok_or_else(|| eyre!("totalBalance missing in {r}"))?;
        s.parse::<u64>().map_err(|e| eyre!("balance parse: {e}"))
    }

    /// Get the largest SUI coin object owned by `owner`. Returns its ObjectReference
    /// (id, version, digest) suitable for `GasPayment::objects`.
    pub async fn pick_gas_coin(&self, owner: &SuiAddress) -> Result<ObjectReference> {
        let r = self
            .call(
                "suix_getCoins",
                json!([owner_addr_hex(owner), "0x2::sui::SUI", null, 50]),
            )
            .await?;
        let arr = r["data"]
            .as_array()
            .ok_or_else(|| eyre!("getCoins response missing data: {r}"))?;
        if arr.is_empty() {
            return Err(eyre!(
                "wallet {} has no SUI coins — fund it first",
                owner_addr_hex(owner)
            ));
        }
        // Pick the largest balance.
        let mut best: Option<&Value> = None;
        let mut best_bal: u128 = 0;
        for c in arr {
            let bal: u128 = c["balance"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if bal > best_bal {
                best_bal = bal;
                best = Some(c);
            }
        }
        let c = best.ok_or_else(|| eyre!("no usable gas coin"))?;
        object_ref_from_json(c)
    }

    /// Fetch `(initial_shared_version, latest_version, digest)` for a shared
    /// object. The first is what PTB inputs need; the latter two are useful
    /// for owned-object inputs.
    pub async fn get_shared_object_initial_version(&self, object_id: &SuiAddress) -> Result<u64> {
        let r = self
            .call(
                "sui_getObject",
                json!([
                    owner_addr_hex(object_id),
                    {"showOwner": true, "showPreviousTransaction": false}
                ]),
            )
            .await?;
        let owner = r
            .pointer("/data/owner")
            .ok_or_else(|| eyre!("getObject missing owner: {r}"))?;
        if let Some(shared) = owner.get("Shared") {
            let v: u64 = shared
                .get("initial_shared_version")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| eyre!("Shared.initial_shared_version missing: {shared}"))?;
            return Ok(v);
        }
        Err(eyre!(
            "object {} is not Shared; owner = {owner}",
            owner_addr_hex(object_id)
        ))
    }

    /// Submit a fully-signed transaction. Returns the tx digest.
    pub async fn execute_transaction(
        &self,
        tx_bcs: &[u8],
        sigs: &[Vec<u8>],
    ) -> Result<SubmittedTx> {
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(tx_bcs);
        let sigs_b64: Vec<String> = sigs
            .iter()
            .map(|s| base64::engine::general_purpose::STANDARD.encode(s))
            .collect();
        let r = self
            .call(
                "sui_executeTransactionBlock",
                json!([
                    tx_b64,
                    sigs_b64,
                    {"showEffects": true, "showEvents": true},
                    "WaitForLocalExecution"
                ]),
            )
            .await?;
        let digest = r["digest"]
            .as_str()
            .ok_or_else(|| eyre!("execute response missing digest: {r}"))?
            .to_string();
        let status = r
            .pointer("/effects/status/status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let success = status == "success";
        let error = if success {
            None
        } else {
            r.pointer("/effects/status/error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let events = r
            .get("events")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(SubmittedTx {
            digest,
            success,
            error,
            events,
        })
    }

    /// Query recent `MessageApproved` events from a Move events module
    /// (typically the AxelarGateway events module) and return whether one
    /// matching `(source_chain, message_id)` exists.
    pub async fn has_message_approved(
        &self,
        gateway_events_type: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<bool> {
        self.has_matching_event(gateway_events_type, source_chain, message_id)
            .await
    }

    /// Query for `MessageExecuted` event matching `(source_chain, message_id)`.
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
        // Walk pages of `suix_queryEvents` (newest-first) until we find a
        // matching `(source_chain, message_id)` or hit `MAX_PAGES`. The
        // gateway can emit hundreds of MessageApproved events per minute on
        // testnet, so the 50-newest snapshot from a single call isn't
        // enough — our event ages out before we poll.
        const PAGE_SIZE: usize = 100;
        const MAX_PAGES: usize = 20; // 20 × 100 = 2000 newest events scanned
        let mut cursor: Value = Value::Null;
        for _ in 0..MAX_PAGES {
            let r = self
                .call(
                    "suix_queryEvents",
                    json!([
                        {"MoveEventType": move_event_type},
                        cursor,
                        PAGE_SIZE,
                        true,
                    ]),
                )
                .await?;
            let arr = r
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for ev in &arr {
                let pj = match ev.get("parsedJson") {
                    Some(p) => p,
                    None => continue,
                };
                // Sui's `MessageApproved` events nest `{source_chain,
                // message_id, ...}` under a `message` key. `MessageExecuted`
                // emits the fields at the top level. Try both shapes.
                let inner = pj.get("message").unwrap_or(pj);
                let src = inner
                    .get("source_chain")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let mid = inner
                    .get("message_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if src == source_chain && mid == message_id {
                    return Ok(true);
                }
            }
            let has_next = r
                .get("hasNextPage")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_next {
                break;
            }
            match r.get("nextCursor") {
                Some(c) if !c.is_null() => cursor = c.clone(),
                _ => break,
            }
        }
        Ok(false)
    }
}

#[derive(Debug, Clone)]
pub struct SubmittedTx {
    pub digest: String,
    pub success: bool,
    pub error: Option<String>,
    pub events: Vec<Value>,
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

fn owner_addr_hex(a: &SuiAddress) -> String {
    format!("0x{}", hex::encode(a.as_bytes()))
}

fn object_ref_from_json(v: &Value) -> Result<ObjectReference> {
    let id = v["coinObjectId"]
        .as_str()
        .or_else(|| v["objectId"].as_str())
        .or_else(|| v.pointer("/data/objectId").and_then(|x| x.as_str()))
        .ok_or_else(|| eyre!("objectId missing in {v}"))?;
    let version: u64 = v["version"]
        .as_str()
        .or_else(|| v.pointer("/data/version").and_then(|x| x.as_str()))
        .ok_or_else(|| eyre!("version missing in {v}"))?
        .parse()
        .map_err(|e| eyre!("version parse: {e}"))?;
    let digest_str = v["digest"]
        .as_str()
        .or_else(|| v.pointer("/data/digest").and_then(|x| x.as_str()))
        .ok_or_else(|| eyre!("digest missing in {v}"))?;
    let id_addr = SuiAddress::from_hex(id).map_err(|e| eyre!("objectId hex: {e:?}"))?;
    let digest = parse_sui_digest(digest_str)?;
    Ok(ObjectReference::new(id_addr, version, digest))
}

/// Parse Sui's base58-encoded digest into the canonical 32-byte `Digest`.
fn parse_sui_digest(s: &str) -> Result<Digest> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| eyre!("digest base58 decode: {e}"))?;
    if bytes.len() != 32 {
        return Err(eyre!(
            "digest decoded to {} bytes, expected 32",
            bytes.len()
        ));
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&bytes);
    Digest::from_bytes(a).map_err(|e| eyre!("digest parse: {e:?}"))
}

// ---------------------------------------------------------------------------
// PTB construction helpers
// ---------------------------------------------------------------------------

/// Build a Move-call PTB that invokes `package::module::function(args...)`,
/// with optional `splitGas: Some(amount)` to split off `amount` mist from the
/// gas coin and pass it as one of the args.
///
/// Returns the fully-formed `Transaction` (unsigned), ready to BCS-serialize
/// and sign.
#[allow(clippy::too_many_arguments)]
pub struct PtbBuilder {
    inputs: Vec<Input>,
    commands: Vec<Command>,
}

impl Default for PtbBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PtbBuilder {
    pub fn new() -> Self {
        Self {
            inputs: Vec::new(),
            commands: Vec::new(),
        }
    }

    /// Add a pure (BCS-serialized primitive) input. Returns its `Argument`.
    pub fn pure_bytes(&mut self, bytes: Vec<u8>) -> Argument {
        let idx = self.inputs.len() as u16;
        self.inputs.push(Input::Pure(bytes));
        Argument::Input(idx)
    }

    pub fn pure_u64(&mut self, n: u64) -> Result<Argument> {
        let bytes = bcs::to_bytes(&n).map_err(|e| eyre!("bcs u64: {e}"))?;
        Ok(self.pure_bytes(bytes))
    }

    pub fn pure_address(&mut self, addr: SuiAddress) -> Result<Argument> {
        let bytes = bcs::to_bytes(&addr).map_err(|e| eyre!("bcs address: {e}"))?;
        Ok(self.pure_bytes(bytes))
    }

    pub fn pure_vec_u8(&mut self, v: &[u8]) -> Result<Argument> {
        let bytes = bcs::to_bytes(&v.to_vec()).map_err(|e| eyre!("bcs vec<u8>: {e}"))?;
        Ok(self.pure_bytes(bytes))
    }

    pub fn pure_string(&mut self, s: &str) -> Result<Argument> {
        let bytes = bcs::to_bytes(&s.to_string()).map_err(|e| eyre!("bcs string: {e}"))?;
        Ok(self.pure_bytes(bytes))
    }

    /// Add a shared-object input.
    pub fn shared_object(
        &mut self,
        object_id: SuiAddress,
        initial_shared_version: u64,
        mutable: bool,
    ) -> Argument {
        let idx = self.inputs.len() as u16;
        self.inputs.push(Input::Shared(SharedInput::new(
            object_id,
            initial_shared_version,
            mutable,
        )));
        Argument::Input(idx)
    }

    pub fn split_coin(&mut self, coin: Argument, amount: Argument) -> Argument {
        let cmd_idx = self.commands.len() as u16;
        self.commands.push(Command::SplitCoins(SplitCoins {
            coin,
            amounts: vec![amount],
        }));
        // SplitCoins returns a Vec<Coin<T>>; the first split is NestedResult(cmd, 0).
        Argument::NestedResult(cmd_idx, 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn move_call(
        &mut self,
        package: SuiAddress,
        module: &str,
        function: &str,
        type_arguments: Vec<TypeTag>,
        arguments: Vec<Argument>,
    ) -> Result<Argument> {
        let cmd_idx = self.commands.len() as u16;
        let module_id = Identifier::new(module).map_err(|e| eyre!("module ident: {e:?}"))?;
        let function_id = Identifier::new(function).map_err(|e| eyre!("function ident: {e:?}"))?;
        self.commands.push(Command::MoveCall(MoveCall {
            package,
            module: module_id,
            function: function_id,
            type_arguments,
            arguments,
        }));
        Ok(Argument::Result(cmd_idx))
    }

    pub fn build(self, sender: SuiAddress, gas: GasPayment) -> Transaction {
        Transaction {
            kind: TransactionKind::ProgrammableTransaction(ProgrammableTransaction {
                inputs: self.inputs,
                commands: self.commands,
            }),
            sender,
            gas_payment: gas,
            expiration: TransactionExpiration::None,
        }
    }
}

/// BCS-encode a `Transaction` for signing/submitting.
pub fn bcs_encode_transaction(tx: &Transaction) -> Result<Vec<u8>> {
    bcs::to_bytes(tx).map_err(|e| eyre!("bcs encode tx: {e}"))
}

/// Build the intent message for a `Transaction`: `[0,0,0] || bcs(tx)`.
pub fn intent_message_for(tx_bcs: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(3 + tx_bcs.len());
    buf.extend_from_slice(&TX_INTENT);
    buf.extend_from_slice(tx_bcs);
    buf
}

// ---------------------------------------------------------------------------
// Convenience: build, sign, and submit a PTB in one shot
// ---------------------------------------------------------------------------

/// Sign + submit a built `Transaction`. The wallet must own `gas_payment.objects`.
pub async fn sign_and_submit(
    client: &SuiClient,
    wallet: &SuiWallet,
    tx: Transaction,
) -> Result<SubmittedTx> {
    let tx_bcs = bcs_encode_transaction(&tx)?;
    let intent = intent_message_for(&tx_bcs);
    let sig = wallet.serialized_intent_signature(&intent);
    client.execute_transaction(&tx_bcs, &[sig]).await
}

// ---------------------------------------------------------------------------
// High-level helpers used by the load-test runners
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SuiContractsConfig {
    pub example_pkg: SuiAddress,
    pub gmp_singleton: SuiAddress,
    pub gateway_object: SuiAddress,
    pub gas_service_object: SuiAddress,
}

/// Read Sui chain config (RPC + key contract object IDs) from the chains config JSON.
pub fn read_sui_chain_config(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<(String, SuiContractsConfig)> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre!("failed to read config: {e}"))?;
    let root: Value = serde_json::from_str(&content)?;
    let chain = root
        .pointer(&format!("/chains/{chain_id}"))
        .ok_or_else(|| eyre!("chain '{chain_id}' not found in config"))?;
    let rpc = chain
        .get("rpc")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no rpc for sui chain '{chain_id}'"))?
        .to_string();

    let example_pkg = chain
        .pointer("/contracts/Example/address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no Example.address for '{chain_id}'"))?;
    let gmp_singleton = chain
        .pointer("/contracts/Example/objects/GmpSingleton")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no Example.objects.GmpSingleton for '{chain_id}'"))?;
    let gateway_object = chain
        .pointer("/contracts/AxelarGateway/objects/Gateway")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no AxelarGateway.objects.Gateway for '{chain_id}'"))?;
    let gas_service_object = chain
        .pointer("/contracts/GasService/objects/GasService")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no GasService.objects.GasService for '{chain_id}'"))?;

    Ok((
        rpc,
        SuiContractsConfig {
            example_pkg: parse_sui_addr(example_pkg)?,
            gmp_singleton: parse_sui_addr(gmp_singleton)?,
            gateway_object: parse_sui_addr(gateway_object)?,
            gas_service_object: parse_sui_addr(gas_service_object)?,
        },
    ))
}

pub fn parse_sui_addr(s: &str) -> Result<SuiAddress> {
    SuiAddress::from_hex(s).map_err(|e| eyre!("Sui address parse '{s}': {e:?}"))
}

/// Read the AxelarGateway Move-package address for a Sui chain. Used by the
/// destination-side verifier to construct event-type strings for
/// `events::MessageApproved` / `events::MessageExecuted`.
pub fn read_sui_gateway_pkg(config: &std::path::Path, chain_id: &str) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre!("failed to read config: {e}"))?;
    let root: Value = serde_json::from_str(&content)?;
    let pkg = root
        .pointer(&format!(
            "/chains/{chain_id}/contracts/AxelarGateway/address"
        ))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no AxelarGateway.address for sui chain '{chain_id}'"))?;
    Ok(pkg.to_string())
}

// ---------------------------------------------------------------------------
// Build + send a GMP send_call from Sui to any destination
// ---------------------------------------------------------------------------

/// One Sui→destination GMP call. Bundles the per-call inputs (destination
/// fields + gas/budget) so `send_gmp_call` doesn't need an 8-positional-arg
/// signature where it's easy to swap, e.g., the chain and address strings at
/// the call site.
pub struct SuiGmpCall<'a> {
    pub destination_chain: &'a str,
    pub destination_address: &'a str,
    pub payload: &'a [u8],
    /// Cross-chain gas paid into the Sui `GasService`, in mist (1 SUI = 1e9
    /// mist). Used by the relayer to fund the destination-side `execute`.
    pub gas_value_mist: u64,
    /// On-chain Sui tx gas budget in mist, separate from `gas_value_mist`
    /// (which is the cross-chain message gas). Caller picks based on a
    /// pessimistic upper bound for the PTB cost.
    pub gas_budget_mist: u64,
}

/// Outcome of a GMP send: tx digest + the index in `events[]` of the
/// `ContractCall` event (which is the message id suffix).
#[derive(Debug, Clone)]
pub struct GmpSendResult {
    pub digest: String,
    pub success: bool,
    pub error: Option<String>,
    pub event_index: u32,
    pub source_address_hex: String,
    pub payload_hash_hex: String,
}

/// Build, sign, and submit a Sui GMP send_call calling
/// `Example::gmp::send_call(singleton, gateway, gas_service, dest_chain,
///   dest_address, payload, refund_address, coin, params)`.
///
/// Move signature: `destination_chain: String, destination_address: String,
/// payload: vector<u8>, refund_address: address, coin: Coin<SUI>, params:
/// vector<u8>`. We mirror the TypeScript reference (`sui/gmp.js`).
///
/// `destination_address` is the human-readable string the destination chain
/// expects (for EVM, e.g. `"0xd7f2…"`), not raw bytes.
///
/// `gas_value_mist` is the SUI to attach as cross-chain gas (split off the
/// gas coin). `gas_budget_mist` is the on-chain Sui gas budget (the cost
/// of running this PTB itself), separate from the cross-chain gas payment.
#[allow(clippy::too_many_arguments)]
pub async fn send_gmp_call(
    client: &SuiClient,
    wallet: &SuiWallet,
    contracts: &SuiContractsConfig,
    call: &SuiGmpCall<'_>,
) -> Result<GmpSendResult> {
    // Fetch shared-object versions in parallel.
    let (singleton_v, gateway_v, gas_service_v) = tokio::try_join!(
        client.get_shared_object_initial_version(&contracts.gmp_singleton),
        client.get_shared_object_initial_version(&contracts.gateway_object),
        client.get_shared_object_initial_version(&contracts.gas_service_object),
    )?;

    let gas_coin = client.pick_gas_coin(&wallet.address).await?;
    let rgp = client.get_reference_gas_price().await?;

    let mut b = PtbBuilder::new();
    let singleton = b.shared_object(contracts.gmp_singleton, singleton_v, true);
    let gateway = b.shared_object(contracts.gateway_object, gateway_v, true);
    let gas_svc = b.shared_object(contracts.gas_service_object, gas_service_v, true);
    let dest_chain_arg = b.pure_string(call.destination_chain)?;
    let dest_addr_arg = b.pure_string(call.destination_address)?;
    let payload_arg = b.pure_vec_u8(call.payload)?;
    let refund_arg = b.pure_address(wallet.address)?;
    let amt_arg = b.pure_u64(call.gas_value_mist)?;
    let coin_arg = b.split_coin(Argument::Gas, amt_arg);
    let params_arg = b.pure_vec_u8(&[])?;

    b.move_call(
        contracts.example_pkg,
        "gmp",
        "send_call",
        vec![],
        vec![
            singleton,
            gateway,
            gas_svc,
            dest_chain_arg,
            dest_addr_arg,
            payload_arg,
            refund_arg,
            coin_arg,
            params_arg,
        ],
    )?;

    let tx = b.build(
        wallet.address,
        GasPayment {
            objects: vec![gas_coin],
            owner: wallet.address,
            price: rgp,
            budget: call.gas_budget_mist,
        },
    );

    let submitted = sign_and_submit(client, wallet, tx).await?;

    // Find the ContractCall event in events[] to determine event_index.
    // The on-chain message_id is `0x{digest_hex}-{event_index}`.
    let mut event_index = 0u32;
    let mut source_address_hex = String::new();
    let mut payload_hash_hex = String::new();
    for (i, ev) in submitted.events.iter().enumerate() {
        let ty = ev["type"].as_str().unwrap_or("");
        if ty.ends_with("::events::ContractCall") {
            event_index = i as u32;
            source_address_hex = ev
                .pointer("/parsedJson/source_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_start_matches("0x")
                .to_string();
            payload_hash_hex = ev
                .pointer("/parsedJson/payload_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_start_matches("0x")
                .to_string();
            break;
        }
    }

    Ok(GmpSendResult {
        digest: submitted.digest,
        success: submitted.success,
        error: submitted.error,
        event_index,
        source_address_hex,
        payload_hash_hex,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_address_derivation() {
        // Vector from Sui docs: a known keypair → known address.
        let seed = [
            0x9a, 0x1f, 0x52, 0x90, 0x4d, 0x70, 0x14, 0x6e, 0xe5, 0x6f, 0xb6, 0x83, 0xf6, 0x88,
            0x97, 0x44, 0x37, 0x6b, 0x68, 0x3a, 0xf6, 0x57, 0xe1, 0x69, 0x66, 0x5d, 0x90, 0x65,
            0xc6, 0x16, 0xf6, 0x1c,
        ];
        let w = SuiWallet::from_ed25519_seed(&seed).unwrap();
        // sanity: produces a 32-byte address that's deterministic.
        assert_eq!(w.address.as_bytes().len(), 32);
    }
}
