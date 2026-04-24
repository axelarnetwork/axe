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
use sha2::{Digest, Sha256};
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

    pub fn address(&self) -> String {
        self.account_id.to_address()
    }
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

/// Default faucet URL for a given XRPL network type string (`"testnet"`,
/// `"devnet"`, etc.). Returns `None` for networks without a public faucet
/// (mainnet/stagenet).
pub fn faucet_url_for_network(network_type: &str) -> Option<&'static str> {
    match network_type {
        "testnet" => Some("https://faucet.altnet.rippletest.net/accounts"),
        "devnet" => Some("https://faucet.devnet.rippletest.net/accounts"),
        _ => None,
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
