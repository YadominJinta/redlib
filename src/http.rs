//! HTTP client for Reddit.
//!
//! Uses hyper over rustls, deliberately *without* HTTP/2: Reddit's edge serves
//! HTTP/2 from non-browser stacks with its HTML "You've been blocked by network
//! security" page (see [`crate::client`]), while plain HTTP/1.1 requests are
//! answered normally. rustls is pure Rust, so the crate builds without a C/C++
//! toolchain.

use hyper::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING};
use hyper::{Body, Client, Request, Response, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use log::debug;
use rustls::ClientConfig;
use std::sync::LazyLock;
use std::time::Duration;

/// Requests are small; Reddit answers in well under this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub static CLIENT: LazyLock<Client<HttpsConnector<hyper::client::HttpConnector>, Body>> = LazyLock::new(|| {
	let mut roots = rustls::RootCertStore::empty();
	roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

	// No ALPN: hyper-rustls then speaks HTTP/1.1, which is what Reddit serves.
	let mut config = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
	config.alpn_protocols.clear();
	config.enable_sni = true;

	let mut connector = hyper::client::HttpConnector::new();
	connector.set_nodelay(true);
	connector.enforce_http(false);

	let https = hyper_rustls::HttpsConnectorBuilder::new()
		.with_tls_config(config)
		.https_or_http()
		.enable_http1()
		.wrap_connector(connector);

	Client::builder()
		.pool_idle_timeout(Duration::from_secs(60))
		.build(https)
});

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
