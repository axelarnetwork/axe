//! XRPL wallet: secp256k1 keypair + derived r-address. Includes the
//! family-seed (`s...` base58check) decoder and the `rippled`-compatible
//! master-key derivation used to lift a 16-byte family seed into a 32-byte
//! signing key.

use eyre::{Result, eyre};
use libsecp256k1::{PublicKey, SecretKey};
use ripemd::Ripemd160;
use sha2::{Digest, Sha256, Sha512};
use xrpl_types::AccountId;

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
