//! What an agent needs to know before it tries anything.
//!
//! Route validity is answered by delegating to the same `SupportedRoute`
//! resolution the load test performs, so the answer cannot drift from what
//! actually runs. The narrative documentation is served as resources so an
//! agent can read it on demand rather than carrying it in every request.

use std::path::Path;

use rmcp::model::Resource;
use serde::Serialize;

use crate::commands::load_test::route::is_supported;
use crate::commands::load_test::{self, Protocol, TestType};

/// One documentation page, embedded at compile time.
struct DocPage {
    file: &'static str,
    title: &'static str,
    description: &'static str,
    body: &'static str,
}

/// The pages worth putting in front of an agent, in the order a newcomer
/// would want them.
///
/// Curated rather than a directory scan, so a stray file cannot silently
/// become part of the contract, and embedded rather than read from disk, so
/// an installed binary serves them without needing the repo alongside it.
const DOC_PAGES: &[DocPage] = &[
    DocPage {
        file: "routes.md",
        title: "Supported routes",
        description: "Which chain pairs and protocols work, per network",
        body: include_str!("../../docs/routes.md"),
    },
    DocPage {
        file: "load-testing.md",
        title: "Testing and load testing",
        description: "Single messages, burst and sustained modes, per-chain keys",
        body: include_str!("../../docs/load-testing.md"),
    },
    DocPage {
        file: "load-test-coverage.md",
        title: "Load-test coverage matrix",
        description: "Dispatcher support by chain type",
        body: include_str!("../../docs/load-test-coverage.md"),
    },
    DocPage {
        file: "intents.md",
        title: "Intents",
        description: "RFQ routes, quotes, and the flows that spend on them",
        body: include_str!("../../docs/intents.md"),
    },
    DocPage {
        file: "decode.md",
        title: "Decoding",
        description: "Calldata, transactions, and on-chain activity",
        body: include_str!("../../docs/decode.md"),
    },
    DocPage {
        file: "monitoring.md",
        title: "Monitoring",
        description: "Verifiers, votes, and ITS ownership",
        body: include_str!("../../docs/monitoring.md"),
    },
    DocPage {
        file: "axelar-debugging.md",
        title: "Debugging cross-chain messages",
        description: "Tracing GMP and ITS through the pipeline",
        body: include_str!("../../docs/axelar-debugging.md"),
    },
];

fn uri_for(file: &str) -> String {
    format!("axe://docs/{file}")
}

/// Whether a route can be attempted, and why not when it cannot.
#[derive(Debug, Serialize)]
pub struct RouteSupport {
    pub protocol: Protocol,
    /// The pairing that was checked. Absent when the chains could not be
    /// resolved, so no pairing could be inferred.
    pub route: Option<TestType>,
    pub source_chain: String,
    pub destination_chain: String,
    pub supported: bool,
    /// Why the route was rejected, absent when it is supported.
    pub reason: Option<String>,
}

/// Ask the load test's own resolver whether a protocol and pairing are viable.
pub fn check_route(
    protocol: Protocol,
    route: TestType,
    source_chain: &str,
    destination_chain: &str,
) -> RouteSupport {
    let outcome = is_supported(protocol, route, source_chain, destination_chain);
    RouteSupport {
        protocol,
        route: Some(route),
        source_chain: source_chain.to_string(),
        destination_chain: destination_chain.to_string(),
        supported: outcome.is_ok(),
        reason: outcome.err().map(|e| format!("{e:#}")),
    }
}

/// Resolve both chains against a chains config first, inferring the pairing
/// when the caller did not name one, then ask the resolver.
///
/// This is the same resolution a load test performs before it spends, so an
/// unknown chain, a chain without an RPC, or two chains whose types form no
/// pairing all come back unsupported with the reason the run would have
/// failed with.
pub async fn check_route_in_config(
    config: &Path,
    protocol: Protocol,
    route: Option<TestType>,
    source_chain: &str,
    destination_chain: &str,
) -> RouteSupport {
    let resolved = load_test::resolve_from_config(
        &config.to_path_buf(),
        route,
        Some(source_chain.to_string()),
        Some(destination_chain.to_string()),
        None,
        None,
        None,
    )
    .await;

    match resolved {
        Ok(resolved) => check_route(
            protocol,
            resolved.test_type,
            resolved.source_chain.as_ref(),
            resolved.destination_chain.as_ref(),
        ),
        Err(e) => RouteSupport {
            protocol,
            route,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            supported: false,
            reason: Some(format!("{e:#}")),
        },
    }
}

/// The documentation pages, as protocol resources.
pub fn doc_resources() -> Vec<Resource> {
    DOC_PAGES
        .iter()
        .map(|page| {
            Resource::new(uri_for(page.file), page.title)
                .with_description(page.description)
                .with_mime_type("text/markdown")
        })
        .collect()
}

/// The body of a documentation page, or `None` for any URI outside the
/// curated list. Matching against the list is what stops a URI being used to
/// read arbitrary files.
pub fn doc_body(uri: &str) -> Option<&'static str> {
    DOC_PAGES
        .iter()
        .find(|page| uri == uri_for(page.file))
        .map(|page| page.body)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{check_route, check_route_in_config, doc_body, doc_resources, is_supported};
    use crate::commands::load_test::{Protocol, TestType};

    static FIXTURES: AtomicUsize = AtomicUsize::new(0);

    /// A two-chain config: one Solana, one EVM, both with an RPC.
    fn fixture_config() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axe-mcp-chains-{}-{}.json",
            std::process::id(),
            FIXTURES.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(
            &path,
            r#"{"chains":{
                "solana":{"chainType":"svm","axelarId":"solana","rpc":"http://solana.invalid"},
                "flow":{"chainType":"evm","axelarId":"flow","rpc":"http://flow.invalid"}
            },"axelar":{}}"#,
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn unknown_chain_is_unsupported_with_the_resolvers_reason() {
        let support =
            check_route_in_config(&fixture_config(), Protocol::Gmp, None, "nope-chain", "flow")
                .await;
        assert!(!support.supported);
        assert_eq!(support.route, None);
        assert!(
            support
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("nope-chain") && r.contains("not found in config")),
            "{:?}",
            support.reason
        );
    }

    #[tokio::test]
    async fn route_is_inferred_from_the_chain_types_when_omitted() {
        let support =
            check_route_in_config(&fixture_config(), Protocol::Gmp, None, "solana", "flow").await;
        assert_eq!(support.route, Some(TestType::SolToEvm));
        assert_eq!(
            support.supported,
            is_supported(Protocol::Gmp, TestType::SolToEvm, "solana", "flow").is_ok()
        );
    }

    /// The verdict is whatever the load test's resolver says, for a pair it
    /// accepts and a pair it rejects. The matrix itself is pinned by the
    /// resolver's own tests, so this asserts the delegation, not the matrix.
    #[test]
    fn route_verdict_is_the_resolvers_verdict() {
        for (protocol, route) in [
            (Protocol::Gmp, TestType::SolToEvm),
            (Protocol::Its, TestType::SolToSol),
        ] {
            let expected = is_supported(protocol, route, "solana", "flow");
            let support = check_route(protocol, route, "solana", "flow");

            assert_eq!(support.supported, expected.is_ok());
            assert_eq!(support.route, Some(route));
            assert_eq!(
                support.reason,
                expected.err().map(|e| format!("{e:#}")),
                "an unsupported pair carries the resolver's reason"
            );
            assert_eq!(support.source_chain, "solana");
            assert_eq!(support.destination_chain, "flow");
        }
    }

    #[test]
    fn route_support_serializes_in_the_vocabulary_the_caller_used() {
        let support = check_route(Protocol::ItsWithData, TestType::EvmToSol, "flow", "solana");
        let json = serde_json::to_value(&support).unwrap();
        assert_eq!(json["protocol"], "its-with-data");
        assert_eq!(json["route"], "evm-to-sol");
    }

    #[test]
    fn every_doc_resource_is_readable_and_nothing_else_is() {
        let resources = doc_resources();
        assert!(!resources.is_empty());
        for resource in &resources {
            assert!(
                doc_body(&resource.uri).is_some(),
                "{} unreadable",
                resource.uri
            );
            assert!(
                resource.description.is_some(),
                "{} undescribed",
                resource.uri
            );
        }
        assert!(doc_body("axe://docs/../Cargo.toml").is_none());
        assert!(doc_body("file:///etc/passwd").is_none());
    }
}
