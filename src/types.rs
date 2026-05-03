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

use alloy::primitives::{Address, U256, address};
use eyre::{Result, eyre};

// ---------------------------------------------------------------------------
// Wei / token amount helpers
// ---------------------------------------------------------------------------

/// Wei per 1 ETH (or any 18-decimal native token). 10^18.
pub const WEI_PER_ETH: u128 = 1_000_000_000_000_000_000;

/// Wei per 0.001 ETH (1 milli-ETH). 10^15. Useful for `const`-time arithmetic
/// where the `u64`-based [`eth_milli`] helper isn't usable.
pub const WEI_PER_MILLI_ETH: u128 = 1_000_000_000_000_000;

/// `n` whole ETH expressed in wei. `eth(2) == 2 ETH`.
pub fn eth(n: u64) -> U256 {
    U256::from(n) * U256::from(WEI_PER_ETH)
}

/// `n` thousandths of ETH (milli-ETH) expressed in wei.
/// `eth_milli(200) == 0.2 ETH`. Use this for sub-ETH gas budgets where
/// `eth(N)` would force you to pick a too-coarse N.
pub fn eth_milli(n: u64) -> U256 {
    U256::from(n) * U256::from(WEI_PER_MILLI_ETH)
}

/// `n` whole tokens at the given decimal precision.
/// `whole_tokens(100, 18) == 100 tokens at 18-decimal precision`.
pub fn whole_tokens(n: u64, decimals: u8) -> U256 {
    U256::from(n) * U256::from(10u64).pow(U256::from(decimals))
}

// ---------------------------------------------------------------------------
// Standard burn / receiver address
// ---------------------------------------------------------------------------

/// Canonical "no-key" EOA used as a default receiver in tests and load tests.
/// ITS happily transfers tokens here (unlike its own proxy address, which
/// reverts EVM estimation). Picked because it's universally recognisable as
/// "this address has no owner".
pub const DEAD_ADDRESS: Address = address!("0x000000000000000000000000000000000000dEaD");

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

    /// Axelar's canonical relayer/operator wallets for this network. Every new
    /// EVM GMP deployment registers these with the chain's `Operators`
    /// contract so the protocol's relayers can call `executeMessage`.
    ///
    /// Source of truth: `axelar-contract-deployments/releases/evm/EVM-GMP-Release-Template.md`.
    /// Update both places together when Axelar rotates these keys.
    pub const fn axelar_operators(self) -> &'static [Address] {
        match self {
            Self::Mainnet => &MAINNET_OPERATORS,
            Self::Testnet => &TESTNET_OPERATORS,
            Self::Stagenet => &STAGENET_OPERATORS,
            Self::DevnetAmplifier => &DEVNET_AMPLIFIER_OPERATORS,
        }
    }
}

const MAINNET_OPERATORS: [Address; 2] = [
    address!("0x0CDeE446bD3c2E0D11568eeDB859Aa7112BE657a"),
    address!("0x1a07a2Ee043Dd3922448CD53D20Aae88a67e486E"),
];
const TESTNET_OPERATORS: [Address; 2] = [
    address!("0x8f23e84c49624a22e8c252684129910509ade4e2"),
    address!("0x3b401fa00191acb03c24ebb7754fe35d34dd1abd"),
];
const STAGENET_OPERATORS: [Address; 2] = [
    address!("0x7054acf1b2d01e33b86235458edf0046cc354293"),
    address!("0xf669ed1ebc608c48f58b6e290df566ede7fb1103"),
];
const DEVNET_AMPLIFIER_OPERATORS: [Address; 2] = [
    address!("0x01c793e1F8185a2527C5a2Ef3b4a3FBCb8982690"),
    address!("0xDb32E08fd5d6823E7f0298963E487d5df4e54b1E"),
];

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
