use std::time::Instant;

use eyre::{Result, WrapErr, bail, eyre};
use reqwest::StatusCode;

use super::types::{
    BackendType, ChainsResponse, Quote, QuoteOutcome, QuoteRequest, QuoteResponse, StatusResponse,
    TimedQuote, TokensResponse,
};
use crate::types::Network;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RfqEnvironment {
    Devnet,
    Testnet,
    Mainnet,
}

impl RfqEnvironment {
    const fn base_url(self) -> &'static str {
        match self {
            Self::Devnet => "https://devnet.api.axelar.network/rfq/v1",
            Self::Testnet => "https://testnet.api.axelar.network/rfq/v1",
            Self::Mainnet => "https://api.axelar.network/rfq/v1",
        }
    }
}

impl TryFrom<Network> for RfqEnvironment {
    type Error = eyre::Report;

    fn try_from(network: Network) -> Result<Self> {
        match network {
            Network::DevnetAmplifier => Ok(Self::Devnet),
            Network::Testnet => Ok(Self::Testnet),
            Network::Mainnet => Ok(Self::Mainnet),
            Network::Stagenet => Err(eyre!(
                "Axelar RFQ has no stagenet endpoint; use devnet-amplifier, testnet, or mainnet"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RfqClient {
    base_url: String,
    client: reqwest::Client,
}

impl RfqClient {
    pub fn new(network: Network, base_url: Option<&str>) -> Result<Self> {
        let base_url = match base_url.filter(|value| !value.trim().is_empty()) {
            Some(base_url) => normalize_base_url(base_url)?,
            None => RfqEnvironment::try_from(network)?.base_url().to_owned(),
        };
        Ok(Self {
            base_url,
            client: crate::http::client().clone(),
        })
    }

    pub async fn chains(&self) -> Result<ChainsResponse> {
        self.get_json("chains").await
    }

    pub async fn tokens(&self) -> Result<TokensResponse> {
        self.get_json("tokens?backend=intent").await
    }

    pub async fn quote(&self, request: &QuoteRequest) -> Result<QuoteOutcome> {
        self.quote_from(request, &[BackendType::Intent]).await
    }

    /// The first quote, in the API's order, served by one of `backends`.
    pub async fn quote_from(
        &self,
        request: &QuoteRequest,
        backends: &[BackendType],
    ) -> Result<QuoteOutcome> {
        let started = Instant::now();
        let url = format!("{}/quote", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(request)
            .send()
            .await
            .wrap_err_with(|| format!("RFQ POST {url} request failed"))?;
        let response_url = response.url().to_string();
        let status = response.status();
        if status == StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(QuoteOutcome::Unavailable(response_message(response).await));
        }
        if !status.is_success() {
            return Err(http_error(
                "POST",
                &response_url,
                status,
                &response_message(response).await,
            ));
        }
        let body = response
            .json::<QuoteResponse>()
            .await
            .wrap_err("RFQ quote response did not match the expected schema")?;
        let Some(quote) = first_quote_from(body.quotes, backends) else {
            return Ok(QuoteOutcome::Unavailable(format!(
                "no {} quote returned",
                backend_names(backends)
            )));
        };
        Ok(QuoteOutcome::Available(Box::new(TimedQuote {
            quote,
            latency: started.elapsed(),
        })))
    }

    pub async fn status(&self, quote_id: &str) -> Result<StatusResponse> {
        let url = format!("{}/status", self.base_url);
        let response = self
            .client
            .get(&url)
            .query(&[("quoteId", quote_id)])
            .send()
            .await
            .wrap_err_with(|| format!("RFQ GET {url} request failed"))?;
        let response_url = response.url().to_string();
        let status = response.status();
        if !status.is_success() {
            return Err(http_error(
                "GET",
                &response_url,
                status,
                &response_message(response).await,
            ));
        }
        response
            .json::<StatusResponse>()
            .await
            .wrap_err("RFQ status response did not match the expected schema")
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}/{path}", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .wrap_err_with(|| format!("RFQ GET {url} request failed"))?;
        let response_url = response.url().to_string();
        let status = response.status();
        if !status.is_success() {
            return Err(http_error(
                "GET",
                &response_url,
                status,
                &response_message(response).await,
            ));
        }
        response
            .json::<T>()
            .await
            .wrap_err_with(|| format!("RFQ {path} response did not match the expected schema"))
    }
}

fn first_quote_from(quotes: Vec<Quote>, backends: &[BackendType]) -> Option<Quote> {
    quotes
        .into_iter()
        .find(|quote| backends.contains(&quote.backend.kind))
}

fn backend_names(backends: &[BackendType]) -> String {
    backends
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" or ")
}

async fn response_message(response: reqwest::Response) -> String {
    response
        .text()
        .await
        .unwrap_or_else(|_| "response body unavailable".to_owned())
        .chars()
        .take(500)
        .collect()
}

fn http_error(method: &str, url: &str, status: StatusCode, message: &str) -> eyre::Report {
    eyre::Report::new(RfqHttpError {
        method: method.to_owned(),
        url: url.to_owned(),
        status,
        message: message.to_owned(),
    })
}

/// A non-success answer from the RFQ API.
#[derive(Debug)]
pub struct RfqHttpError {
    pub(super) method: String,
    pub(super) url: String,
    pub status: StatusCode,
    pub(super) message: String,
}

impl std::fmt::Display for RfqHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "RFQ {} {} returned HTTP {}: {}",
            self.method, self.url, self.status, self.message
        )
    }
}

impl std::error::Error for RfqHttpError {}

fn normalize_base_url(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/');
    let parsed = reqwest::Url::parse(value)
        .wrap_err("INTENTS_API_URL must be an absolute HTTP or HTTPS URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("INTENTS_API_URL must use HTTP or HTTPS");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("INTENTS_API_URL must not contain a query or fragment");
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_networks_to_fixed_environments() {
        assert_eq!(
            RfqEnvironment::try_from(Network::DevnetAmplifier).unwrap(),
            RfqEnvironment::Devnet
        );
        assert_eq!(
            RfqEnvironment::try_from(Network::Testnet).unwrap(),
            RfqEnvironment::Testnet
        );
        assert_eq!(
            RfqEnvironment::try_from(Network::Mainnet).unwrap(),
            RfqEnvironment::Mainnet
        );
        assert!(RfqEnvironment::try_from(Network::Stagenet).is_err());
        assert_eq!(
            RfqEnvironment::Testnet.base_url(),
            "https://testnet.api.axelar.network/rfq/v1"
        );
    }

    #[test]
    fn http_errors_include_method_and_url() {
        let error = http_error(
            "GET",
            "https://testnet.api.axelar.network/rfq/v1/chains",
            StatusCode::NOT_FOUND,
            "404 page not found",
        );

        assert_eq!(
            error.to_string(),
            "RFQ GET https://testnet.api.axelar.network/rfq/v1/chains returned HTTP 404 Not Found: 404 page not found"
        );
    }

    #[test]
    fn accepts_and_normalizes_custom_base_urls() {
        let client =
            RfqClient::new(Network::Stagenet, Some("http://127.0.0.1:8080/solver/v1/")).unwrap();

        assert_eq!(client.base_url, "http://127.0.0.1:8080/solver/v1");
        assert!(RfqClient::new(Network::Testnet, Some("ftp://example.com")).is_err());
        assert!(RfqClient::new(Network::Testnet, Some("example.com/rfq/v1")).is_err());
    }

    fn quote_from(id: &str, backend: &str) -> Quote {
        serde_json::from_value(serde_json::json!({
            "quoteId": id,
            "selectionReason": "alternative",
            "backend": { "type": backend, "name": "test", "tracking": {}, "metadata": {} },
            "estimatedTimeSeconds": 60,
            "validity": { "type": "expires_at", "quoteExpiresAt": "2026-11-13T23:02:45Z" },
            "input": { "chain": "eip155:43113", "token": "0xfrom", "amount": "1" },
            "output": { "chain": "eip155:11155111", "token": "0xto", "amount": "1" },
            "fees": { "integrator": null },
            "actions": []
        }))
        .unwrap()
    }

    #[test]
    fn selects_the_first_quote_from_an_accepted_backend() {
        let quotes = || {
            vec![
                quote_from("intent-1", "intent"),
                quote_from("express-1", "gateway-express"),
                quote_from("gateway-1", "gateway"),
            ]
        };
        let gateway = [BackendType::Gateway, BackendType::GatewayExpress];

        assert_eq!(
            first_quote_from(quotes(), &gateway).unwrap().quote_id,
            "express-1"
        );
        assert_eq!(
            first_quote_from(quotes(), &[BackendType::Intent])
                .unwrap()
                .quote_id,
            "intent-1"
        );
        assert!(first_quote_from(quotes(), &[BackendType::Its]).is_none());
        assert_eq!(backend_names(&[BackendType::Intent]), "intent");
        assert_eq!(backend_names(&gateway), "gateway or gateway-express");
    }

    #[test]
    fn empty_overrides_use_the_selected_network_default() {
        for empty in ["", "   "] {
            let client = RfqClient::new(Network::Testnet, Some(empty)).unwrap();
            assert_eq!(client.base_url, RfqEnvironment::Testnet.base_url());
        }
    }
}
