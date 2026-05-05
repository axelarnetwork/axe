//! Stellar/Soroban client primitives used by the load-test tool.
//!
//! Wraps `stellar-rpc-client` + manual `stellar-xdr` transaction construction
//! so `axe` can sign and submit Soroban `InvokeHostFunction` operations
//! (mainly `AxelarGateway.call_contract` and ITS methods).
//!
//! References:
//! - TypeScript: `axelar-contract-deployments/stellar/gateway.js`, `its.js`
//! - Soroban submission flow: build → `simulate_transaction` → merge footprint
//!   + auth + min_resource_fee → sign → `send_transaction` → poll

#![allow(dead_code)]

use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use eyre::{Result, eyre};
use sha2::{Digest, Sha256};
use stellar_rpc_client::Client as RpcClient;
use stellar_strkey::{Contract as StrContract, ed25519::PublicKey as StrPubKey};
use stellar_xdr::curr::{
    AccountId, BytesM, DecoratedSignature, Hash, HostFunction, InvokeContractArgs,
    InvokeHostFunctionOp, Limits, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
    PublicKey, ScAddress, ScSymbol, ScVal, SequenceNumber, Signature, SignatureHint,
    SorobanAuthorizationEntry, StringM, Transaction, TransactionEnvelope, TransactionExt,
    TransactionSignaturePayload, TransactionSignaturePayloadTaggedTransaction,
    TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

/// Default poll cadence after submit.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Higher base fee for ITS transfers — the simulate step still tops it up
/// with the actual resource fee, but a generous floor avoids `txInsufficientFee`
/// rejections when the network bumps fees mid-test.
const BASE_FEE_ITS: u32 = 1_000;
/// Upper bound for waiting on a single tx to validate. Stellar ledger close
/// time is ~5s; a generous window protects against RPC lag without hiding
/// real failures.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(180);

/// Well-known network passphrases — Stellar does NOT put these in chain
/// config, but they're stable per-network. SHA256(passphrase) becomes the
/// `network_id` used as the signing domain.
pub fn network_passphrase_for(network_type: &str) -> &'static str {
    match network_type {
        "testnet" => "Test SDF Network ; September 2015",
        "futurenet" => "Test SDF Future Network ; October 2022",
        "mainnet" => "Public Global Stellar Network ; September 2015",
        _ => "Test SDF Network ; September 2015",
    }
}

// ---------------------------------------------------------------------------
// Wallet
// ---------------------------------------------------------------------------

/// Stellar wallet: an Ed25519 keypair plus the derived G-address.
#[derive(Clone)]
pub struct StellarWallet {
    pub signing_key: SigningKey,
    pub public_key_bytes: [u8; 32],
}

impl StellarWallet {
    /// Build a wallet from a raw 32-byte Ed25519 seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(seed);
        let public_key_bytes = signing_key.verifying_key().to_bytes();
        Self {
            signing_key,
            public_key_bytes,
        }
    }

    /// Parse a Stellar secret key string (`S...`, base32-encoded) into a
    /// wallet.
    pub fn from_secret_str(secret: &str) -> Result<Self> {
        let sk = stellar_strkey::ed25519::PrivateKey::from_string(secret)
            .map_err(|e| eyre!("invalid Stellar secret key: {e}"))?;
        Ok(Self::from_seed(&sk.0))
    }

    /// Accept a hex-encoded 32-byte seed (convenience for CLIs that share
    /// env vars across chains).
    pub fn from_hex_seed(hex_str: &str) -> Result<Self> {
        let bytes = hex::decode(hex_str.trim_start_matches("0x"))
            .map_err(|e| eyre!("invalid Stellar hex seed: {e}"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| eyre!("Stellar seed must be exactly 32 bytes"))?;
        Ok(Self::from_seed(&bytes))
    }

    /// Stellar G-address (base32-encoded with checksum).
    pub fn address(&self) -> String {
        StrPubKey(self.public_key_bytes).to_string()
    }

    pub fn account_id(&self) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            self.public_key_bytes,
        )))
    }

    pub fn muxed_account(&self) -> MuxedAccount {
        MuxedAccount::Ed25519(Uint256(self.public_key_bytes))
    }

    /// Last 4 bytes of public key — required in each DecoratedSignature.
    pub fn signature_hint(&self) -> SignatureHint {
        let mut hint = [0u8; 4];
        hint.copy_from_slice(&self.public_key_bytes[28..32]);
        SignatureHint(hint)
    }
}

// ---------------------------------------------------------------------------
// ScVal helpers — thin, correct encodings matching the JS reference
// ---------------------------------------------------------------------------

pub fn scval_address_account(pk: &[u8; 32]) -> ScVal {
    ScVal::Address(ScAddress::Account(AccountId(
        PublicKey::PublicKeyTypeEd25519(Uint256(*pk)),
    )))
}

pub fn scval_address_from_str(addr: &str) -> Result<ScVal> {
    // G... = account, C... = contract
    if addr.starts_with('G') {
        let pk = StrPubKey::from_string(addr)
            .map_err(|e| eyre!("invalid Stellar account address {addr:?}: {e}"))?;
        Ok(scval_address_account(&pk.0))
    } else if addr.starts_with('C') {
        let c = StrContract::from_string(addr)
            .map_err(|e| eyre!("invalid Stellar contract address {addr:?}: {e}"))?;
        Ok(ScVal::Address(ScAddress::Contract(
            stellar_xdr::curr::ContractId(Hash(c.0)),
        )))
    } else {
        Err(eyre!("Stellar address must start with G or C: {addr}"))
    }
}

pub fn scval_string(s: &str) -> Result<ScVal> {
    let sm: StringM = s
        .try_into()
        .map_err(|e| eyre!("string too long for ScVal::String: {e}"))?;
    Ok(ScVal::String(stellar_xdr::curr::ScString(sm)))
}

pub fn scval_symbol(s: &str) -> Result<ScSymbol> {
    let sm: StringM<32> = s
        .try_into()
        .map_err(|e| eyre!("symbol too long (max 32): {e}"))?;
    Ok(ScSymbol(sm))
}

pub fn scval_bytes(b: &[u8]) -> Result<ScVal> {
    let v: BytesM = b
        .to_vec()
        .try_into()
        .map_err(|e| eyre!("bytes too long for ScVal::Bytes: {e}"))?;
    Ok(ScVal::Bytes(stellar_xdr::curr::ScBytes(v)))
}

/// ScVal::I128 from a u64 (amounts are always non-negative for our use).
pub fn scval_i128_from_u64(n: u64) -> ScVal {
    ScVal::I128(stellar_xdr::curr::Int128Parts { hi: 0, lo: n })
}

/// Build the `{ address: Address, amount: i128 }` ScVal::Map struct that
/// Soroban contracts expect for a gas-token arg (e.g., `AxelarExample.send`'s
/// `gas_token` parameter). Matches `tokenToScVal` in the TS reference.
pub fn scval_token(token_contract: &str, amount: u64) -> Result<ScVal> {
    let addr_val = scval_address_from_str(token_contract)?;
    let entries: VecM<stellar_xdr::curr::ScMapEntry> = vec![
        stellar_xdr::curr::ScMapEntry {
            key: ScVal::Symbol(scval_symbol("address")?),
            val: addr_val,
        },
        stellar_xdr::curr::ScMapEntry {
            key: ScVal::Symbol(scval_symbol("amount")?),
            val: scval_i128_from_u64(amount),
        },
    ]
    .try_into()
    .map_err(|e| eyre!("token map too long: {e}"))?;
    Ok(ScVal::Map(Some(stellar_xdr::curr::ScMap(entries))))
}

/// Build the `{ decimal: u32, name: String, symbol: String }` ScVal::Map
/// struct that `InterchainTokenService.deploy_interchain_token` expects for
/// its `metadata` parameter. Matches `tokenMetadataToScVal` in the TS
/// reference. Soroban map keys are sorted by symbol-name when serialized;
/// the entries here are already in lexicographic order (decimal < name <
/// symbol).
pub fn scval_token_metadata(decimal: u32, name: &str, symbol: &str) -> Result<ScVal> {
    let entries: VecM<stellar_xdr::curr::ScMapEntry> = vec![
        stellar_xdr::curr::ScMapEntry {
            key: ScVal::Symbol(scval_symbol("decimal")?),
            val: ScVal::U32(decimal),
        },
        stellar_xdr::curr::ScMapEntry {
            key: ScVal::Symbol(scval_symbol("name")?),
            val: scval_string(name)?,
        },
        stellar_xdr::curr::ScMapEntry {
            key: ScVal::Symbol(scval_symbol("symbol")?),
            val: scval_string(symbol)?,
        },
    ]
    .try_into()
    .map_err(|e| eyre!("metadata map too long: {e}"))?;
    Ok(ScVal::Map(Some(stellar_xdr::curr::ScMap(entries))))
}

/// `ScVal::I128` from a non-negative `u128`.
pub fn scval_i128_from_u128(n: u128) -> ScVal {
    ScVal::I128(stellar_xdr::curr::Int128Parts {
        hi: (n >> 64) as i64,
        lo: (n & 0xFFFF_FFFF_FFFF_FFFF) as u64,
    })
}

/// `ScVal::Void`/null literal.
pub fn scval_void() -> ScVal {
    ScVal::Void
}

/// Convert a raw `ContractId` byte array back into the user-facing C-address
/// (base32-encoded with checksum).
pub fn contract_id_to_address(id: &[u8; 32]) -> String {
    StrContract(*id).to_string()
}

/// Decode an `ScVal::Bytes` of exactly 32 bytes into a `[u8; 32]` (e.g.,
/// the `tokenId` returned by `deploy_interchain_token`).
pub fn scval_to_bytes32(v: &ScVal) -> Option<[u8; 32]> {
    if let ScVal::Bytes(b) = v {
        let bytes = b.0.as_slice();
        if bytes.len() == 32 {
            let mut out = [0u8; 32];
            out.copy_from_slice(bytes);
            return Some(out);
        }
    }
    None
}

/// Decode an `ScVal::Address` into its user-facing string form
/// (`G...` for accounts, `C...` for contracts).
pub fn scval_to_address_string(v: &ScVal) -> Option<String> {
    if let ScVal::Address(addr) = v {
        match addr {
            ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(k))) => {
                Some(StrPubKey(k.0).to_string())
            }
            ScAddress::Contract(c) => Some(StrContract(c.0.0).to_string()),
            _ => None,
        }
    } else {
        None
    }
}

/// Decode `ScVal::I128` (assumed non-negative) into `u128`.
pub fn scval_to_u128(v: &ScVal) -> Option<u128> {
    if let ScVal::I128(parts) = v
        && parts.hi >= 0
    {
        return Some(((parts.hi as u128) << 64) | (parts.lo as u128));
    }
    None
}

pub fn parse_contract_id(addr: &str) -> Result<Hash> {
    let c = StrContract::from_string(addr)
        .map_err(|e| eyre!("invalid Stellar contract address {addr:?}: {e}"))?;
    Ok(Hash(c.0))
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct StellarClient {
    pub rpc: RpcClient,
    pub network_passphrase: String,
    pub network_id: [u8; 32],
}

impl StellarClient {
    pub fn new(rpc_url: &str, network_type: &str) -> Result<Self> {
        let rpc = RpcClient::new(rpc_url)
            .map_err(|e| eyre!("failed to build Stellar RPC client: {e}"))?;
        let network_passphrase = network_passphrase_for(network_type).to_string();
        let network_id: [u8; 32] = Sha256::digest(network_passphrase.as_bytes()).into();
        Ok(Self {
            rpc,
            network_passphrase,
            network_id,
        })
    }

    /// Fetch the current account sequence number. Returns `None` if the
    /// account is unfunded / missing.
    pub async fn account_sequence(&self, address: &str) -> Result<Option<i64>> {
        match self.rpc.get_account(address).await {
            Ok(entry) => Ok(Some(entry.seq_num.0)),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not found")
                    || msg.contains("NotFound")
                    || msg.contains("Account not found")
                {
                    Ok(None)
                } else {
                    Err(eyre!("get_account({address}) failed: {msg}"))
                }
            }
        }
    }

    /// Fund a testnet account via Friendbot. On non-testnet networks, this
    /// will error — callers should have a main wallet fund instead.
    pub async fn friendbot_fund(&self, address: &str) -> Result<()> {
        let friendbot = self
            .rpc
            .friendbot_url()
            .await
            .map_err(|e| eyre!("friendbot_url: {e}"))?;
        let url = format!("{friendbot}?addr={address}");
        let resp = reqwest::get(&url)
            .await
            .map_err(|e| eyre!("friendbot GET failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(eyre!(
                "friendbot returned {status}: {}",
                text.chars().take(300).collect::<String>()
            ));
        }
        Ok(())
    }

    /// Build + simulate + sign + submit an InvokeContract call, then poll.
    ///
    /// `event_filter` (if provided) is the contract hash of the emitter we
    /// want to locate in the validated tx's event list — axe uses the
    /// `AxelarGateway` hash so the returned `event_index` matches the
    /// `hex_tx_hash_and_event_index` message-id format expected by the
    /// Stellar `VotingVerifier`.
    #[allow(clippy::too_many_arguments)]
    pub async fn invoke_contract(
        &self,
        wallet: &StellarWallet,
        contract: &str,
        function: &str,
        args: Vec<ScVal>,
        base_fee: u32,
        event_filter: Option<Hash>,
    ) -> Result<InvokedTx> {
        // 1. Read account sequence
        let seq = self
            .account_sequence(&wallet.address())
            .await?
            .ok_or_else(|| {
                eyre!(
                    "Stellar account {} is not activated — fund it via friendbot first",
                    wallet.address()
                )
            })?;

        let contract_id = parse_contract_id(contract)?;
        let fn_symbol = scval_symbol(function)?;
        let args_vec: VecM<ScVal> = args
            .try_into()
            .map_err(|e| eyre!("too many args for InvokeContract: {e}"))?;
        let invoke = InvokeContractArgs {
            contract_address: ScAddress::Contract(stellar_xdr::curr::ContractId(contract_id)),
            function_name: fn_symbol,
            args: args_vec,
        };

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(invoke),
                auth: VecM::default(),
            }),
        };
        let ops: VecM<Operation, 100> =
            vec![op].try_into().map_err(|e| eyre!("tx op build: {e}"))?;

        let mut tx = Transaction {
            source_account: wallet.muxed_account(),
            fee: base_fee,
            seq_num: SequenceNumber(seq + 1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops,
            ext: TransactionExt::V0,
        };

        // 2. Simulate to discover footprint, auth, and resource fee.
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: tx.clone(),
            signatures: VecM::default(),
        });
        let sim = self
            .rpc
            .simulate_transaction_envelope(&envelope, None)
            .await
            .map_err(|e| eyre!("simulate_transaction_envelope: {e}"))?;
        if let Some(err) = &sim.error {
            return Err(eyre!("Stellar simulation failed: {err}"));
        }

        // 3. Merge footprint + auth + fee back into the tx.
        let soroban_data = sim
            .transaction_data()
            .map_err(|e| eyre!("decode transaction_data: {e}"))?;
        tx.ext = TransactionExt::V1(soroban_data);
        tx.fee = base_fee.saturating_add(sim.min_resource_fee.min(u64::from(u32::MAX)) as u32);

        let sim_results = sim
            .results()
            .map_err(|e| eyre!("decode simulate results: {e}"))?;
        if let Some(first) = sim_results.first()
            && !first.auth.is_empty()
        {
            // VecM is read-only at index — rebuild the ops vec with auth merged.
            let mut ops_vec: Vec<Operation> = tx.operations.to_vec();
            if let Some(op) = ops_vec.first_mut()
                && let OperationBody::InvokeHostFunction(ref mut ihf) = op.body
            {
                let auth_vec: VecM<SorobanAuthorizationEntry> = first
                    .auth
                    .clone()
                    .try_into()
                    .map_err(|e| eyre!("auth too long: {e}"))?;
                ihf.auth = auth_vec;
            }
            tx.operations = ops_vec.try_into().map_err(|e| eyre!("rebuild ops: {e}"))?;
        }

        // 4. Sign.
        let signed = self.sign(wallet, tx)?;

        // 5. Submit.
        let hash = self
            .rpc
            .send_transaction(&signed)
            .await
            .map_err(|e| eyre!("send_transaction: {e}"))?;
        let tx_hash_hex = hex::encode(hash.0);

        // 6. Poll for validation.
        let start = Instant::now();
        loop {
            match self.rpc.get_transaction(&hash).await {
                Ok(resp) => {
                    // 23.x uses status string: "SUCCESS"/"FAILED"/"NOT_FOUND"
                    if let Some(status) = extract_status(&resp) {
                        match status.as_str() {
                            "SUCCESS" => {
                                let event_index = event_filter
                                    .as_ref()
                                    .and_then(|filter| find_event_index(&resp, filter));
                                let return_value = extract_return_value(&resp);
                                return Ok(InvokedTx {
                                    tx_hash_hex,
                                    success: true,
                                    event_index,
                                    return_value,
                                });
                            }
                            "FAILED" => {
                                return Ok(InvokedTx {
                                    tx_hash_hex,
                                    success: false,
                                    event_index: None,
                                    return_value: None,
                                });
                            }
                            _ => {} // NOT_FOUND → keep polling
                        }
                    }
                }
                Err(_) => {
                    // Transient; keep polling until timeout.
                }
            }
            if start.elapsed() >= VALIDATE_TIMEOUT {
                return Err(eyre!(
                    "Stellar tx {tx_hash_hex} not validated within {:?}",
                    VALIDATE_TIMEOUT
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    // -----------------------------------------------------------------
    // ITS-specific helpers
    // -----------------------------------------------------------------

    /// `InterchainTokenService.deploy_interchain_token(caller, salt, metadata,
    /// initial_supply, minter)` → returns the new `tokenId` (32 bytes).
    /// Initial supply is minted to `caller`.
    #[allow(clippy::too_many_arguments)]
    pub async fn its_deploy_interchain_token(
        &self,
        wallet: &StellarWallet,
        its_contract: &str,
        salt: [u8; 32],
        decimals: u32,
        name: &str,
        symbol: &str,
        initial_supply: u128,
    ) -> Result<(InvokedTx, Option<[u8; 32]>)> {
        let caller = scval_address_account(&wallet.public_key_bytes);
        let metadata = scval_token_metadata(decimals, name, symbol)?;
        let supply = scval_i128_from_u128(initial_supply);
        let minter = scval_address_account(&wallet.public_key_bytes);
        let salt_arg = scval_bytes(&salt)?;

        let invoked = self
            .invoke_contract(
                wallet,
                its_contract,
                "deploy_interchain_token",
                vec![caller, salt_arg, metadata, supply, minter],
                500, // higher base fee — Soroban deploy is heavy
                None,
            )
            .await?;
        let token_id = invoked.return_value.as_ref().and_then(scval_to_bytes32);
        Ok((invoked, token_id))
    }

    /// `InterchainTokenService.deploy_remote_interchain_token(caller, salt,
    /// destination_chain, gas_token)` — registers the same token on a
    /// destination chain via the ITS hub.
    #[allow(clippy::too_many_arguments)]
    pub async fn its_deploy_remote_interchain_token(
        &self,
        wallet: &StellarWallet,
        its_contract: &str,
        gateway_contract: &str,
        salt: [u8; 32],
        destination_chain: &str,
        gas_token: &str,
        gas_amount: u64,
    ) -> Result<InvokedTx> {
        let caller = scval_address_account(&wallet.public_key_bytes);
        let dest_chain_v = scval_string(destination_chain)?;
        let gas_v = scval_token(gas_token, gas_amount)?;
        let salt_arg = scval_bytes(&salt)?;

        let gw_filter = parse_contract_id(gateway_contract).ok();
        self.invoke_contract(
            wallet,
            its_contract,
            "deploy_remote_interchain_token",
            vec![caller, salt_arg, dest_chain_v, gas_v],
            500,
            gw_filter,
        )
        .await
    }

    /// `InterchainTokenService.interchain_transfer(caller, token_id,
    /// destination_chain, destination_address, amount, data, gas_token)`.
    /// `data` is `None` for plain transfers.
    #[allow(clippy::too_many_arguments)]
    pub async fn its_interchain_transfer(
        &self,
        wallet: &StellarWallet,
        its_contract: &str,
        gateway_contract: &str,
        token_id: [u8; 32],
        destination_chain: &str,
        destination_address_bytes: &[u8],
        amount: u128,
        data: Option<&[u8]>,
        gas_token: &str,
        gas_amount: u64,
    ) -> Result<InvokedTx> {
        let caller = scval_address_account(&wallet.public_key_bytes);
        let token_id_v = scval_bytes(&token_id)?;
        let dest_chain_v = scval_string(destination_chain)?;
        let dest_addr_v = scval_bytes(destination_address_bytes)?;
        let amount_v = scval_i128_from_u128(amount);
        let data_v = match data {
            Some(d) => scval_bytes(d)?,
            None => scval_void(),
        };
        let gas_v = scval_token(gas_token, gas_amount)?;

        let gw_filter = parse_contract_id(gateway_contract).ok();
        self.invoke_contract(
            wallet,
            its_contract,
            "interchain_transfer",
            vec![
                caller,
                token_id_v,
                dest_chain_v,
                dest_addr_v,
                amount_v,
                data_v,
                gas_v,
            ],
            BASE_FEE_ITS,
            gw_filter,
        )
        .await
    }

    /// Query `InterchainTokenService.interchain_token_address(token_id)` —
    /// returns the Stellar SAC contract address that owns the token's
    /// balances on this chain.
    pub async fn its_query_token_address(
        &self,
        wallet: &StellarWallet,
        its_contract: &str,
        token_id: [u8; 32],
    ) -> Result<Option<String>> {
        let token_id_v = scval_bytes(&token_id)?;
        let invoked = self
            .invoke_contract(
                wallet,
                its_contract,
                "interchain_token_address",
                vec![token_id_v],
                100,
                None,
            )
            .await?;
        Ok(invoked
            .return_value
            .as_ref()
            .and_then(scval_to_address_string))
    }

    /// Standard SAC `transfer(from, to, amount)`. Used to distribute the AXE
    /// load-test token from the deployer to ephemeral wallets.
    pub async fn token_transfer(
        &self,
        wallet: &StellarWallet,
        token_contract: &str,
        to_account_pk: &[u8; 32],
        amount: u128,
    ) -> Result<InvokedTx> {
        let from = scval_address_account(&wallet.public_key_bytes);
        let to = scval_address_account(to_account_pk);
        let amount_v = scval_i128_from_u128(amount);
        self.invoke_contract(
            wallet,
            token_contract,
            "transfer",
            vec![from, to, amount_v],
            200,
            None,
        )
        .await
    }

    /// Query `balance(account)` on a SAC token contract.
    pub async fn token_balance(
        &self,
        wallet: &StellarWallet,
        token_contract: &str,
        account_pk: &[u8; 32],
    ) -> Result<u128> {
        let account_v = scval_address_account(account_pk);
        let invoked = self
            .invoke_contract(
                wallet,
                token_contract,
                "balance",
                vec![account_v],
                100,
                None,
            )
            .await?;
        Ok(invoked
            .return_value
            .as_ref()
            .and_then(scval_to_u128)
            .unwrap_or(0))
    }

    // -----------------------------------------------------------------
    // Read-only contract views (simulate-only, no tx submission)
    // -----------------------------------------------------------------

    /// Simulate an `InvokeContract` host function and return the contract's
    /// return ScVal — without submitting a tx. Used for view methods like
    /// `is_message_approved`, `interchain_token_address`, `balance`, etc.
    /// The `signer_account_pk` is just the source account for the simulation
    /// envelope — no authorization needed for read-only calls.
    pub async fn simulate_view(
        &self,
        signer_account_pk: &[u8; 32],
        contract: &str,
        function: &str,
        args: Vec<ScVal>,
    ) -> Result<Option<ScVal>> {
        let contract_id = parse_contract_id(contract)?;
        let fn_symbol = scval_symbol(function)?;
        let args_vec: VecM<ScVal> = args
            .try_into()
            .map_err(|e| eyre!("too many args for InvokeContract: {e}"))?;
        let invoke = InvokeContractArgs {
            contract_address: ScAddress::Contract(stellar_xdr::curr::ContractId(contract_id)),
            function_name: fn_symbol,
            args: args_vec,
        };
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(invoke),
                auth: VecM::default(),
            }),
        };
        let ops: VecM<Operation, 100> = vec![op].try_into().map_err(|e| eyre!("op vec: {e}"))?;
        let source_account =
            AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*signer_account_pk)));
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(*signer_account_pk)),
            fee: 100,
            // simulate doesn't need a real seq number; use 1 as a placeholder.
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops,
            ext: TransactionExt::V0,
        };
        let _ = source_account;
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });
        let sim = self
            .rpc
            .simulate_transaction_envelope(&envelope, None)
            .await
            .map_err(|e| eyre!("simulate_transaction_envelope: {e}"))?;
        if let Some(err) = sim.error {
            return Err(eyre!("Stellar simulate failed: {err}"));
        }
        let results = sim
            .results()
            .map_err(|e| eyre!("decode simulate results: {e}"))?;
        Ok(results.into_iter().next().map(|r| r.xdr))
    }

    /// `AxelarGateway.is_message_approved(source_chain, message_id,
    /// source_address, contract_address, payload_hash) → bool`. Read-only.
    /// Returns `Some(true)` if approved, `Some(false)` if not (or already
    /// executed), `None` if the simulation could not be parsed.
    #[allow(clippy::too_many_arguments)]
    pub async fn gateway_is_message_approved(
        &self,
        signer_account_pk: &[u8; 32],
        gateway_contract: &str,
        source_chain: &str,
        message_id: &str,
        source_address: &str,
        contract_address: &str,
        payload_hash: [u8; 32],
    ) -> Result<Option<bool>> {
        let args = vec![
            scval_string(source_chain)?,
            scval_string(message_id)?,
            scval_string(source_address)?,
            scval_address_from_str(contract_address)?,
            // BytesN<32>
            ScVal::Bytes(stellar_xdr::curr::ScBytes(
                payload_hash
                    .to_vec()
                    .try_into()
                    .map_err(|e| eyre!("payload hash: {e}"))?,
            )),
        ];
        let ret = self
            .simulate_view(
                signer_account_pk,
                gateway_contract,
                "is_message_approved",
                args,
            )
            .await?;
        Ok(ret.and_then(|v| match v {
            ScVal::Bool(b) => Some(b),
            _ => None,
        }))
    }

    /// `AxelarGateway.is_message_executed(source_chain, message_id) → bool`.
    pub async fn gateway_is_message_executed(
        &self,
        signer_account_pk: &[u8; 32],
        gateway_contract: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<Option<bool>> {
        let args = vec![scval_string(source_chain)?, scval_string(message_id)?];
        let ret = self
            .simulate_view(
                signer_account_pk,
                gateway_contract,
                "is_message_executed",
                args,
            )
            .await?;
        Ok(ret.and_then(|v| match v {
            ScVal::Bool(b) => Some(b),
            _ => None,
        }))
    }

    /// Sign a pre-simulated `Transaction` and wrap it in a `TransactionEnvelope`.
    pub fn sign(&self, wallet: &StellarWallet, tx: Transaction) -> Result<TransactionEnvelope> {
        let payload = TransactionSignaturePayload {
            network_id: Hash(self.network_id),
            tagged_transaction: TransactionSignaturePayloadTaggedTransaction::Tx(tx.clone()),
        };
        let payload_bytes = payload
            .to_xdr(Limits::none())
            .map_err(|e| eyre!("encode signature payload: {e}"))?;
        let hash: [u8; 32] = Sha256::digest(&payload_bytes).into();
        let sig = wallet.signing_key.sign(&hash);

        let sig_bytes: BytesM<64> = sig
            .to_bytes()
            .to_vec()
            .try_into()
            .map_err(|e| eyre!("signature encoding: {e}"))?;
        let decorated = DecoratedSignature {
            hint: wallet.signature_hint(),
            signature: Signature(sig_bytes),
        };
        let sigs: VecM<DecoratedSignature, 20> = vec![decorated]
            .try_into()
            .map_err(|e| eyre!("signatures: {e}"))?;

        Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: sigs,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct InvokedTx {
    pub tx_hash_hex: String,
    pub success: bool,
    /// Flat index (across all ops) of the ContractEvent emitted by the filter
    /// contract. `None` if no filter provided or no matching event.
    pub event_index: Option<u32>,
    /// The Soroban contract's return value, if any. Populated for SUCCESS
    /// responses on `TransactionMeta::V3` (testnet) and `V4`. Used to read
    /// out e.g. the token_id returned by `deploy_interchain_token`.
    pub return_value: Option<ScVal>,
}

// `GetTransactionResponse.status` is a plain `String` ("SUCCESS" / "FAILED"
// / "NOT_FOUND") in stellar-rpc-client 23.x. Keep normalized (uppercase,
// trimmed) so minor format changes don't break the caller.
fn extract_status(resp: &stellar_rpc_client::GetTransactionResponse) -> Option<String> {
    Some(resp.status.trim().to_uppercase())
}

/// Extract the Soroban contract's return value from a validated tx, if any.
/// The Soroban return value lives in `result_meta.soroban_meta.return_value`
/// (V3) or `result_meta.soroban_meta.return_value` (V4 — same location).
fn extract_return_value(resp: &stellar_rpc_client::GetTransactionResponse) -> Option<ScVal> {
    use stellar_xdr::curr::TransactionMeta;
    let meta = resp.result_meta.as_ref()?;
    match meta {
        TransactionMeta::V3(m) => m.soroban_meta.as_ref().map(|s| s.return_value.clone()),
        // V4's SorobanTransactionMetaV2 wraps return_value in Option<ScVal>.
        TransactionMeta::V4(m) => m.soroban_meta.as_ref().and_then(|s| s.return_value.clone()),
        _ => None,
    }
}

/// Find the flat index of the first `ContractEvent` whose `contract_id`
/// matches `target` in a validated tx's events. The Amplifier Stellar
/// VotingVerifier uses this same flat index in its `hex_tx_hash_and_event_index`
/// message-id format.
///
/// On testnet/mainnet today the RPC returns `TransactionMeta::V3`, whose
/// contract events live under `soroban_meta.events`. `stellar-rpc-client`
/// 23.x only flattens those into `GetTransactionEvents::contract_events`
/// for V4 metas, so we walk the raw `result_meta` ourselves.
fn find_event_index(
    resp: &stellar_rpc_client::GetTransactionResponse,
    target: &Hash,
) -> Option<u32> {
    use stellar_xdr::curr::{ContractId, TransactionMeta};
    let target_cid = ContractId(target.clone());
    let meta = resp.result_meta.as_ref()?;

    // Collect the flat list of contract events for this tx. Layout differs
    // by meta version: V3 uses `soroban_meta.events`; V4 attaches them to
    // each per-op `events` vec as `TransactionEvent`s wrapping ContractEvent.
    let events: Vec<&stellar_xdr::curr::ContractEvent> = match meta {
        TransactionMeta::V3(m) => m
            .soroban_meta
            .as_ref()
            .map(|s| s.events.iter().collect())
            .unwrap_or_default(),
        TransactionMeta::V4(m) => m
            .operations
            .iter()
            .flat_map(|op| op.events.iter())
            .collect(),
        _ => return None,
    };

    for (i, ev) in events.iter().enumerate() {
        if ev.contract_id.as_ref() == Some(&target_cid) {
            return Some(i as u32);
        }
    }
    None
}
