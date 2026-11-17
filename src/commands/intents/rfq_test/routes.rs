use alloy::primitives::{Address, U256, address};
use eyre::{Result, eyre};
use solana_sdk::pubkey::Pubkey;

use super::types::{Recipient, RfqRoute, RfqSource};
use crate::commands::intents::types::{BackendType, OrderType, QuoteRequest};
use crate::types::Network;

const NATIVE_TOKEN: &str = "0x0000000000000000000000000000000000000000";
// rfq-api tells transfers apart by sender, contract, token and amount, so
// routes that share a token get distinct amounts.
const ITS_DEFAULT_AMOUNT: u64 = 100_000_000_000_000;
const NATIVE_WRAP_DEFAULT_AMOUNT: u64 = 110_000_000_000_000;
const SOLANA_DEFAULT_AMOUNT: u64 = 120_000_000_000_000;
/// 1 USDC: gateway USDC has 6 decimals on both networks.
const GATEWAY_DEFAULT_AMOUNT: u64 = 1_000_000;

/// One value per EVM source chain.
#[derive(Clone, Copy, Debug)]
struct BySource<T> {
    avalanche: T,
    ethereum: T,
}

impl<T: Copy> BySource<T> {
    const fn on(&self, source: RfqSource) -> T {
        match source {
            RfqSource::Avalanche => self.avalanche,
            RfqSource::Ethereum => self.ethereum,
        }
    }
}

/// A wrapped native token as deployed on both EVM sources and on Solana.
#[derive(Clone, Copy, Debug)]
struct WrappedToken {
    evm: BySource<&'static str>,
    solana: &'static str,
}

/// The token the its route sends to the same token on the other EVM source.
#[derive(Clone, Copy, Debug)]
struct ItsToken {
    evm: BySource<&'static str>,
    /// Whether it is the source's wrapped native token, topped up by wrapping
    /// native gas when the wallet holds too little.
    wraps_native: bool,
}

/// Each EVM source's own wrapped native token, WAVAX and WETH, and the Solana
/// chain that receives it.
struct WrappedNatives {
    tokens: BySource<WrappedToken>,
    solana: Chain,
}

/// The chains and tokens one network's routes run over. Addresses match
/// rfq-api's vendored axelar-app network configs.
struct NetworkTokens {
    chains: BySource<Chain>,
    its: BySource<ItsToken>,
    /// `None` where axelar-app lists the wrapped native tokens only on their
    /// origin chains, which leaves the native-wrap and solana routes nothing
    /// to quote.
    wrapped_natives: Option<WrappedNatives>,
    usdc: BySource<&'static str>,
    /// The contract every quoted transaction calls and every approval names.
    axelar_app: BySource<Address>,
    /// The most native gas, in wei, a quote may charge as transaction value.
    max_fee: BySource<u128>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
    pub caip2: &'static str,
    pub label: &'static str,
}

const WAVAX_TESTNET: WrappedToken = WrappedToken {
    evm: BySource {
        avalanche: "0xd00ae08403B9bbb9124bB305C09058E32C39A48c",
        ethereum: "0xEeE9FC09dCf8130b2ba3744b099266b3F1E89069",
    },
    solana: "8d7dmvUho3uAKZPceCbFr9k1LjQMZzPwHTFTDrhrP9kH",
};

const WETH_TESTNET: WrappedToken = WrappedToken {
    evm: BySource {
        avalanche: "0xd7680446338b2C078910d2D01125b0509b5F6337",
        ethereum: "0x7b79995e5f793A07Bc00c21412e50Ecae098E7f9",
    },
    solana: "HUHo4T9cEbpyBv5N8t33fopd8YL7jXF6SMtM1p3tBrAY",
};

const TESTNET: NetworkTokens = NetworkTokens {
    chains: BySource {
        avalanche: Chain {
            caip2: "eip155:43113",
            label: "Avalanche Fuji",
        },
        ethereum: Chain {
            caip2: "eip155:11155111",
            label: "Ethereum Sepolia",
        },
    },
    its: BySource {
        avalanche: ItsToken {
            evm: WAVAX_TESTNET.evm,
            wraps_native: true,
        },
        ethereum: ItsToken {
            evm: WETH_TESTNET.evm,
            wraps_native: true,
        },
    },
    wrapped_natives: Some(WrappedNatives {
        tokens: BySource {
            avalanche: WAVAX_TESTNET,
            ethereum: WETH_TESTNET,
        },
        solana: Chain {
            caip2: "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
            label: "Solana devnet",
        },
    }),
    usdc: BySource {
        avalanche: "0x57F1c63497AEe0bE305B8852b354CEc793da43bB",
        ethereum: "0x254d06f33bDc5b8ee05b2ea472107E300226659A",
    },
    axelar_app: BySource {
        avalanche: address!("0xe4f05a0D5541C03d07f5175147E92D796Cae8db6"),
        ethereum: address!("0xe4f05a0D5541C03d07f5175147E92D796Cae8db6"),
    },
    // Observed fees peak at about 0.05 AVAX and 0.0002 ETH, on the solana
    // route.
    max_fee: BySource {
        avalanche: 250_000_000_000_000_000,
        ethereum: 10_000_000_000_000_000,
    },
};

const AI_MAINNET: ItsToken = ItsToken {
    evm: BySource {
        avalanche: "0x2598c30330D5771AE9F983979209486aE26dE875",
        ethereum: "0x2598c30330D5771AE9F983979209486aE26dE875",
    },
    wraps_native: false,
};

const MAINNET: NetworkTokens = NetworkTokens {
    chains: BySource {
        avalanche: Chain {
            caip2: "eip155:43114",
            label: "Avalanche",
        },
        ethereum: Chain {
            caip2: "eip155:1",
            label: "Ethereum",
        },
    },
    its: BySource {
        avalanche: AI_MAINNET,
        ethereum: AI_MAINNET,
    },
    wrapped_natives: None,
    usdc: BySource {
        avalanche: "0xfaB550568C688d5D8A52C7d794cb93Edc26eC0eC",
        ethereum: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    },
    axelar_app: BySource {
        avalanche: address!("0x77Accd23cC3Ccc5E36a543CEdcD03764BF6AD401"),
        ethereum: address!("0x77Accd23cC3Ccc5E36a543CEdcD03764BF6AD401"),
    },
    // About 25 to 30 USD at the time of writing.
    max_fee: BySource {
        avalanche: 1_000_000_000_000_000_000,
        ethereum: 10_000_000_000_000_000,
    },
};

impl NetworkTokens {
    fn for_network(network: Network) -> Result<&'static Self> {
        match network {
            Network::Testnet => Ok(&TESTNET),
            Network::Mainnet => Ok(&MAINNET),
            Network::DevnetAmplifier | Network::Stagenet => Err(eyre!(
                "rfq route checks run on testnet or mainnet, not {network}"
            )),
        }
    }

    fn wrapped_natives(&self, network: Network, route: RfqRoute) -> Result<&WrappedNatives> {
        self.wrapped_natives.as_ref().ok_or_else(|| {
            eyre!(
                "the {route} route has nothing to quote on {network}: axelar-app lists WAVAX and WETH only on their origin chains"
            )
        })
    }
}

/// A token on a chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteAsset {
    pub chain: Chain,
    pub token: &'static str,
}

/// A route resolved against one network's chains and tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoutePlan {
    pub route: RfqRoute,
    pub from: RouteAsset,
    pub to: RouteAsset,
    /// Whether the source token is the wrapped native token, topped up by
    /// wrapping native gas when the wallet holds too little.
    pub wrap_shortfall: bool,
    /// Backends whose quotes the route accepts.
    pub backends: &'static [BackendType],
    pub default_amount: u64,
    pub axelar_app: Address,
    pub max_fee: U256,
}

impl RoutePlan {
    pub fn resolve(network: Network, route: RfqRoute, source: RfqSource) -> Result<Self> {
        let tokens = NetworkTokens::for_network(network)?;
        let other = source.other();
        let from = |token| RouteAsset {
            chain: tokens.chains.on(source),
            token,
        };
        let to = |token| RouteAsset {
            chain: tokens.chains.on(other),
            token,
        };
        let plan = |from: RouteAsset,
                    to: RouteAsset,
                    wrap_shortfall: bool,
                    backends: &'static [BackendType],
                    default_amount: u64| Self {
            route,
            from,
            to,
            wrap_shortfall,
            backends,
            default_amount,
            axelar_app: tokens.axelar_app.on(source),
            max_fee: U256::from(tokens.max_fee.on(source)),
        };
        Ok(match route {
            RfqRoute::Its => {
                let its = tokens.its.on(source);
                plan(
                    from(its.evm.on(source)),
                    to(its.evm.on(other)),
                    its.wraps_native,
                    &[BackendType::Its],
                    ITS_DEFAULT_AMOUNT,
                )
            }
            RfqRoute::NativeWrap => {
                let wrapped = tokens.wrapped_natives(network, route)?.tokens.on(source);
                plan(
                    from(NATIVE_TOKEN),
                    to(wrapped.evm.on(other)),
                    false,
                    &[BackendType::Its],
                    NATIVE_WRAP_DEFAULT_AMOUNT,
                )
            }
            // rfq-api quotes gateway-express instead of gateway when express
            // covers the amount.
            RfqRoute::Gateway => plan(
                from(tokens.usdc.on(source)),
                to(tokens.usdc.on(other)),
                false,
                &[BackendType::Gateway, BackendType::GatewayExpress],
                GATEWAY_DEFAULT_AMOUNT,
            ),
            RfqRoute::Solana => {
                let natives = tokens.wrapped_natives(network, route)?;
                let wrapped = natives.tokens.on(source);
                plan(
                    from(wrapped.evm.on(source)),
                    RouteAsset {
                        chain: natives.solana,
                        token: wrapped.solana,
                    },
                    true,
                    &[BackendType::Its],
                    SOLANA_DEFAULT_AMOUNT,
                )
            }
        })
    }

    /// `--amount`, else the route's default.
    pub fn amount(&self, explicit: Option<U256>) -> U256 {
        explicit.unwrap_or_else(|| U256::from(self.default_amount))
    }

    pub fn spends_native(&self) -> bool {
        self.route == RfqRoute::NativeWrap
    }

    pub fn delivers_to_solana(&self) -> bool {
        self.route == RfqRoute::Solana
    }

    /// `--solana-recipient` on the solana route, else `--recipient` or the
    /// sender.
    pub fn recipient(
        &self,
        evm: Option<Address>,
        solana: Option<Pubkey>,
        sender: Address,
    ) -> Result<Recipient> {
        if self.delivers_to_solana() {
            return solana
                .map(Recipient::Solana)
                .ok_or_else(|| eyre!("the solana route needs --solana-recipient"));
        }
        Ok(Recipient::Evm(evm.unwrap_or(sender)))
    }

    pub fn quote_request(
        &self,
        amount: U256,
        sender: Address,
        recipient: Recipient,
    ) -> QuoteRequest {
        QuoteRequest {
            from_chain: self.from.chain.caip2.to_owned(),
            from_token: self.from.token.to_owned(),
            to_chain: self.to.chain.caip2.to_owned(),
            to_token: self.to.token.to_owned(),
            amount: amount.to_string(),
            order_type: OrderType::ExactInput,
            sender: sender.to_string(),
            recipient: recipient.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(route: RfqRoute, source: RfqSource) -> RoutePlan {
        RoutePlan::resolve(Network::Testnet, route, source).unwrap()
    }

    #[test]
    fn its_sends_the_source_wrapped_native_token_to_its_twin() {
        let fuji = plan(RfqRoute::Its, RfqSource::Avalanche);
        assert_eq!(fuji.from.chain.caip2, "eip155:43113");
        assert_eq!(
            fuji.from.token,
            "0xd00ae08403B9bbb9124bB305C09058E32C39A48c"
        );
        assert_eq!(fuji.to.chain.caip2, "eip155:11155111");
        assert_eq!(fuji.to.token, "0xEeE9FC09dCf8130b2ba3744b099266b3F1E89069");
        assert!(fuji.wrap_shortfall && !fuji.spends_native());
        assert_eq!(fuji.backends, &[BackendType::Its]);
        assert_eq!(
            fuji.axelar_app,
            address!("0xe4f05a0D5541C03d07f5175147E92D796Cae8db6")
        );
        assert_eq!(fuji.max_fee, U256::from(250_000_000_000_000_000u128));

        let sepolia = plan(RfqRoute::Its, RfqSource::Ethereum);
        assert_eq!(
            sepolia.from.token,
            "0x7b79995e5f793A07Bc00c21412e50Ecae098E7f9"
        );
        assert_eq!(sepolia.to.chain.caip2, "eip155:43113");
        assert_eq!(
            sepolia.to.token,
            "0xd7680446338b2C078910d2D01125b0509b5F6337"
        );
        assert_eq!(sepolia.max_fee, U256::from(10_000_000_000_000_000u128));
    }

    #[test]
    fn native_wrap_spends_native_gas_for_the_wrapped_twin() {
        let route = plan(RfqRoute::NativeWrap, RfqSource::Avalanche);
        assert_eq!(route.from.token, NATIVE_TOKEN);
        assert_eq!(route.to.token, "0xEeE9FC09dCf8130b2ba3744b099266b3F1E89069");
        assert!(!route.wrap_shortfall && route.spends_native());
        assert_eq!(route.backends, &[BackendType::Its]);
    }

    #[test]
    fn routes_sharing_a_token_default_to_distinct_amounts() {
        let amounts: Vec<U256> = [RfqRoute::Its, RfqRoute::NativeWrap, RfqRoute::Solana]
            .into_iter()
            .map(|route| plan(route, RfqSource::Avalanche).amount(None))
            .collect();
        assert_eq!(
            amounts,
            [
                U256::from(100_000_000_000_000u64),
                U256::from(110_000_000_000_000u64),
                U256::from(120_000_000_000_000u64)
            ]
        );
        assert_eq!(
            plan(RfqRoute::Its, RfqSource::Avalanche).amount(Some(U256::from(7u8))),
            U256::from(7u8)
        );
    }

    #[test]
    fn gateway_accepts_plain_and_express_quotes() {
        let route = plan(RfqRoute::Gateway, RfqSource::Ethereum);
        assert_eq!(
            route.from.token,
            "0x254d06f33bDc5b8ee05b2ea472107E300226659A"
        );
        assert_eq!(route.to.token, "0x57F1c63497AEe0bE305B8852b354CEc793da43bB");
        assert!(!route.wrap_shortfall);
        assert_eq!(
            route.backends,
            &[BackendType::Gateway, BackendType::GatewayExpress]
        );
        assert_eq!(route.amount(None), U256::from(1_000_000u64));
    }

    #[test]
    fn mainnet_gateway_sends_one_axlusdc_for_usdc_on_ethereum() {
        let route =
            RoutePlan::resolve(Network::Mainnet, RfqRoute::Gateway, RfqSource::Avalanche).unwrap();

        assert_eq!(
            route.from.token,
            "0xfaB550568C688d5D8A52C7d794cb93Edc26eC0eC"
        );
        assert_eq!(route.to.token, "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        assert_eq!(route.amount(None), U256::from(1_000_000u64));
    }

    #[test]
    fn mainnet_its_sends_ai_without_wrapping() {
        for source in [RfqSource::Avalanche, RfqSource::Ethereum] {
            let route = RoutePlan::resolve(Network::Mainnet, RfqRoute::Its, source).unwrap();
            assert_eq!(
                route.from.token,
                "0x2598c30330D5771AE9F983979209486aE26dE875"
            );
            assert_eq!(route.to.token, route.from.token);
            assert!(!route.wrap_shortfall);
        }
    }

    #[test]
    fn mainnet_refuses_the_wrapped_native_routes() {
        for route in [RfqRoute::NativeWrap, RfqRoute::Solana] {
            let error =
                RoutePlan::resolve(Network::Mainnet, route, RfqSource::Avalanche).unwrap_err();
            assert!(error.to_string().contains("only on their origin chains"));
        }
    }

    #[test]
    fn solana_delivers_the_wrapped_native_mint_on_devnet() {
        let fuji = plan(RfqRoute::Solana, RfqSource::Avalanche);
        assert_eq!(
            fuji.to.chain.caip2,
            "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1"
        );
        assert_eq!(
            fuji.to.token,
            "8d7dmvUho3uAKZPceCbFr9k1LjQMZzPwHTFTDrhrP9kH"
        );
        assert!(fuji.wrap_shortfall);
        assert!(fuji.delivers_to_solana());

        let sepolia = plan(RfqRoute::Solana, RfqSource::Ethereum);
        assert_eq!(
            sepolia.to.token,
            "HUHo4T9cEbpyBv5N8t33fopd8YL7jXF6SMtM1p3tBrAY"
        );
        assert!(!plan(RfqRoute::Its, RfqSource::Avalanche).delivers_to_solana());
    }

    #[test]
    fn mainnet_resolves_and_other_networks_are_refused() {
        let route =
            RoutePlan::resolve(Network::Mainnet, RfqRoute::Its, RfqSource::Avalanche).unwrap();
        assert_eq!(route.from.chain.caip2, "eip155:43114");
        assert_eq!(route.to.chain.caip2, "eip155:1");
        assert_eq!(
            route.axelar_app,
            address!("0x77Accd23cC3Ccc5E36a543CEdcD03764BF6AD401")
        );

        for network in [Network::DevnetAmplifier, Network::Stagenet] {
            assert!(RoutePlan::resolve(network, RfqRoute::Its, RfqSource::Avalanche).is_err());
        }
    }

    #[test]
    fn evm_recipients_default_to_the_sender() {
        let route = plan(RfqRoute::Its, RfqSource::Avalanche);
        let sender = Address::from([9u8; 20]);
        let other = Address::from([8u8; 20]);

        assert_eq!(
            route.recipient(None, None, sender).unwrap(),
            Recipient::Evm(sender)
        );
        assert_eq!(
            route
                .recipient(Some(other), Some(Pubkey::new_unique()), sender)
                .unwrap(),
            Recipient::Evm(other)
        );
    }

    #[test]
    fn the_solana_route_needs_a_solana_recipient() {
        let route = plan(RfqRoute::Solana, RfqSource::Avalanche);
        let sender = Address::from([9u8; 20]);
        let solana = Pubkey::new_unique();

        assert_eq!(
            route
                .recipient(Some(Address::from([8u8; 20])), Some(solana), sender)
                .unwrap(),
            Recipient::Solana(solana)
        );
        assert!(route.recipient(Some(sender), None, sender).is_err());
    }

    #[test]
    fn quote_requests_are_exact_input_from_the_sender() {
        let route = plan(RfqRoute::Solana, RfqSource::Avalanche);
        let sender: Address = "0x000000000000000000000000000000000000dEaD"
            .parse()
            .unwrap();
        let recipient = Recipient::Solana(
            "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"
                .parse()
                .unwrap(),
        );

        let request =
            serde_json::to_value(route.quote_request(route.amount(None), sender, recipient))
                .unwrap();

        assert_eq!(
            request,
            serde_json::json!({
                "fromChain": "eip155:43113",
                "fromToken": "0xd00ae08403B9bbb9124bB305C09058E32C39A48c",
                "toChain": "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
                "toToken": "8d7dmvUho3uAKZPceCbFr9k1LjQMZzPwHTFTDrhrP9kH",
                "amount": "120000000000000",
                "orderType": "EXACT_INPUT",
                "sender": "0x000000000000000000000000000000000000dEaD",
                "recipient": "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"
            })
        );
    }
}
