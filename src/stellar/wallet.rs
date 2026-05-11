//! Stellar wallet: an Ed25519 keypair plus the derived G-address, including
//! the well-known network passphrase lookup used to derive the signing
//! domain.

use ed25519_dalek::SigningKey;
use eyre::{Result, eyre};
use stellar_strkey::ed25519::PublicKey as StrPubKey;
use stellar_xdr::curr::{MuxedAccount, SignatureHint, Uint256};

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
