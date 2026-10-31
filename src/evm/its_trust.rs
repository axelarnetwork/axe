//! The edge ITS's trusted-chain API, in both of its generations.
//!
//! ITS 2.1.x keeps a per-chain trusted *address* that is the sentinel `"hub"`
//! for hub-routed chains. ITS 2.2.0 replaced it with a plain trusted-chain
//! flag. Calling the wrong setter reverts, so the generation is probed
//! on-chain rather than read from config. The contract compares chain names
//! as exact strings, so callers must use the ITS Hub's spelling (`Ethereum`,
//! not `ethereum`).

use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::Result;

use super::InterchainTokenService;

sol! {
    function setTrustedChain(string chainName);
    function removeTrustedChain(string chainName);
    function setTrustedAddress(string chain, string address_);
    function removeTrustedAddress(string chain);
}

/// Which trusted-chain setter an ITS exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustApi {
    /// `setTrustedChain(chain)` / `removeTrustedChain(chain)` (ITS >= 2.2.0).
    TrustedChain,
    /// `setTrustedAddress(chain, "hub")` / `removeTrustedAddress(chain)` (ITS 2.1.x).
    TrustedAddress,
}

impl TrustApi {
    pub const fn label(self) -> &'static str {
        match self {
            Self::TrustedChain => "setTrustedChain (ITS >= 2.2.0)",
            Self::TrustedAddress => "setTrustedAddress(…, \"hub\") (ITS 2.1.x)",
        }
    }
}

/// Hub-routed chains are trusted through the sentinel address `"hub"` on ITS 2.1.x.
const HUB_TRUSTED_ADDRESS: &str = "hub";

/// Ask an ITS which trusted-chain API it has: `isTrustedChain` exists only
/// from 2.2.0, `trustedAddress` only up to 2.1.x. A probe name is used so the
/// answer is independent of what is already trusted.
pub async fn probe<P: Provider>(provider: &P, its: Address, chain: &str) -> Result<TrustApi> {
    let contract = InterchainTokenService::new(its, provider);
    const PROBE: &str = "axe-trust-api-probe";
    if contract.isTrustedChain(PROBE.into()).call().await.is_ok() {
        return Ok(TrustApi::TrustedChain);
    }
    match contract.trustedAddress(PROBE.into()).call().await {
        Ok(_) => Ok(TrustApi::TrustedAddress),
        Err(error) => Err(eyre::eyre!(
            "ITS {its} on {chain} answers neither isTrustedChain nor trustedAddress — not a hub-era ITS, or the RPC is failing: {error}"
        )),
    }
}

/// Whether `chain_name` is trusted, through whichever API the ITS has.
pub async fn is_trusted<P: Provider>(
    provider: &P,
    its: Address,
    api: TrustApi,
    chain_name: &str,
) -> Result<bool> {
    let contract = InterchainTokenService::new(its, provider);
    let trusted = match api {
        TrustApi::TrustedChain => {
            contract
                .isTrustedChain(chain_name.to_string())
                .call()
                .await?
        }
        TrustApi::TrustedAddress => {
            contract
                .trustedAddress(chain_name.to_string())
                .call()
                .await?
                == HUB_TRUSTED_ADDRESS
        }
    };
    Ok(trusted)
}

pub fn encode_set(chain_name: &str, api: TrustApi) -> Bytes {
    match api {
        TrustApi::TrustedChain => setTrustedChainCall {
            chainName: chain_name.to_string(),
        }
        .abi_encode()
        .into(),
        TrustApi::TrustedAddress => setTrustedAddressCall {
            chain: chain_name.to_string(),
            address_: HUB_TRUSTED_ADDRESS.to_string(),
        }
        .abi_encode()
        .into(),
    }
}

pub fn encode_remove(chain_name: &str, api: TrustApi) -> Bytes {
    match api {
        TrustApi::TrustedChain => removeTrustedChainCall {
            chainName: chain_name.to_string(),
        }
        .abi_encode()
        .into(),
        TrustApi::TrustedAddress => removeTrustedAddressCall {
            chain: chain_name.to_string(),
        }
        .abi_encode()
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_chain_calldata_follows_the_its_version() {
        let new = encode_set("arc", TrustApi::TrustedChain);
        assert_eq!(&new[..4], setTrustedChainCall::SELECTOR.as_slice());
        let old = encode_set("arc", TrustApi::TrustedAddress);
        assert_eq!(&old[..4], setTrustedAddressCall::SELECTOR.as_slice());
        let decoded = setTrustedAddressCall::abi_decode(&old).unwrap();
        assert_eq!(decoded.chain, "arc");
        assert_eq!(decoded.address_, "hub");
        assert_eq!(
            &encode_remove("arc", TrustApi::TrustedAddress)[..4],
            removeTrustedAddressCall::SELECTOR.as_slice()
        );
        assert_eq!(
            &encode_remove("arc", TrustApi::TrustedChain)[..4],
            removeTrustedChainCall::SELECTOR.as_slice()
        );
    }
}
