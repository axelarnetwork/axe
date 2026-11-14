use alloy::primitives::{Address, U256};
use eyre::{Result, WrapErr, eyre};
use solana_sdk::pubkey::Pubkey;

use super::types::{Recipient, RfqRoute, RfqSource};
use crate::commands::intents::types::{BackendType, OrderType, QuoteRequest};
use crate::types::Network;

const NATIVE_TOKEN: &str = "0x0000000000000000000000000000000000000000";
const WRAPPED_DEFAULT_AMOUNT: u64 = 100_000_000_000_000;
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

/// The chains and tokens one network's routes run over. Addresses match
/// rfq-api's vendored axelar-app network configs.
struct NetworkTokens {
    chains: BySource<Chain>,
    solana: Chain,
    /// Each source chain's own wrapped native token: WAVAX and WETH.
    wrapped_native: BySource<WrappedToken>,
    usdc: BySource<&'static str>,
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
    solana: Chain {
        caip2: "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
        label: "Solana devnet",
    },
    wrapped_native: BySource {
        avalanche: WAVAX_TESTNET,
        ethereum: WETH_TESTNET,
    },
    usdc: BySource {
        avalanche: "0x57F1c63497AEe0bE305B8852b354CEc793da43bB",
        ethereum: "0x254d06f33bDc5b8ee05b2ea472107E300226659A",
    },
};

const WAVAX_MAINNET: WrappedToken = WrappedToken {
    evm: BySource {
        avalanche: "0xB31f66AA3C1e785363F0875A1B74E27b85FD66c7",
        ethereum: "0x38Cb2e8b5A4e34c38c87A56b2F4681D0579DAdD5",
    },
    solana: "5xMfUDBky6FF72h1Dc1x4LEwTZAxJKsy3iUV85cTodGA",
};

const WETH_MAINNET: WrappedToken = WrappedToken {
    evm: BySource {
        avalanche: "0x50498dC52bCd3dAeB54B7225A7d2FA8D536F313E",
        ethereum: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
    },
    solana: "9VbXPX6Xd7wHSirGXf6r7FhCVzzCLFxnF3xtqGyAcHZf",
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
    solana: Chain {
        caip2: "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
        label: "Solana",
    },
    wrapped_native: BySource {
        avalanche: WAVAX_MAINNET,
        ethereum: WETH_MAINNET,
    },
    usdc: BySource {
        avalanche: "0xfaB550568C688d5D8A52C7d794cb93Edc26eC0eC",
        ethereum: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
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
}

impl RoutePlan {
    pub fn resolve(network: Network, route: RfqRoute, source: RfqSource) -> Result<Self> {
        let tokens = NetworkTokens::for_network(network)?;
        let other = source.other();
        let wrapped = tokens.wrapped_native.on(source);
        let from = |token| RouteAsset {
            chain: tokens.chains.on(source),
            token,
        };
        let to = |token| RouteAsset {
            chain: tokens.chains.on(other),
            token,
        };
        let plan = match route {
            RfqRoute::Its => Self {
                route,
                from: from(wrapped.evm.on(source)),
                to: to(wrapped.evm.on(other)),
                wrap_shortfall: true,
                backends: &[BackendType::Its],
                default_amount: WRAPPED_DEFAULT_AMOUNT,
            },
            RfqRoute::NativeWrap => Self {
                route,
                from: from(NATIVE_TOKEN),
                to: to(wrapped.evm.on(other)),
                wrap_shortfall: false,
                backends: &[BackendType::Its],
                default_amount: WRAPPED_DEFAULT_AMOUNT,
            },
            // rfq-api quotes gateway-express instead of gateway when express
            // covers the amount.
            RfqRoute::Gateway => Self {
                route,
                from: from(tokens.usdc.on(source)),
                to: to(tokens.usdc.on(other)),
                wrap_shortfall: false,
                backends: &[BackendType::Gateway, BackendType::GatewayExpress],
                default_amount: GATEWAY_DEFAULT_AMOUNT,
            },
            RfqRoute::Solana => Self {
                route,
                from: from(wrapped.evm.on(source)),
                to: RouteAsset {
                    chain: tokens.solana,
                    token: wrapped.solana,
                },
                wrap_shortfall: true,
                backends: &[BackendType::Its],
                default_amount: WRAPPED_DEFAULT_AMOUNT,
            },
        };
        Ok(plan)
    }

    pub fn delivers_to_solana(&self) -> bool {
        self.route == RfqRoute::Solana
    }

    /// The recipient as typed, else the sender for EVM destinations or the
    /// configured Solana key for Solana ones.
    pub fn recipient(
        &self,
        explicit: Option<&str>,
        sender: Address,
        configured_solana: Option<Pubkey>,
    ) -> Result<Recipient> {
        let route = self.route;
        match (self.delivers_to_solana(), explicit) {
            (false, Some(value)) => value
                .parse()
                .map(Recipient::Evm)
                .wrap_err_with(|| format!("--recipient {value} is not an EVM address ({route} route)")),
            (false, None) => Ok(Recipient::Evm(sender)),
            (true, Some(value)) => value.parse().map(Recipient::Solana).wrap_err_with(|| {
                format!("--recipient {value} is not a Solana address ({route} route)")
            }),
            (true, None) => configured_solana.map(Recipient::Solana).ok_or_else(|| {
                eyre!(
                    "the solana route needs --recipient set to a Solana address; no Solana key is configured"
                )
            }),
        }
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
        assert!(fuji.wrap_shortfall);
        assert_eq!(fuji.backends, &[BackendType::Its]);
        assert_eq!(fuji.default_amount, 100_000_000_000_000);

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
    }

    #[test]
    fn native_wrap_spends_native_gas_for_the_wrapped_twin() {
        let route = plan(RfqRoute::NativeWrap, RfqSource::Avalanche);
        assert_eq!(route.from.token, NATIVE_TOKEN);
        assert_eq!(route.to.token, "0xEeE9FC09dCf8130b2ba3744b099266b3F1E89069");
        assert!(!route.wrap_shortfall);
        assert_eq!(route.backends, &[BackendType::Its]);
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
        assert_eq!(route.default_amount, 1_000_000);
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

        for network in [Network::DevnetAmplifier, Network::Stagenet] {
            assert!(RoutePlan::resolve(network, RfqRoute::Its, RfqSource::Avalanche).is_err());
        }
    }

    #[test]
    fn evm_recipients_default_to_the_sender() {
        let route = plan(RfqRoute::Its, RfqSource::Avalanche);
        let sender = Address::from([9u8; 20]);
        let other = "0x000000000000000000000000000000000000dEaD";

        assert_eq!(
            route.recipient(None, sender, None).unwrap(),
            Recipient::Evm(sender)
        );
        assert_eq!(
            route.recipient(Some(other), sender, None).unwrap(),
            Recipient::Evm(other.parse().unwrap())
        );
        assert!(
            route
                .recipient(
                    Some("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"),
                    sender,
                    None
                )
                .is_err()
        );
    }

    #[test]
    fn solana_recipients_come_from_the_flag_or_the_configured_key() {
        let route = plan(RfqRoute::Solana, RfqSource::Avalanche);
        let sender = Address::from([9u8; 20]);
        let typed = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";
        let configured = Pubkey::new_unique();

        assert_eq!(
            route
                .recipient(Some(typed), sender, Some(configured))
                .unwrap(),
            Recipient::Solana(typed.parse().unwrap())
        );
        assert_eq!(
            route.recipient(None, sender, Some(configured)).unwrap(),
            Recipient::Solana(configured)
        );
        assert!(route.recipient(None, sender, None).is_err());
        assert!(
            route
                .recipient(
                    Some("0x000000000000000000000000000000000000dEaD"),
                    sender,
                    None
                )
                .is_err()
        );
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

        let request = serde_json::to_value(route.quote_request(
            U256::from(route.default_amount),
            sender,
            recipient,
        ))
        .unwrap();

        assert_eq!(
            request,
            serde_json::json!({
                "fromChain": "eip155:43113",
                "fromToken": "0xd00ae08403B9bbb9124bB305C09058E32C39A48c",
                "toChain": "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
                "toToken": "8d7dmvUho3uAKZPceCbFr9k1LjQMZzPwHTFTDrhrP9kH",
                "amount": "100000000000000",
                "orderType": "EXACT_INPUT",
                "sender": "0x000000000000000000000000000000000000dEaD",
                "recipient": "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"
            })
        );
    }
}
