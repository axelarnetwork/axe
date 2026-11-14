use std::path::PathBuf;

use alloy::primitives::{Address, Bytes, TxHash, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr, eyre};

use super::report::Output;
use super::routes::Chain;
use crate::commands::intents::execution::RECEIPT_TIMEOUT;
use crate::commands::intents::route::{DiscoveryFeedback, resolve_evm_rpc};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, EvmEndpoints, send_tx_confirmed};
use crate::retry::retry_with_fallback_all;
use crate::types::Network;
use crate::ui;

sol! {
    interface WrappedNative {
        function deposit() external payable;
    }
}

/// The EVM chain every route sends from, connected through its first working
/// RPC.
pub struct SourceChain {
    pub chain_id: u64,
    pub endpoints: EvmEndpoints,
}

impl SourceChain {
    pub async fn connect(network: Network, config: Option<PathBuf>, chain: Chain) -> Result<Self> {
        let chain_id = chain
            .caip2
            .strip_prefix("eip155:")
            .and_then(|id| id.parse::<u64>().ok())
            .ok_or_else(|| eyre!("{} is not an EVM chain", chain.caip2))?;
        let config_path = crate::config_source::resolve(network, config)
            .await?
            .into_path();
        let config = ChainsConfig::load(&config_path).await?;
        let rpc_url = resolve_evm_rpc(&config, chain_id, chain.label, DiscoveryFeedback::Detailed)
            .await
            .ok_or_else(|| eyre!("{} has no working RPC in the chains config", chain.label))?;
        Ok(Self {
            chain_id,
            endpoints: EvmEndpoints::connect(std::slice::from_ref(&rpc_url))?,
        })
    }

    /// Send `tx` from `signer` and wait for its receipt. A revert is an error.
    pub async fn send(
        &self,
        signer: &PrivateKeySigner,
        tx: TransactionRequest,
        label: &str,
    ) -> Result<TxHash> {
        send_tx_confirmed(
            &self.endpoints,
            signer,
            tx,
            label,
            RECEIPT_TIMEOUT,
            ui::warn,
        )
        .await
    }

    /// Wrap native gas into `token` to cover what the wallet's balance lacks
    /// of `amount`. Returns the wrap transaction when one was needed.
    pub async fn wrap_shortfall(
        &self,
        signer: &PrivateKeySigner,
        token: &str,
        amount: U256,
        output: Output,
    ) -> Result<Option<TxHash>> {
        let token: Address = token.parse().wrap_err("invalid wrapped token address")?;
        let balance = self.token_balance(token, signer.address()).await?;
        let Some(shortfall) = shortfall(amount, balance) else {
            return Ok(None);
        };
        output.kv(
            "wrap",
            &format!("{shortfall} base units of native gas (wallet holds {balance} wrapped)"),
        );
        let hash = self
            .send(
                signer,
                self.deposit(signer.address(), token, shortfall),
                "rfq wrap",
            )
            .await?;
        output.kv("wrap tx", &hash.to_string());
        Ok(Some(hash))
    }

    async fn token_balance(&self, token: Address, owner: Address) -> Result<U256> {
        retry_with_fallback_all(
            "rfq wrapped balance",
            self.endpoints.providers(),
            |provider| async move { ERC20::new(token, provider).balanceOf(owner).call().await },
        )
        .await
        .map_err(|error| eyre!("could not read the wrapped native balance: {error}"))
    }

    /// WETH9 `deposit()` on `token`, paying `amount` of native gas.
    fn deposit(&self, sender: Address, token: Address, amount: U256) -> TransactionRequest {
        TransactionRequest {
            chain_id: Some(self.chain_id),
            ..Default::default()
        }
        .from(sender)
        .to(token)
        .input(Bytes::from(WrappedNative::depositCall {}.abi_encode()).into())
        .value(amount)
    }
}

fn shortfall(amount: U256, balance: U256) -> Option<U256> {
    amount
        .checked_sub(balance)
        .filter(|missing| !missing.is_zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_only_the_missing_part_of_the_amount() {
        let amount = U256::from(100u64);
        assert_eq!(shortfall(amount, U256::ZERO), Some(amount));
        assert_eq!(
            shortfall(amount, U256::from(40u64)),
            Some(U256::from(60u64))
        );
        assert_eq!(shortfall(amount, amount), None);
        assert_eq!(shortfall(amount, U256::from(500u64)), None);
    }

    #[test]
    fn the_wrap_is_a_payable_deposit_on_the_wrapped_token() {
        let source = SourceChain {
            chain_id: 43_113,
            endpoints: EvmEndpoints::connect(&["http://127.0.0.1:8545".to_owned()]).unwrap(),
        };
        let sender = Address::from([5u8; 20]);
        let token = Address::from([6u8; 20]);

        let request = source.deposit(sender, token, U256::from(60u64));

        assert_eq!(request.chain_id, Some(43_113));
        assert_eq!(request.from, Some(sender));
        assert_eq!(request.value, Some(U256::from(60u64)));
        assert_eq!(request.to.and_then(|to| to.to().copied()), Some(token));
        assert_eq!(
            request.input.input().map(|data| data.to_vec()),
            Some(vec![0xd0, 0xe3, 0x0d, 0xb0])
        );
    }
}
