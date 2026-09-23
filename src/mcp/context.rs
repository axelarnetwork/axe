//! Startup state, fixed for the life of the server.

use std::path::PathBuf;

use eyre::Result;

use crate::mcp::policy::SpendPolicy;
use crate::mcp::runs::RunRegistry;
use crate::types::Network;

/// What the operator chose when they started the server.
///
/// The network lives here rather than in tool arguments because the ITS and
/// GMP caches are scoped by a process-global, write-once network value. Chain
/// ids are not unique across Axelar networks, and letting one process serve
/// two networks would silently reuse the first network's cache for the second
/// -- a deterministic revert that has been observed live. Fixing the network
/// at startup reproduces the invariant the CLI already relies on.
#[derive(Clone)]
pub struct McpContext {
    network: Network,
    runs: RunRegistry,
    policy: SpendPolicy,
}

impl McpContext {
    /// Serve the network the operator pinned, mainnet included, unless they
    /// shut mainnet out when starting the server.
    pub fn new(
        network: Network,
        deny_mainnet: bool,
        reports_dir: PathBuf,
        policy: SpendPolicy,
    ) -> Result<Self> {
        if network == Network::Mainnet && deny_mainnet {
            return Err(eyre::eyre!(
                "refusing to serve mainnet: --deny-mainnet was passed, and these flows spend \
                 real funds"
            ));
        }

        Ok(Self {
            network,
            runs: RunRegistry::new(reports_dir),
            policy,
        })
    }

    /// The pinned network. No tool can change it.
    pub fn network(&self) -> Network {
        self.network
    }

    /// Background load-test runs started through this server.
    pub fn runs(&self) -> &RunRegistry {
        &self.runs
    }

    /// The operator's caps on fund-spending tools. No tool can change them.
    pub fn policy(&self) -> &SpendPolicy {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::McpContext;
    use crate::mcp::policy::SpendPolicy;
    use crate::types::Network;

    #[test]
    fn mainnet_is_refused_when_it_was_denied() {
        let err = McpContext::new(
            Network::Mainnet,
            true,
            PathBuf::from("."),
            SpendPolicy::default(),
        )
        .err()
        .expect("mainnet with --deny-mainnet must be refused");
        assert!(err.to_string().contains("--deny-mainnet"), "{err}");
    }

    #[test]
    fn mainnet_is_served_by_default() {
        let context = McpContext::new(
            Network::Mainnet,
            false,
            PathBuf::from("."),
            SpendPolicy::default(),
        )
        .unwrap();
        assert_eq!(context.network(), Network::Mainnet);
    }

    /// The flag names mainnet, so it says nothing about anything else.
    #[test]
    fn denying_mainnet_leaves_the_other_networks_alone() {
        let context = McpContext::new(
            Network::Testnet,
            true,
            PathBuf::from("."),
            SpendPolicy::default(),
        )
        .unwrap();
        assert_eq!(context.network(), Network::Testnet);
    }
}
