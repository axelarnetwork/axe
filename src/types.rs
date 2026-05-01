//! Foundation types that wrap stringly- or numerically-typed boundaries with
//! enums and newtypes so the compiler catches misuse the runtime currently
//! catches (or doesn't).
//!
//! This module is the single source of truth for cross-cutting domain types:
//! chain types, networks, hub identity, and the various 32-byte values that
//! cross the Solana/EVM divide. It intentionally has no business logic — it
//! exists so the rest of the codebase can talk in nouns instead of strings.
//!
//! Each type's `Display`/`Serialize` impl produces the exact string the
//! codebase emitted before this module existed (so JSON bodies, RPC URLs,
//! cache filenames, and CLI output bytes are all unchanged).

use std::fmt;
use std::str::FromStr;

use eyre::{Result, eyre};

// ---------------------------------------------------------------------------
// ChainType — the chain's runtime model.
// ---------------------------------------------------------------------------

/// One of the runtime models a chain can target. The set is closed because
/// it mirrors the values the `chains-config` JSON emits in `chainType`.
///
/// The `Display`/`FromStr` round-trip is `"evm"` / `"svm"`, byte-for-byte
/// matching the on-disk config. Don't add variants here without also
/// teaching the Cosmos amplifier deployments side what the new value means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(try_from = "&str")]
pub enum ChainType {
    Evm,
    Svm,
}

impl ChainType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Evm => "evm",
            Self::Svm => "svm",
        }
    }
}

impl fmt::Display for ChainType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ChainType {
    type Err = eyre::Report;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "evm" => Ok(Self::Evm),
            "svm" => Ok(Self::Svm),
            other => Err(eyre!(
                "unknown chainType '{other}' (expected 'evm' or 'svm')"
            )),
        }
    }
}

impl TryFrom<&str> for ChainType {
    type Error = eyre::Report;
    fn try_from(s: &str) -> Result<Self> {
        s.parse()
    }
}

// ---------------------------------------------------------------------------
// Network — the Axelar deployment a binary or config targets.
// ---------------------------------------------------------------------------

/// One of the four Axelar networks the deployments repo defines a config for.
/// Round-trips through `FromStr`/`Display` as the lowercase string the cargo
/// feature flags and config filenames already use (note: `devnet-amplifier`
/// has the dash, not an underscore). Serde uses the same string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Network {
    Mainnet,
    Testnet,
    Stagenet,
    DevnetAmplifier,
}

impl Network {
    pub const ALL: &'static [Self] = &[
        Self::Mainnet,
        Self::Testnet,
        Self::Stagenet,
        Self::DevnetAmplifier,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
            Self::Stagenet => "stagenet",
            Self::DevnetAmplifier => "devnet-amplifier",
        }
    }

    /// The network this binary was compiled against, as selected by the
    /// active cargo feature flag. The fall-through is `DevnetAmplifier`,
    /// matching the existing `network_name()` helpers.
    pub fn from_features() -> Self {
        if cfg!(feature = "mainnet") {
            Self::Mainnet
        } else if cfg!(feature = "testnet") {
            Self::Testnet
        } else if cfg!(feature = "stagenet") {
            Self::Stagenet
        } else {
            Self::DevnetAmplifier
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Network {
    type Err = eyre::Report;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "mainnet" => Ok(Self::Mainnet),
            "testnet" => Ok(Self::Testnet),
            "stagenet" => Ok(Self::Stagenet),
            "devnet-amplifier" => Ok(Self::DevnetAmplifier),
            other => Err(eyre!(
                "unknown network '{other}' (expected one of: mainnet, testnet, stagenet, devnet-amplifier)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// HubChain — the ITS hub's source-chain identity.
// ---------------------------------------------------------------------------

/// Marker for the literal source-chain string the ITS hub uses (`"axelar"`).
/// Use `HubChain::NAME` everywhere a hub-routed message names its source.
/// Doesn't replace `"axelar"` as a Cosmos bech32 HRP (that's a different
/// domain — see `cosmos::derive_axelar_wallet`).
pub struct HubChain;

impl HubChain {
    /// The on-the-wire string used in cosmwasm message bodies, gateway events,
    /// and EVM `execute(commandId, sourceChain, ...)` arguments.
    pub const NAME: &str = "axelar";
}

impl fmt::Display for HubChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::NAME)
    }
}

// ---------------------------------------------------------------------------
// ChainKey vs ChainAxelarId — the same chain, two distinct identities.
// ---------------------------------------------------------------------------
//
// A given chain has two strings that identify it in this codebase:
//
//   * `ChainKey` — the JSON object key inside `chains` (e.g. `"avalanche"`).
//     Used for `chains.get(...)`, RPC lookups, and as the local CLI flag value.
//
//   * `ChainAxelarId` — the value of the `axelarId` field inside that JSON
//     entry (e.g. `"Avalanche"`). Used inside cosmwasm contract paths
//     (`/axelar/contracts/Gateway/{axelar_id}/address`) and inside EVM ITS
//     calls (`isTrustedChain(...)`, the inner ITS payload's source-chain).
//
// They are often (but not always) the same string. Mixing them up is a real
// bug we hit in development — the compiler now refuses.

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ChainKey(String);

impl ChainKey {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChainKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ChainKey {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<ChainKey> for String {
    fn from(c: ChainKey) -> Self {
        c.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ChainAxelarId(String);

impl ChainAxelarId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChainAxelarId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ChainAxelarId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<ChainAxelarId> for String {
    fn from(c: ChainAxelarId) -> Self {
        c.0
    }
}

/// JSON pointer to the Cosmos `Gateway` contract address for a given chain.
/// Takes a `ChainAxelarId` because that's what the Axelar deployments use as
/// the path segment (NOT the JSON key — they often differ).
pub fn cosm_gateway_pointer(chain: &ChainAxelarId) -> String {
    format!("/axelar/contracts/Gateway/{}/address", chain.as_str())
}

/// JSON pointer to the `VotingVerifier` contract address for a given chain.
pub fn voting_verifier_pointer(chain: &ChainAxelarId) -> String {
    format!(
        "/axelar/contracts/VotingVerifier/{}/address",
        chain.as_str()
    )
}

/// JSON pointer to the `MultisigProver` contract address for a given chain.
pub fn multisig_prover_pointer(chain: &ChainAxelarId) -> String {
    format!(
        "/axelar/contracts/MultisigProver/{}/address",
        chain.as_str()
    )
}

// ---------------------------------------------------------------------------
// ItsMessageType — the discriminator for inbound/outbound ITS messages.
// ---------------------------------------------------------------------------

/// First field of every ITS payload, both on the wire (Solana borsh enum
/// discriminant) and inside the EVM hub envelope (the leading uint256 of
/// `abi.encode(messageType, ...)`). Numeric values must match
/// `interchain-token-service/contracts/InterchainTokenService.sol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u64)]
pub enum ItsMessageType {
    InterchainTransfer = 0,
    DeployInterchainToken = 1,
    /// Wrapping discriminator for outbound (source → hub) messages. Defined
    /// here for completeness even though we don't currently send `SendToHub`
    /// envelopes from this codebase — the Solana ITS program builds them on
    /// our behalf via `solana_axelar_its::encoding::HubMessage::SendToHub`.
    #[allow(dead_code)]
    SendToHub = 3,
    ReceiveFromHub = 4,
}

impl ItsMessageType {
    pub const fn as_u64(self) -> u64 {
        self as u64
    }

    /// Convenience for the EVM ABI side, which expects each discriminator as
    /// a `uint256`.
    pub fn as_u256(self) -> alloy::primitives::U256 {
        alloy::primitives::U256::from(self.as_u64())
    }
}

// ---------------------------------------------------------------------------
// TestTokenSpec — name/symbol/decimals collapsed into one value.
// ---------------------------------------------------------------------------

/// The bundle of metadata that defines a test interchain token. Every test
/// flow (legacy EVM-direct, ITS config-mode, load-tests) used to keep its
/// own three constants — `TOKEN_NAME`/`TOKEN_SYMBOL`/`TOKEN_DECIMALS` —
/// scattered across files. They live here now.
#[derive(Debug, Clone, Copy)]
pub struct TestTokenSpec {
    pub name: &'static str,
    pub symbol: &'static str,
    pub decimals: u8,
}

/// Spec used by the legacy `axe test its` command (EVM-direct, evm-to-evm).
pub const EVM_LEGACY_SPEC: TestTokenSpec = TestTokenSpec {
    name: "Axe Test Token",
    symbol: "AXE",
    decimals: 18,
};

/// Spec used by `axe test its --config ...` (Solana → EVM with manual relay).
/// Decimals are 9 because Solana's SPL convention.
pub const ITS_CONFIG_SPEC: TestTokenSpec = TestTokenSpec {
    name: "Axe ITS Test",
    symbol: "AXE",
    decimals: 9,
};

/// Spec used by the EVM-source load test (`load-test ... --protocol its`,
/// EVM → Solana).
pub const LOAD_TEST_EVM_SPEC: TestTokenSpec = TestTokenSpec {
    name: "AXE",
    symbol: "AXE",
    decimals: 18,
};

/// Spec used by the Solana-source load test (`load-test ... --protocol its`,
/// Solana → EVM).
pub const LOAD_TEST_SOL_SPEC: TestTokenSpec = TestTokenSpec {
    name: "AXE",
    symbol: "AXE",
    decimals: 9,
};
