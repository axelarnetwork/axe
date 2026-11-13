use alloy::hex;
use alloy::primitives::{Address, TxHash, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::Result;

use super::report::Output;
use super::routes::RouteAsset;
use crate::commands::intents::execution::submit_action;
use crate::commands::intents::route::read_asset_balance;
use crate::commands::intents::types::{
    Action, ActionKind, ActionPayload, AssetId, ChainRuntime, EvmTransactionPayload, WalletAsset,
};
use crate::evm::EvmEndpoints;

sol! {
    interface WrappedNative {
        function deposit() external payable;
    }
}

/// The source chain's RPC, as both the intents runtime and a connection.
pub struct SourceChain {
    pub runtime: ChainRuntime,
    pub endpoints: EvmEndpoints,
}

/// Wrap native gas to cover what the wallet's wrapped balance lacks of
/// `amount`. Returns the wrap transaction when one was needed.
pub async fn top_up_wrapped_native(
    source: &SourceChain,
    signer: &PrivateKeySigner,
    asset: RouteAsset,
    amount: U256,
    output: Output,
) -> Result<Option<TxHash>> {
    let balance =
        read_asset_balance(&source.runtime, &wrapped_asset(asset), signer.address()).await?;
    let Some(shortfall) = shortfall(amount, balance) else {
        return Ok(None);
    };
    output.kv(
        "wrap",
        &format!("{shortfall} base units of native gas (wallet holds {balance} wrapped)"),
    );
    let action = wrap_action(asset, shortfall, signer.address());
    let hash = submit_action(
        &source.endpoints,
        signer,
        asset.chain.caip2,
        &action,
        "rfq wrap",
        |message| output.warn(message),
    )
    .await?;
    output.kv("wrap tx", &hash.to_string());
    Ok(Some(hash))
}

fn shortfall(amount: U256, balance: U256) -> Option<U256> {
    amount
        .checked_sub(balance)
        .filter(|missing| !missing.is_zero())
}

fn wrapped_asset(asset: RouteAsset) -> WalletAsset {
    WalletAsset {
        id: AssetId {
            chain_id: asset.chain.caip2.to_owned(),
            token_address: asset.token.to_owned(),
        },
        chain_label: asset.chain.label.to_owned(),
        symbol: "wrapped native".to_owned(),
        decimals: 18,
        balance: U256::ZERO,
        native: false,
    }
}

/// WETH9 `deposit()` on the wrapped token, paying `amount` of native gas.
fn wrap_action(asset: RouteAsset, amount: U256, sender: Address) -> Action {
    Action {
        id: "wrap".to_owned(),
        label: "Wrap native gas".to_owned(),
        kind: ActionKind::Transaction,
        chain: asset.chain.caip2.to_owned(),
        payload: ActionPayload::EvmTransaction(EvmTransactionPayload {
            from: Some(sender.to_string()),
            to: asset.token.to_owned(),
            data: format!(
                "0x{}",
                hex::encode(WrappedNative::depositCall {}.abi_encode())
            ),
            value: amount.to_string(),
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::intents::e2e::routes::RoutePlan;
    use crate::commands::intents::e2e::types::{RfqRoute, RfqSource};
    use crate::commands::intents::execution::action_request;
    use crate::types::Network;

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
    fn the_wrap_is_a_payable_deposit_on_the_source_wrapped_token() {
        let plan =
            RoutePlan::resolve(Network::Testnet, RfqRoute::Its, RfqSource::Avalanche).unwrap();
        let sender = Address::from([5u8; 20]);
        let action = wrap_action(plan.from, U256::from(60u64), sender);

        let request = action_request(&action, sender, plan.from.chain.caip2).unwrap();

        assert_eq!(request.chain_id, Some(43_113));
        assert_eq!(request.value, Some(U256::from(60u64)));
        assert_eq!(
            request.to.and_then(|to| to.to().copied()),
            Some(plan.from.token.parse().unwrap())
        );
        assert_eq!(
            request.input.input().map(|data| data.to_vec()),
            Some(vec![0xd0, 0xe3, 0x0d, 0xb0])
        );
    }
}
