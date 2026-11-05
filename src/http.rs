//! HTTP client for Reddit.
//!
//! Uses hyper over rustls, deliberately *without* HTTP/2: Reddit's edge serves
//! HTTP/2 from non-browser stacks with its HTML "You've been blocked by network
//! security" page (see [`crate::client`]), while plain HTTP/1.1 requests are
//! answered normally. rustls is pure Rust, so the crate builds without a C/C++
//! toolchain.
//!
//! Requests can go through an HTTP proxy (`CONNECT` tunnel), which is what the
//! host needs when it cannot reach Reddit directly. The standard curl/Go
//! variables are honoured per destination scheme:
//!
//! | destination | lookup order |
//! |---|---|
//! | `https://` | `HTTPS_PROXY` -> `ALL_PROXY` -> `HTTP_PROXY` |
//! | `http://` | `HTTP_PROXY` -> `ALL_PROXY` |
//!
//! Lower-case variants are accepted too, and `NO_PROXY` excludes hosts. Name
//! resolution for the target host then happens at the proxy, not here.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use hyper::client::connect::{Connected, Connection};
use hyper::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING};
use hyper::service::Service;
use hyper::{Body, Client, Request, Response, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use log::{debug, info, warn};
use rustls::ClientConfig;
use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// Requests are small; Reddit answers in well under this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on a proxy's `CONNECT` response headers.
const MAX_PROXY_RESPONSE: usize = 8192;

pub static CLIENT: LazyLock<Client<HttpsConnector<TunnelConnector>, Body>> = LazyLock::new(|| {
	let mut roots = rustls::RootCertStore::empty();
	roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

	// No ALPN: hyper-rustls then speaks HTTP/1.1, which is what Reddit serves.
	let mut config = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
	config.alpn_protocols.clear();
	config.enable_sni = true;

	let connector = TunnelConnector::new(proxies_from_env());
	let https = hyper_rustls::HttpsConnectorBuilder::new()
		.with_tls_config(config)
		.https_or_http()
		.enable_http1()
		.wrap_connector(connector);

	Client::builder()
		.pool_idle_timeout(Duration::from_secs(60))
		.build(https)
});

/// An HTTP proxy that requests are tunnelled through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proxy {
	host: String,
	port: u16,
	/// Ready-made `Proxy-Authorization` value, if the proxy URL carried credentials.
	authorization: Option<String>,
}

/// The proxies to use, by destination scheme, plus the `NO_PROXY` exclusions.
#[derive(Clone, Debug, Default)]
pub struct Proxies {
	https: Option<Proxy>,
	http: Option<Proxy>,
	bypass: Vec<String>,
}

impl Proxies {
	/// The proxy for a destination scheme, if any.
	fn for_scheme(&self, scheme: &str) -> Option<&Proxy> {
		if scheme == "http" {
			self.http.as_ref()
		} else {
			self.https.as_ref()
		}
	}

	/// Whether `NO_PROXY` says to reach this host directly.
	fn bypasses(&self, host: &str) -> bool {
		let host = host.to_ascii_lowercase();
		self.bypass.iter().any(|entry| {
			if entry == "*" {
				return true;
			}
			// `NO_PROXY` entries may carry a port, which is ignored here.
			let entry = entry.split(':').next().unwrap_or(entry).trim_start_matches('.');
			!entry.is_empty() && (host == entry || host.ends_with(&format!(".{entry}")))
		})
	}
}

/// Reads the proxy configuration from the environment.
fn proxies_from_env() -> Proxies {
	let proxies = resolve_proxies(|name| std::env::var(name).ok().filter(|value| !value.trim().is_empty()));

	for (scheme, proxy) in [("https", &proxies.https), ("http", &proxies.http)] {
		if let Some(proxy) = proxy {
			info!("[🌐] Routing {scheme} requests through proxy {}:{}", proxy.host, proxy.port);
		}
	}
	if proxies.https.is_none() && proxies.http.is_none() {
		info!("[🌐] No proxy configured, connecting directly");
	}
	if !proxies.bypass.is_empty() {
		info!("[🌐] NO_PROXY excludes: {}", proxies.bypass.join(", "));
	}

	proxies
}

/// Resolves the per-scheme proxies from an environment lookup.
///
/// `HTTPS_PROXY` is used for `https://` URLs and `HTTP_PROXY` for `http://`
/// ones; `ALL_PROXY` covers both, and `HTTP_PROXY` is the last resort for
/// `https://` (as in curl and Go, where it is often the only one set). Lower-case
/// spellings are accepted as well.
fn resolve_proxies(get: impl Fn(&str) -> Option<String>) -> Proxies {
	let var = |names: &[&str]| names.iter().find_map(|name| get(name));
	let proxy = |names: &[&str]| var(names).and_then(|raw| parse_proxy(&raw));

	let all = proxy(&["ALL_PROXY", "all_proxy"]);
	let http = proxy(&["HTTP_PROXY", "http_proxy"]).or_else(|| all.clone());
	let https = proxy(&["HTTPS_PROXY", "https_proxy"]).or(all).or_else(|| http.clone());

	let bypass = var(&["NO_PROXY", "no_proxy"])
		.map(|value| {
			value
				.split(',')
				.map(|entry| entry.trim().to_ascii_lowercase())
				.filter(|entry| !entry.is_empty())
				.collect()
		})
		.unwrap_or_default();

	Proxies { https, http, bypass }
}

/// Parses a proxy URL such as `http://user:pass@127.0.0.1:7890`.
fn parse_proxy(raw: &str) -> Option<Proxy> {
	let raw = raw.trim();
	if raw.is_empty() {
		return None;
	}
	// Accept a bare `host:port` too, but never guess a scheme for something that
	// already has one.
	let url = if raw.contains("://") {
		url::Url::parse(raw).ok()?
	} else {
		url::Url::parse(&format!("http://{raw}")).ok()?
	};

	// Only HTTP proxies can tunnel with `CONNECT`; a `socks5://` value in
	// `ALL_PROXY` must be skipped rather than dialled as if it spoke HTTP.
	if !matches!(url.scheme(), "http" | "https") {
		warn!("[🌐] Ignoring proxy {raw:?}: only http(s) proxies are supported");
		return None;
	}

	let host = url.host_str()?.to_string();
	let port = url.port_or_known_default()?;

	let authorization = (!url.username().is_empty()).then(|| {
		let credentials = format!("{}:{}", url.username(), url.password().unwrap_or_default());
		format!("Basic {}", BASE64.encode(credentials))
	});

	Some(Proxy {
		host,
		port,
		authorization,
	})
}

/// A TCP stream that either goes straight to Reddit or through a `CONNECT` tunnel.
#[derive(Debug)]
pub struct TunnelStream(TcpStream);

impl Connection for TunnelStream {
	fn connected(&self) -> Connected {
		Connected::new()
	}
}

impl AsyncRead for TunnelStream {
	fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
		Pin::new(&mut self.0).poll_read(cx, buf)
	}
}

impl AsyncWrite for TunnelStream {
	fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
		Pin::new(&mut self.0).poll_write(cx, buf)
	}

	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
		Pin::new(&mut self.0).poll_flush(cx)
	}

	fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
		Pin::new(&mut self.0).poll_shutdown(cx)
	}
}

/// Connector that opens the TCP connection, through the proxy when one is set.
///
/// hyper-rustls wraps this and adds TLS, so the proxy only ever sees a `CONNECT`
/// tunnel and the host name being asked for.
#[derive(Clone, Debug)]
pub struct TunnelConnector {
	proxies: Proxies,
}

impl TunnelConnector {
	fn new(proxies: Proxies) -> Self {
		Self { proxies }
	}
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl Service<Uri> for TunnelConnector {
	type Response = TunnelStream;
	type Error = BoxError;
	type Future = Pin<Box<dyn Future<Output = Result<TunnelStream, BoxError>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
	}

	fn call(&mut self, destination: Uri) -> Self::Future {
		let host = destination.host().unwrap_or_default().trim_matches(['[', ']']).to_string();
		let scheme = destination.scheme_str().unwrap_or("https");
		let port = destination.port_u16().unwrap_or(if scheme == "http" { 80 } else { 443 });
		let bypass = self.proxies.bypasses(&host);
		let proxy = (!bypass).then(|| self.proxies.for_scheme(scheme).cloned()).flatten();

		Box::pin(async move {
			let stream = match proxy {
				None => TcpStream::connect((host.as_str(), port)).await?,
				Some(proxy) => tunnel(&proxy, &host, port).await?,
			};
			stream.set_nodelay(true).ok();
			Ok(TunnelStream(stream))
		})
	}
}

/// Opens a `CONNECT` tunnel to `host:port` through `proxy`.
async fn tunnel(proxy: &Proxy, host: &str, port: u16) -> Result<TcpStream, BoxError> {
	let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port)).await?;

	let authority = format!("{host}:{port}");
	let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
	if let Some(authorization) = &proxy.authorization {
		request.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
	}
	request.push_str("\r\n");

	stream.write_all(request.as_bytes()).await?;
	stream.flush().await?;

	// Read the response one byte at a time: a larger read could swallow the
	// first bytes of the tunnel once the blank line is gone.
	let mut response = Vec::with_capacity(128);
	let mut byte = [0u8; 1];
	while !response.ends_with(b"\r\n\r\n") {
		if response.len() >= MAX_PROXY_RESPONSE {
			return Err(format!("Proxy {authority} CONNECT response is too large").into());
		}
		if stream.read(&mut byte).await? == 0 {
			return Err(format!("Proxy closed the connection during CONNECT {authority}").into());
		}
		response.push(byte[0]);
	}

	let response = String::from_utf8_lossy(&response);
	let status = response
		.lines()
		.next()
		.unwrap_or_default()
		.split_whitespace()
		.nth(1)
		.and_then(|code| code.parse::<u16>().ok());
	match status {
		Some(200..=299) => {
			debug!("Tunnelled {authority} through proxy {}:{}", proxy.host, proxy.port);
			Ok(stream)
		}
		status => Err(format!("Proxy refused CONNECT {authority}: {status:?} {}", response.lines().next().unwrap_or_default()).into()),
	}
}


/// A response with its body already collected.
pub struct Reply {
	pub status: StatusCode,
	pub headers: HeaderMap,
	pub body: Vec<u8>,
}

impl Reply {
	pub fn text(&self) -> String {
		String::from_utf8_lossy(&self.body).to_string()
	}
}

pub async fn request(method: &str, url: &str, headers: &[(String, String)], body: Option<Vec<u8>>) -> Result<Reply, String> {
	let uri: Uri = url.parse().map_err(|e| format!("Couldn't parse URL {url}: {e}"))?;

	let mut builder = Request::builder().method(method).uri(uri);
	{
		let map = builder.headers_mut().ok_or_else(|| "Couldn't build request".to_string())?;
		// Reddit only sends compressed bodies when asked; skip it and the
		// decoding as well.
		map.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
		for (key, value) in headers {
			let name = HeaderName::from_bytes(key.as_bytes()).map_err(|e| format!("Bad header name {key}: {e}"))?;
			let value = HeaderValue::from_str(value).map_err(|e| format!("Bad header value for {key}: {e}"))?;
			map.insert(name, value);
		}
	}

	let request = builder.body(body.map(Body::from).unwrap_or_else(Body::empty)).map_err(|e| format!("Couldn't build request: {e}"))?;

	let response = tokio::time::timeout(REQUEST_TIMEOUT, CLIENT.request(request))
		.await
		.map_err(|_| format!("Timed out requesting {url}"))?
		.map_err(|e| format!("Couldn't send request to {url}: {e}"))?;

	collect(response).await
}

/// Sends a request and hands the response back unread, for proxying media.
pub async fn stream(url: &str, headers: &[(String, String)]) -> Result<Response<Body>, String> {
	let uri: Uri = url.parse().map_err(|e| format!("Couldn't parse URL {url}: {e}"))?;

	let mut builder = Request::builder().method("GET").uri(uri);
	{
		let map = builder.headers_mut().ok_or_else(|| "Couldn't build request".to_string())?;
		for (key, value) in headers {
			let name = HeaderName::from_bytes(key.as_bytes()).map_err(|e| format!("Bad header name {key}: {e}"))?;
			let value = HeaderValue::from_str(value).map_err(|e| format!("Bad header value for {key}: {e}"))?;
			map.insert(name, value);
		}
	}

	let request = builder.body(Body::empty()).map_err(|e| format!("Couldn't build request: {e}"))?;

	tokio::time::timeout(REQUEST_TIMEOUT, CLIENT.request(request))
		.await
		.map_err(|_| format!("Timed out requesting {url}"))?
		.map_err(|e| format!("Couldn't send request to {url}: {e}"))
}

async fn collect(response: Response<Body>) -> Result<Reply, String> {
	let status = response.status();
	let headers = response.headers().clone();
	let body = hyper::body::to_bytes(response.into_body())
		.await
		.map_err(|e| format!("Failed receiving body from Reddit: {e}"))?;

	debug!("{} bytes from Reddit, status {status}", body.len());

	Ok(Reply {
		status,
		headers,
		body: body.to_vec(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::HashMap;

	fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
		let map: HashMap<String, String> = pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect();
		move |name: &str| map.get(name).cloned()
	}

	fn host_of(proxy: &Option<Proxy>) -> Option<&str> {
		proxy.as_ref().map(|proxy| proxy.host.as_str())
	}

	#[test]
	fn test_parse_proxy_accepts_urls_and_bare_authorities() {
		for raw in ["http://127.0.0.1:7890", "127.0.0.1:7890", "  http://127.0.0.1:7890  "] {
			let proxy = parse_proxy(raw).expect(raw);
			assert_eq!(proxy.host, "127.0.0.1", "{raw}");
			assert_eq!(proxy.port, 7890, "{raw}");
			assert!(proxy.authorization.is_none(), "{raw}");
		}

		assert!(parse_proxy("").is_none());
		assert!(parse_proxy("   ").is_none());
		assert!(parse_proxy("http://").is_none());
	}

	#[test]
	fn test_parse_proxy_builds_basic_authorization() {
		let proxy = parse_proxy("http://user:pa55@proxy.example:3128").unwrap();
		assert_eq!(proxy.host, "proxy.example");
		assert_eq!(proxy.port, 3128);
		assert_eq!(proxy.authorization.as_deref(), Some("Basic dXNlcjpwYTU1"));
	}

	#[test]
	fn test_https_proxy_is_used_for_https_destinations() {
		let proxies = resolve_proxies(env(&[("HTTPS_PROXY", "http://https.proxy:3128"), ("HTTP_PROXY", "http://http.proxy:8080")]));
		assert_eq!(host_of(&proxies.https), Some("https.proxy"));
		assert_eq!(host_of(&proxies.http), Some("http.proxy"));
		assert_eq!(proxies.for_scheme("https").map(|proxy| proxy.port), Some(3128));
		assert_eq!(proxies.for_scheme("http").map(|proxy| proxy.port), Some(8080));
	}

	#[test]
	fn test_http_proxy_alone_covers_https_too() {
		// Plenty of setups only export HTTP_PROXY; curl and Go treat it as the
		// fallback for https as well.
		let proxies = resolve_proxies(env(&[("HTTP_PROXY", "http://only.proxy:7890")]));
		assert_eq!(host_of(&proxies.https), Some("only.proxy"));
		assert_eq!(host_of(&proxies.http), Some("only.proxy"));
	}

	#[test]
	fn test_all_proxy_covers_both_schemes() {
		let proxies = resolve_proxies(env(&[("ALL_PROXY", "http://all.proxy:1080")]));
		assert_eq!(host_of(&proxies.https), Some("all.proxy"));
		assert_eq!(host_of(&proxies.http), Some("all.proxy"));
	}

	#[test]
	fn test_socks_proxies_are_ignored() {
		// `CONNECT` tunnelling is HTTP-only; a SOCKS value must not be dialled.
		let proxies = resolve_proxies(env(&[("ALL_PROXY", "socks5://socks.proxy:1080"), ("HTTP_PROXY", "http://http.proxy:8080")]));
		assert_eq!(host_of(&proxies.https), Some("http.proxy"));
		assert_eq!(host_of(&proxies.http), Some("http.proxy"));

		let proxies = resolve_proxies(env(&[("ALL_PROXY", "socks5://socks.proxy:1080")]));
		assert!(proxies.https.is_none());
		assert!(proxies.http.is_none());
	}

	#[test]
	fn test_lowercase_variables_are_accepted() {
		let proxies = resolve_proxies(env(&[("https_proxy", "http://lower.proxy:3128")]));
		assert_eq!(host_of(&proxies.https), Some("lower.proxy"));
	}

	#[test]
	fn test_empty_variables_are_ignored() {
		let proxies = resolve_proxies(env(&[("HTTPS_PROXY", "  "), ("ALL_PROXY", "")]));
		assert!(proxies.https.is_none());
		assert!(proxies.http.is_none());
		assert_eq!(proxies.for_scheme("https"), None);
	}

	#[test]
	fn test_no_proxy_bypass_matches_hosts_and_suffixes() {
		let proxies = resolve_proxies(env(&[
			("HTTPS_PROXY", "http://127.0.0.1:7890"),
			("NO_PROXY", "localhost, .example.com, other.test:8080"),
		]));
		assert!(proxies.bypasses("localhost"));
		assert!(proxies.bypasses("api.example.com"));
		assert!(proxies.bypasses("example.com"));
		assert!(proxies.bypasses("other.test"));
		assert!(!proxies.bypasses("reddit.com"));
		assert!(!proxies.bypasses("notexample.com"));

		let all = resolve_proxies(env(&[("HTTPS_PROXY", "http://127.0.0.1:7890"), ("NO_PROXY", "*")]));
		assert!(all.bypasses("reddit.com"));
	}
}
