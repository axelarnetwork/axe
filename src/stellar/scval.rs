//! ScVal helpers — thin, correct encodings matching the JS reference. Builders
//! produce `ScVal` values for InvokeContract args; extractors decode common
//! return-value shapes back into native Rust types.

use eyre::{Result, eyre};
use stellar_strkey::{Contract as StrContract, ed25519::PublicKey as StrPubKey};
use stellar_xdr::curr::{
    AccountId, BytesM, Hash, PublicKey, ScAddress, ScSymbol, ScVal, StringM, Uint256, VecM,
};

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
