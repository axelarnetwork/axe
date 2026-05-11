//! Wallet derivation. Currently a single function — the cosmos signing key
//! lives here so callers don't have to reach through `cosmos::tx` or
//! `cosmos::rpc` to mint a key.

use bip32::Mnemonic;
use cosmrs::bip32::XPrv;
use cosmrs::crypto::secp256k1::SigningKey;
use eyre::Result;

pub fn derive_axelar_wallet(mnemonic_str: &str) -> Result<(SigningKey, String)> {
    let mnemonic = Mnemonic::new(mnemonic_str, bip32::Language::English)
        .map_err(|e| eyre::eyre!("invalid mnemonic: {e}"))?;
    let seed = mnemonic.to_seed("");
    let path: cosmrs::bip32::DerivationPath = "m/44'/118'/0'/0/0"
        .parse()
        .map_err(|e| eyre::eyre!("invalid derivation path: {e}"))?;
    let child_xprv = XPrv::derive_from_path(seed, &path)
        .map_err(|e| eyre::eyre!("key derivation failed: {e}"))?;
    let signing_key = SigningKey::from_slice(&child_xprv.private_key().to_bytes())
        .map_err(|e| eyre::eyre!("invalid signing key: {e}"))?;
    let account_id = signing_key
        .public_key()
        .account_id("axelar")
        .map_err(|e| eyre::eyre!("account id derivation failed: {e}"))?;
    Ok((signing_key, account_id.to_string()))
}
