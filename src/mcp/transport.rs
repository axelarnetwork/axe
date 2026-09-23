//! The only module that names a transport.
//!
//! Everything above this speaks the protocol through rmcp's service traits, so
//! adding a transport means adding an [`Endpoint`] variant here and nothing
//! else.
//!
//! Two endpoints exist. Stdio is for a client that launches axe itself and
//! shares its environment with it. HTTP is for a client that must not: an
//! agent in a sandbox reaches a server the operator started outside it, and
//! the keys the server holds never enter the sandbox.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use eyre::{Result, WrapErr};
use http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::mcp::activity;
use crate::mcp::server::AxeMcp;

/// The environment variable carrying the HTTP bearer token.
pub const TOKEN_ENV: &str = "AXE_MCP_TOKEN";

/// Shorter tokens are refused at startup: a bearer token is the only thing
/// between the network and the tools. 32 characters is what `openssl rand
/// -hex 16` produces, 128 bits.
const MIN_TOKEN_CHARS: usize = 32;

/// Connections served at once. One agent needs a handful; the rest is
/// headroom. Beyond it, new connections wait rather than exhausting the
/// process.
const MAX_CONNECTIONS: usize = 64;

/// A client that has not finished sending headers by then is dropped, so a
/// trickle of half-open connections cannot pin the server.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A wrong token waits this long for its 401. It costs a real client nothing
/// and caps a guessing loop at one attempt per connection per second.
const AUTH_FAILURE_DELAY: Duration = Duration::from_secs(1);

/// Where the server meets its client.
pub enum Endpoint {
    /// This process's stdin and stdout. The client launched us.
    Stdio,
    /// A TCP address. The operator started us; clients connect with a token.
    Http { listen: SocketAddr, token: String },
}

/// Serve until the client disconnects (stdio) or the process is stopped
/// (HTTP).
pub async fn serve(server: AxeMcp, endpoint: Endpoint) -> Result<()> {
    match endpoint {
        Endpoint::Stdio => serve_stdio(server).await,
        Endpoint::Http { listen, token } => serve_http(server, listen, token).await,
    }
}

/// Serve over stdio until the client closes the connection.
///
/// The client launches axe as a child process, so stdin and stdout are the
/// channel and the process inherits the operator's environment. That is what
/// lets keys stay out of the tool schemas.
async fn serve_stdio(server: AxeMcp) -> Result<()> {
    let protocol_out = claim_stdout_for_protocol()?;

    let running = server
        .serve((tokio::io::stdin(), protocol_out))
        .await
        .map_err(|e| eyre::eyre!("MCP server failed to start: {e}"))?;

    running
        .waiting()
        .await
        .map_err(|e| eyre::eyre!("MCP server stopped unexpectedly: {e}"))?;

    Ok(())
}

/// Keep the protocol on a private copy of stdout and send everything else the
/// process prints to stderr.
///
/// Command implementations narrate through `println!`, and a detached load
/// test does so for minutes from a background thread. On stdio the client is
/// parsing stdout, so any of that would land inside the JSON-RPC stream.
/// Duplicating the descriptor first and then pointing descriptor 1 at stderr
/// keeps the channel clean without touching a single print site.
fn claim_stdout_for_protocol() -> Result<tokio::fs::File> {
    let protocol_out: OwnedFd =
        nix::unistd::dup(std::io::stdout()).wrap_err("could not duplicate stdout")?;
    nix::unistd::dup2_stdout(std::io::stderr()).wrap_err("could not send stdout to stderr")?;
    Ok(tokio::fs::File::from_std(std::fs::File::from(protocol_out)))
}

/// The bearer token clients must present, from the environment.
pub fn http_token_from_env() -> Result<String> {
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    if token.chars().count() < MIN_TOKEN_CHARS {
        return Err(eyre::eyre!(
            "--listen needs {TOKEN_ENV} set to a bearer token of at least {MIN_TOKEN_CHARS} \
             characters, for example: openssl rand -hex 32"
        ));
    }
    Ok(token)
}

/// Refuse to listen on every interface.
///
/// The `Host` allowlist is built from the bound address, so a wildcard bind
/// would reject every real request anyway. Better to say so at startup than
/// to serve 4xx to a confused operator. It also keeps "bind to an address only
/// the intended client can reach" a decision rather than a default.
fn check_listen_address(listen: SocketAddr) -> Result<()> {
    if listen.ip().is_unspecified() {
        return Err(eyre::eyre!(
            "--listen {listen} would accept connections on every interface; bind the one \
             address the client will use, for example 127.0.0.1:{} or a bridge address",
            listen.port()
        ));
    }
    Ok(())
}

/// Serve over HTTP until the process is stopped.
async fn serve_http(server: AxeMcp, listen: SocketAddr, token: String) -> Result<()> {
    check_listen_address(listen)?;
    let listener = TcpListener::bind(listen)
        .await
        .wrap_err_with(|| format!("could not listen on {listen}"))?;
    serve_http_on(listener, server, token).await
}

/// Accept connections on an already bound listener.
///
/// Every request must carry the token. A connection that fails is that
/// client's problem; the listener keeps accepting.
pub async fn serve_http_on(listener: TcpListener, server: AxeMcp, token: String) -> Result<()> {
    let bound = listener.local_addr().wrap_err("listener has no address")?;
    let service = Authenticated {
        token: Arc::from(token),
        inner: mcp_http_service(server, bound),
    };
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        let permit = connections
            .clone()
            .acquire_owned()
            .await
            .wrap_err("connection limiter closed")?;
        // A failed accept is that client's problem, not the server's:
        // ECONNABORTED (a client that reset between SYN and accept) and
        // EMFILE are routine and transient, and taking the listener down for
        // one would end the session and abandon any run in flight.
        let stream = match listener.accept().await {
            Ok((stream, _peer)) => stream,
            Err(e) => {
                activity::accept_failed(&e);
                continue;
            }
        };
        let service = service.clone();
        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .await;
            drop(permit);
        });
    }
}

type McpHttpService = StreamableHttpService<AxeMcp, LocalSessionManager>;

/// rmcp's streamable HTTP endpoint, configured for the address we bound.
///
/// rmcp checks the `Host` header against `allowed_hosts` and, for requests
/// that carry one, the `Origin` header against `allowed_origins`. Non-browser
/// clients send no `Origin` and pass. A browser page can pass only from the
/// bound address itself, which is what defeats DNS rebinding.
fn mcp_http_service(server: AxeMcp, bound: SocketAddr) -> McpHttpService {
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(allowed_hosts(bound))
        .with_allowed_origins([format!("http://{bound}")]);
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

/// The `Host` values a client may address us by: loopback names, and the
/// bound address with and without its port.
fn allowed_hosts(bound: SocketAddr) -> Vec<String> {
    vec![
        "localhost".to_string(),
        format!("localhost:{}", bound.port()),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        bound.ip().to_string(),
        bound.to_string(),
    ]
}

type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

/// The MCP endpoint behind a bearer-token check.
#[derive(Clone)]
struct Authenticated {
    token: Arc<str>,
    inner: McpHttpService,
}

impl hyper::service::Service<Request<Incoming>> for Authenticated {
    type Response = HttpResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, Infallible>> + Send>>;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        if let Err(status) = authorize(request.headers(), &self.token) {
            return Box::pin(async move {
                tokio::time::sleep(AUTH_FAILURE_DELAY).await;
                Ok(refusal(status))
            });
        }
        let mut inner = self.inner.clone();
        Box::pin(tower::Service::call(&mut inner, request))
    }
}

/// Accept a request only with `Authorization: Bearer <token>`.
///
/// Compared in constant time, so a wrong token cannot be narrowed down by
/// how far the comparison got before it failed.
fn authorize(headers: &HeaderMap, token: &str) -> Result<(), StatusCode> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// An empty response carrying the status and the challenge header.
fn refusal(status: StatusCode) -> HttpResponse {
    let mut response = Response::new(Full::new(Bytes::new()).boxed());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use http::header::{ACCEPT, AUTHORIZATION};
    use http::{HeaderMap, HeaderValue, StatusCode};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::{allowed_hosts, authorize, check_listen_address, constant_time_eq, serve_http_on};
    use crate::mcp::context::McpContext;
    use crate::mcp::policy::SpendPolicy;
    use crate::mcp::server::AxeMcp;
    use crate::types::Network;

    const TOKEN: &str = "correct-horse-battery-staple-and-more";

    #[test]
    fn wildcard_binds_are_refused_and_specific_ones_accepted() {
        for wildcard in ["0.0.0.0:8765", "[::]:8765"] {
            let err = check_listen_address(wildcard.parse().unwrap())
                .expect_err("a wildcard bind must be refused");
            assert!(err.to_string().contains("every interface"), "{err}");
        }
        for specific in ["127.0.0.1:8765", "10.0.0.7:8765", "[::1]:8765"] {
            assert!(check_listen_address(specific.parse().unwrap()).is_ok());
        }
    }

    fn headers_with(authorization: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = authorization {
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn only_the_exact_bearer_token_is_authorized() {
        assert_eq!(
            authorize(&headers_with(Some(&format!("Bearer {TOKEN}"))), TOKEN),
            Ok(())
        );
        for wrong in [
            None,
            Some("Bearer nope"),
            Some(&format!("Bearer {TOKEN}x")),
            Some(&format!("bearer {TOKEN}")),
            Some(TOKEN),
            Some(&format!("Basic {TOKEN}")),
        ] {
            assert_eq!(
                authorize(&headers_with(wrong), TOKEN),
                Err(StatusCode::UNAUTHORIZED),
                "{wrong:?} must be refused"
            );
        }
    }

    #[test]
    fn constant_time_comparison_agrees_with_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn hosts_cover_loopback_and_the_bound_address() {
        let hosts = allowed_hosts("10.0.0.7:8765".parse().unwrap());
        for expected in ["localhost", "localhost:8765", "10.0.0.7", "10.0.0.7:8765"] {
            assert!(hosts.iter().any(|h| h == expected), "{expected} missing");
        }
    }

    /// End to end over loopback: no token and a wrong token are refused before
    /// the protocol sees the request, the right token completes the handshake.
    #[tokio::test]
    async fn http_endpoint_admits_only_the_token_holder() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = AxeMcp::new(
            McpContext::new(
                Network::Testnet,
                false,
                PathBuf::from("."),
                SpendPolicy::default(),
            )
            .unwrap(),
        );
        tokio::spawn(serve_http_on(listener, server, TOKEN.to_string()));

        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        });
        let post = || {
            crate::http::client()
                .post(&url)
                .header(ACCEPT, "application/json, text/event-stream")
                .json(&initialize)
        };

        assert_eq!(post().send().await.unwrap().status(), 401);
        assert_eq!(
            post().bearer_auth("nope").send().await.unwrap().status(),
            401
        );

        let accepted = post().bearer_auth(TOKEN).send().await.unwrap();
        assert_eq!(accepted.status(), 200);
        let body = accepted.text().await.unwrap();
        assert!(body.contains("serverInfo"), "{body}");
    }
}
