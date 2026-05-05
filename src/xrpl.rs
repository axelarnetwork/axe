//! XRPL client primitives used by the load-test tool.
//!
//! Thin wrapper over `xrpl_http_client::Client` + `xrpl_binary_codec` for
//! building, signing, submitting and polling XRPL `Payment` transactions that
//! carry Axelar ITS memos.

// Several helpers here are used only by the forthcoming EVM → XRPL
// destination verifier; keep them reachable but silence dead-code lints
// until the second direction lands.
#![allow(dead_code)]

use std::time::Duration;

use eyre::{Result, eyre};
use libsecp256k1::{PublicKey, SecretKey};
use ripemd::Ripemd160;
use sha2::{Digest, Sha256, Sha512};
use xrpl_api::{AccountInfoRequest, SubmitRequest, TxRequest};
use xrpl_binary_codec::{serialize, sign::sign_transaction};
use xrpl_types::{AccountId, Amount, Blob, DropsAmount, Memo, PaymentTransaction};

/// Poll interval while waiting for a submitted tx to be validated on the
/// ledger. XRPL closes ledgers ~every 3–4s, so 2s is a reasonable cadence.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Upper bound on how long we wait for a single tx to validate.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Wallet (secp256k1 keypair + derived r-address)
// ---------------------------------------------------------------------------

/// An XRPL wallet derived from a 32-byte secp256k1 secret seed.
///
/// The XRPL address is computed from the compressed public key via
/// `RIPEMD160(SHA256(pubkey))` and base58check-encoded with the `r` version
/// byte (`0x00`).
#[derive(Clone)]
pub struct XrplWallet {
    pub secret_key: SecretKey,
    pub public_key: PublicKey,
    pub account_id: AccountId,
}

impl XrplWallet {
    pub fn from_bytes(secret_bytes: &[u8; 32]) -> Result<Self> {
        let secret_key = SecretKey::parse(secret_bytes)
            .map_err(|e| eyre!("invalid XRPL secret key bytes: {e:?}"))?;
        let public_key = PublicKey::from_secret_key(&secret_key);
        let account_id = account_id_from_public_key(&public_key);
        Ok(Self {
            secret_key,
            public_key,
            account_id,
        })
    }

    pub fn from_hex(hex_str: &str) -> Result<Self> {
        let bytes = hex::decode(hex_str.trim_start_matches("0x"))
            .map_err(|e| eyre!("invalid XRPL secret key hex: {e}"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| eyre!("XRPL secret key must be exactly 32 bytes"))?;
        Self::from_bytes(&bytes)
    }

    /// Parse an XRPL family seed (s-prefix base58check, e.g. `snr...`) and
    /// derive the secp256k1 master keypair per the XRPL `signing` spec —
    /// root_key + intermediate_key (account_index = 0), summed mod n.
    pub fn from_family_seed(seed_str: &str) -> Result<Self> {
        let seed16 = decode_xrpl_family_seed(seed_str)?;
        let master_priv = derive_secp256k1_master(&seed16)?;
        Self::from_bytes(&master_priv)
    }

    /// Auto-detect the input format: 64/66-char hex, or XRPL family seed.
    pub fn from_secret_str(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        let stripped = trimmed.trim_start_matches("0x");
        if stripped.len() == 64 && stripped.chars().all(|c| c.is_ascii_hexdigit()) {
            Self::from_hex(trimmed)
        } else if trimmed.starts_with('s') {
            Self::from_family_seed(trimmed)
        } else {
            Err(eyre!(
                "unrecognized XRPL secret format (expected 32-byte hex or s-prefix family seed)"
            ))
        }
    }

    pub fn address(&self) -> String {
        self.account_id.to_address()
    }
}

/// XRPL base58 alphabet (note: differs from Bitcoin's).
const XRPL_B58_ALPHA: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";

fn b58_decode_xrpl(s: &str) -> Result<Vec<u8>> {
    let mut bytes: Vec<u8> = vec![0u8];
    for c in s.chars() {
        let idx = XRPL_B58_ALPHA
            .iter()
            .position(|&a| a == c as u8)
            .ok_or_else(|| eyre!("invalid base58 char in XRPL seed: {c}"))?;
        let mut carry = idx as u32;
        for b in bytes.iter_mut() {
            carry += (*b as u32) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    // Leading 'r' chars (alphabet[0]) → leading zero bytes.
    for c in s.chars() {
        if c as u8 == XRPL_B58_ALPHA[0] {
            bytes.push(0);
        } else {
            break;
        }
    }
    bytes.reverse();
    Ok(bytes)
}

/// Decode the 16-byte payload from an XRPL family seed string.
fn decode_xrpl_family_seed(seed: &str) -> Result<[u8; 16]> {
    let raw = b58_decode_xrpl(seed)?;
    if raw.len() != 21 {
        return Err(eyre!(
            "XRPL family seed has wrong length: got {} bytes, expected 21 (1 prefix + 16 payload + 4 checksum)",
            raw.len()
        ));
    }
    if raw[0] != 0x21 {
        return Err(eyre!(
            "XRPL family seed has wrong version byte: got 0x{:02x}, expected 0x21 (sec256k1 seed)",
            raw[0]
        ));
    }
    let payload = &raw[..17];
    let checksum = &raw[17..21];
    let expected = &Sha256::digest(Sha256::digest(payload))[..4];
    if checksum != expected {
        return Err(eyre!("XRPL family seed checksum mismatch"));
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&raw[1..17]);
    Ok(out)
}

/// Derive the secp256k1 master private key from a 16-byte family seed,
/// matching `rippled`'s standard derivation:
///   1. root_priv = first SHA512_half(seed || seq) that is in (0, n).
///   2. intermediate = first SHA512_half(root_pub_compressed || 0u32_be || seq) in (0, n).
///   3. master_priv = (root_priv + intermediate) mod n.
fn derive_secp256k1_master(seed16: &[u8; 16]) -> Result<[u8; 32]> {
    let root = derive_part_secp256k1(seed16)?;
    let root_sk = SecretKey::parse(&root).map_err(|e| eyre!("root key invalid: {e:?}"))?;
    let root_pk = PublicKey::from_secret_key(&root_sk);
    let pk_compressed = root_pk.serialize_compressed();
    let mut payload = Vec::with_capacity(33 + 4);
    payload.extend_from_slice(&pk_compressed);
    payload.extend_from_slice(&0u32.to_be_bytes());
    let intermediate = derive_part_secp256k1(&payload)?;

    // master = (root + intermediate) mod n
    let mut sum_sk = SecretKey::parse(&root).map_err(|e| eyre!("root key invalid: {e:?}"))?;
    let inter_sk =
        SecretKey::parse(&intermediate).map_err(|e| eyre!("intermediate key invalid: {e:?}"))?;
    sum_sk
        .tweak_add_assign(&inter_sk)
        .map_err(|e| eyre!("master key tweak failed: {e:?}"))?;
    Ok(sum_sk.serialize())
}

fn derive_part_secp256k1(prefix: &[u8]) -> Result<[u8; 32]> {
    for seq in 0u32..=u32::MAX {
        let mut h = Sha512::new();
        h.update(prefix);
        h.update(seq.to_be_bytes());
        let half = &h.finalize()[..32];
        let mut candidate = [0u8; 32];
        candidate.copy_from_slice(half);
        if SecretKey::parse(&candidate).is_ok() {
            return Ok(candidate);
        }
    }
    Err(eyre!("exhausted u32 search deriving XRPL secp256k1 key"))
}

/// Derive an XRPL AccountId from a secp256k1 compressed public key.
fn account_id_from_public_key(pk: &PublicKey) -> AccountId {
    let compressed = pk.serialize_compressed();
    let sha = Sha256::digest(compressed);
    let ripe = Ripemd160::digest(sha);
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(&ripe);
    AccountId(bytes)
}

// ---------------------------------------------------------------------------
// Memo helpers
// ---------------------------------------------------------------------------

/// Build an XRPL `Memo` from a UTF-8 key and arbitrary bytes value.
fn memo(key: &str, value: impl AsRef<[u8]>) -> Memo {
    Memo {
        memo_type: Blob(key.as_bytes().to_vec()),
        memo_data: Blob(value.as_ref().to_vec()),
        memo_format: None,
    }
}

/// Build the 4 Axelar ITS `interchain_transfer` memos for a native-XRP payment
/// to the Axelar multisig.
///
/// * `destination_chain` — e.g. `"xrpl-evm"`
/// * `destination_address_hex` — hex-encoded destination bytes, WITHOUT
///   the leading `0x` (this matches the off-chain TypeScript reference
///   implementation in `axelar-contract-deployments/xrpl/interchain-transfer.js`)
/// * `gas_fee_drops` — gas fee, in the same units as the payment `Amount`
///   (drops for XRP), encoded as a decimal string
pub fn build_its_transfer_memos(
    destination_chain: &str,
    destination_address_hex: &str,
    gas_fee_drops: u64,
    payload: Option<&[u8]>,
) -> Vec<Memo> {
    let mut memos = vec![
        memo("type", b"interchain_transfer"),
        memo("destination_address", destination_address_hex.as_bytes()),
        memo("destination_chain", destination_chain.as_bytes()),
        memo("gas_fee_amount", gas_fee_drops.to_string().as_bytes()),
    ];
    if let Some(p) = payload {
        memos.push(memo("payload", p));
    }
    memos
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct XrplClient {
    inner: xrpl_http_client::Client,
    rpc_url: String,
}

impl XrplClient {
    pub fn new(rpc_url: &str) -> Self {
        let inner = xrpl_http_client::Client::builder()
            .base_url(rpc_url)
            .build();
        Self {
            inner,
            rpc_url: rpc_url.to_string(),
        }
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    pub fn inner(&self) -> &xrpl_http_client::Client {
        &self.inner
    }

    /// Fetch the current balance (drops) and next sequence for an account.
    /// Returns `None` if the account does not exist (unactivated).
    pub async fn account_info(&self, address: &str) -> Result<Option<AccountInfo>> {
        let req = AccountInfoRequest::new(address);
        match self.inner.call(req).await {
            Ok(resp) => Ok(Some(AccountInfo {
                balance_drops: resp
                    .account_data
                    .balance
                    .as_deref()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0),
                sequence: resp.account_data.sequence,
            })),
            Err(e) => {
                // `actNotFound` → account doesn't exist yet
                let msg = e.to_string();
                if msg.contains("actNotFound") || msg.contains("Account not found") {
                    Ok(None)
                } else {
                    Err(eyre!("account_info({address}) failed: {msg}"))
                }
            }
        }
    }

    /// Fund an XRPL account via the public testnet/devnet faucet.
    ///
    /// * `faucet_url` — e.g. `https://faucet.altnet.rippletest.net/accounts`
    pub async fn fund_from_faucet(&self, address: &str, faucet_url: &str) -> Result<()> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({ "destination": address });
        let resp = client
            .post(faucet_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("faucet request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(eyre!(
                "faucet returned {status}: {}",
                text.chars().take(200).collect::<String>()
            ));
        }
        Ok(())
    }

    /// Build, sign, and submit an ITS interchain_transfer `Payment` from
    /// `wallet` to the Axelar multisig at `destination_multisig`.
    ///
    /// `total_drops` must include the gas fee (which is deducted by the
    /// relayer from the total).
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_its_interchain_transfer(
        &self,
        wallet: &XrplWallet,
        destination_multisig: &AccountId,
        total_drops: u64,
        destination_chain: &str,
        destination_address_hex: &str,
        gas_fee_drops: u64,
        payload: Option<&[u8]>,
    ) -> Result<String> {
        let amount = Amount::drops(total_drops)
            .map_err(|e| eyre!("invalid Amount::drops({total_drops}): {e}"))?;

        let mut tx = PaymentTransaction::new(wallet.account_id, amount, *destination_multisig);
        tx.common.memos = build_its_transfer_memos(
            destination_chain,
            destination_address_hex,
            gas_fee_drops,
            payload,
        );

        // Auto-fill sequence, fee, last_ledger_sequence
        self.inner
            .prepare_transaction(&mut tx.common)
            .await
            .map_err(|e| eyre!("prepare_transaction failed: {e}"))?;

        // Sign with secp256k1
        sign_transaction(&mut tx, &wallet.public_key, &wallet.secret_key)
            .map_err(|e| eyre!("sign_transaction failed: {e:?}"))?;

        let tx_bytes = serialize::serialize(&tx).map_err(|e| eyre!("serialize failed: {e:?}"))?;
        let tx_blob = hex::encode_upper(&tx_bytes);
        let tx_hash = signed_tx_hash_hex(&tx_bytes);

        let req = SubmitRequest::new(tx_blob).fail_hard(true);
        let resp = self
            .inner
            .call(req)
            .await
            .map_err(|e| eyre!("submit failed: {e}"))?;

        // `tesSUCCESS` means accepted into the mempool (not yet validated).
        let engine = format!("{:?}", resp.engine_result);
        if !engine.contains("tesSUCCESS") {
            return Err(eyre!(
                "submit rejected: {engine}: {}",
                resp.engine_result_message
            ));
        }

        Ok(tx_hash)
    }

    /// Send a simple XRP Payment with no memos. Used by the funding code to
    /// activate ephemeral load-test wallets from the main wallet.
    pub async fn submit_plain_payment(
        &self,
        wallet: &XrplWallet,
        destination: &AccountId,
        amount_drops: u64,
    ) -> Result<String> {
        let amount = Amount::drops(amount_drops)
            .map_err(|e| eyre!("invalid Amount::drops({amount_drops}): {e}"))?;
        let mut tx = PaymentTransaction::new(wallet.account_id, amount, *destination);

        self.inner
            .prepare_transaction(&mut tx.common)
            .await
            .map_err(|e| eyre!("prepare_transaction failed: {e}"))?;
        if let Some(lls) = tx.common.last_ledger_sequence {
            tx.common.last_ledger_sequence = Some(lls.saturating_add(26));
        }
        sign_transaction(&mut tx, &wallet.public_key, &wallet.secret_key)
            .map_err(|e| eyre!("sign_transaction failed: {e:?}"))?;

        let tx_bytes = serialize::serialize(&tx).map_err(|e| eyre!("serialize failed: {e:?}"))?;
        let tx_blob = hex::encode_upper(&tx_bytes);
        let tx_hash = signed_tx_hash_hex(&tx_bytes);

        let req = SubmitRequest::new(tx_blob).fail_hard(true);
        let resp = self
            .inner
            .call(req)
            .await
            .map_err(|e| eyre!("submit failed: {e}"))?;
        let engine = format!("{:?}", resp.engine_result);
        if !engine.contains("tesSUCCESS") {
            return Err(eyre!(
                "submit rejected: {engine}: {}",
                resp.engine_result_message
            ));
        }
        Ok(tx_hash)
    }

    /// Search the recipient account's recent transactions for an incoming
    /// `Payment` carrying a `message_id` memo equal to `target_message_id`
    /// (decoded from hex-encoded UTF-8). Returns the matching tx hash if
    /// found. The XRPL relayer attaches `message_id` and `source_chain`
    /// memos when broadcasting proof-driven payouts to recipients.
    ///
    /// `min_ledger` lets the caller bound the lookback window; pass `None`
    /// to scan the latest 200 txs only.
    pub async fn find_inbound_with_message_id(
        &self,
        recipient: &str,
        target_message_id: &str,
        min_ledger: Option<u32>,
    ) -> Result<Option<String>> {
        let target_lower = target_message_id.trim_start_matches("0x").to_lowercase();

        // Mainnet rippled (e.g. s1.ripple.com:51234) rejects
        // `ledger_index_min/max="-1"` with `invalidParams`. Only set those
        // params when actually constraining the lookback window; otherwise
        // omit them and let the server return the latest validated txs.
        let req = xrpl_api::AccountTxRequest {
            account: recipient.to_string(),
            forward: Some(false),
            ledger_index_min: min_ledger.map(|n| n.to_string()),
            pagination: xrpl_api::RequestPagination {
                limit: Some(200),
                ..Default::default()
            },
            ..Default::default()
        };
        let resp = self
            .inner
            .call(req)
            .await
            .map_err(|e| eyre!("account_tx({recipient}): {e}"))?;

        for at in resp.transactions {
            if !at.validated {
                continue;
            }
            // We only care about Payments (the relayer broadcasts Payments).
            let common = at.tx.common();
            let memos = match &common.memos {
                Some(m) => m,
                None => continue,
            };
            for m in memos {
                let memo_type_decoded = m
                    .memo_type
                    .as_deref()
                    .and_then(|h| hex::decode(h).ok())
                    .and_then(|b| String::from_utf8(b).ok());
                if memo_type_decoded.as_deref() != Some("message_id") {
                    continue;
                }
                let memo_data_decoded = m
                    .memo_data
                    .as_deref()
                    .and_then(|h| hex::decode(h).ok())
                    .and_then(|b| String::from_utf8(b).ok());
                if let Some(decoded) = memo_data_decoded
                    && decoded.trim_start_matches("0x").to_lowercase() == target_lower
                    && let Some(hash) = common.hash.clone()
                {
                    return Ok(Some(hash));
                }
            }
        }
        Ok(None)
    }

    /// Poll `tx` until the ledger validates the transaction (or we time out).
    pub async fn wait_for_validated(&self, tx_hash: &str) -> Result<ValidatedTx> {
        let start = std::time::Instant::now();
        loop {
            match self.get_validated_tx(tx_hash).await? {
                Some(v) => return Ok(v),
                None => {
                    if start.elapsed() >= VALIDATE_TIMEOUT {
                        return Err(eyre!(
                            "tx {tx_hash} not validated within {:?}",
                            VALIDATE_TIMEOUT
                        ));
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// One-shot check: if the tx has been validated, return its result;
    /// otherwise return `None`. Non-`tesSUCCESS` validated txs still return
    /// `Some` with `success=false` so the caller can decide what to do.
    pub async fn get_validated_tx(&self, tx_hash: &str) -> Result<Option<ValidatedTx>> {
        let req = TxRequest::new(tx_hash);
        match self.inner.call(req).await {
            Ok(resp) => {
                let common = resp.tx.common();
                if common.validated != Some(true) {
                    return Ok(None);
                }
                let success = common
                    .meta
                    .as_ref()
                    .map(|m| m.transaction_result == xrpl_api::TransactionResult::tesSUCCESS)
                    .unwrap_or(false);
                Ok(Some(ValidatedTx {
                    ledger_index: common.ledger_index,
                    success,
                }))
            }
            Err(e) => {
                let msg = e.to_string();
                // `txnNotFound` means the tx is not yet on a validated ledger
                // (or has been dropped). Treat as "not yet".
                if msg.contains("txnNotFound") {
                    Ok(None)
                } else {
                    Err(eyre!("tx({tx_hash}) failed: {msg}"))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AccountInfo {
    pub balance_drops: u64,
    pub sequence: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ValidatedTx {
    pub ledger_index: Option<u32>,
    pub success: bool,
}

// ---------------------------------------------------------------------------
// Convenience helpers
// ---------------------------------------------------------------------------

/// Parse an r-address string into an `AccountId`.
pub fn parse_address(addr: &str) -> Result<AccountId> {
    AccountId::from_address(addr).map_err(|e| eyre!("invalid XRPL address {addr:?}: {e}"))
}

/// Encode an `AccountId`'s 20-byte payload as lowercase hex (no 0x prefix).
/// Used when building the `destination_address` memo for inbound transfers
/// where the destination is an XRPL account.
pub fn account_id_to_hex(id: &AccountId) -> String {
    hex::encode(id.0)
}

/// Convenience conversion: 1 XRP = 1_000_000 drops.
pub const fn xrp_to_drops(xrp: u64) -> u64 {
    xrp.saturating_mul(1_000_000)
}

/// Default faucet URL for a given XRPL chain. We look at the configured
/// RPC/WSS URL because the chain config's `networkType` is unreliable on
/// devnet-amplifier (it labels the chain "testnet" even though the multisig
/// lives on XRPL devnet — a separate ledger). Returns `None` for mainnet.
pub fn faucet_url_for_network(network_type_or_rpc: &str) -> Option<&'static str> {
    let lower = network_type_or_rpc.to_lowercase();
    if lower.contains("devnet") {
        Some("https://faucet.devnet.rippletest.net/accounts")
    } else if lower.contains("altnet") || lower == "testnet" || lower == "stagenet" {
        Some("https://faucet.altnet.rippletest.net/accounts")
    } else {
        None
    }
}

/// `DropsAmount` convenience — wraps validation.
#[allow(dead_code)]
pub fn drops(d: u64) -> Result<DropsAmount> {
    DropsAmount::from_drops(d).map_err(|e| eyre!("invalid drops amount {d}: {e}"))
}

/// Compute the deterministic hash of a signed XRPL transaction blob, returning
/// it as 64 uppercase hex characters (the canonical XRPL tx hash format).
fn signed_tx_hash_hex(tx_bytes: &[u8]) -> String {
    let h = xrpl_binary_codec::hash::hash(
        xrpl_binary_codec::hash::HASH_PREFIX_SIGNED_TRANSACTION,
        tx_bytes,
    );
    h.to_hex()
}
