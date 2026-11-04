use crate::dbg_msg;
use crate::oauth::{force_refresh_token, token_daemon, Oauth, OauthBackendImpl};
use crate::server::RequestExt;
use crate::utils::{format_url, Post};
use arc_swap::ArcSwap;
use cached::proc_macro::cached;
use crate::http::{self, Reply};
use futures_lite::future::block_on;
use futures_lite::{future::Boxed, FutureExt};
use hyper::{header, Body, Request as HyperRequest, Response as HyperResponse};
use log::{error, trace, warn};
use percent_encoding::{percent_encode, CONTROLS};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::result::Result;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU16};
use std::sync::{LazyLock, Mutex};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";
const REDDIT_URL_BASE_HOST: &str = "oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";

const SHREDDIT_PATH_PREFIX: &str = "/svc/shreddit/";
// Asks for the bare fragment instead of a full page shell, roughly halving the response.
const SHREDDIT_PARTIAL_ACCEPT: &str = "text/vnd.reddit.partial+html, text/html;q=0.9";
// Reddit answers rate-limited shreddit requests with this reCAPTCHA page and a 200 status.
const SHREDDIT_CHALLENGE_TITLE: &str = "<title>Reddit - Prove your humanity</title>";

// Logged-out reddit.com session shared by shreddit requests. Reddit challenges
// cookieless clients after about ten requests, but gives each session a budget
// of a couple hundred requests. It is kept apart from the OAuth API requests on purpose.
static SHREDDIT_SESSION: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
// Leaves room for the requests already in flight when the session is rotated.
const SHREDDIT_RATELIMIT_ROTATE_BELOW: f32 = 20.0;

pub static OAUTH_CLIENT: LazyLock<ArcSwap<Oauth>> = LazyLock::new(|| {
	let client = block_on(Oauth::new());
	tokio::spawn(token_daemon());
	ArcSwap::new(client.into())
});

pub static OAUTH_RATELIMIT_REMAINING: AtomicU16 = AtomicU16::new(99);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

/// Gets the canonical path for a resource on Reddit. This is accomplished by
/// making a `HEAD` request to Reddit at the path given in `path`.
///
/// This function returns `Ok(Some(path))`, where `path`'s value is identical
/// to that of the value of the argument `path`, if Reddit responds to our
/// `HEAD` request with a 2xx-family HTTP code. It will also return an
/// `Ok(Some(String))` if Reddit responds to our `HEAD` request with a
/// `Location` header in the response, and the HTTP code is in the 3xx-family;
/// the `String` will contain the path as reported in `Location`. The return
/// value is `Ok(None)` if Reddit responded with a 3xx, but did not provide a
/// `Location` header. An `Err(String)` is returned if Reddit responds with a
/// 429, or if we were unable to decode the value in the `Location` header.
#[cached(size = 1024, time = 600, result = true)]
#[async_recursion::async_recursion]
pub async fn canonical_path(path: String, tries: i8) -> Result<Option<String>, String> {
	if tries == 0 {
		return Ok(None);
	}

	// for each URL pair, try the HEAD request
	let res = {
		// for url base and host in URL_PAIRS, try reddit_short_head(path.clone(), true, url_base, url_base_host) and if it succeeds, set res. else, res = None
		let mut res = None;
		for (url_base, url_base_host) in URL_PAIRS {
			res = reddit_short_head(path.clone(), true, url_base, url_base_host).await.ok();
			if let Some(res) = &res {
				if !res.status.is_client_error() {
					break;
				}
			}
		}
		res
	};

	let res = res.ok_or_else(|| "Unable to make HEAD request to Reddit.".to_string())?;
	let status = res.status.as_u16();
	let policy_error = res.headers.contains_key(header::RETRY_AFTER);

	match status {
		// If Reddit responds with a 2xx, then the path is already canonical.
		200..=299 => Ok(Some(path)),

		// If Reddit responds with a 301, then the path is redirected.
		301 => match res.headers.get(header::LOCATION) {
			Some(val) => {
				let Ok(original) = val.to_str() else {
					return Err("Unable to decode Location header.".to_string());
				};

				// We need to strip the .json suffix from the original path.
				// In addition, we want to remove share parameters.
				// Cut it off here instead of letting it propagate all the way
				// to main.rs
				let stripped_uri = original.strip_suffix(".json").unwrap_or(original).split('?').next().unwrap_or_default();

				// The reason why we now have to format_url, is because the new OAuth
				// endpoints seem to return full paths, instead of relative paths.
				// So we need to strip the .json suffix from the original path, and
				// also remove all Reddit domain parts with format_url.
				// Otherwise, it will literally redirect to Reddit.com.
				let uri = comments_logical_path(&format_url(stripped_uri));

				// Decrement tries and try again
				canonical_path(uri, tries - 1).await
			}
			None => Ok(None),
		},

		// If Reddit responds with anything other than 3xx (except for the 2xx and 301
		// as above), return a None.
		300..=399 => Ok(None),

		// Rate limiting
		429 => Err("Too many requests.".to_string()),

		// Special condition rate limiting - https://github.com/redlib-org/redlib/issues/229
		403 if policy_error => Err("Too many requests.".to_string()),

		_ => Ok(
			res.headers
				.get(header::LOCATION)
				.map(|val| percent_encode(val.as_bytes(), CONTROLS).to_string().trim_start_matches(REDDIT_URL_BASE).to_string()),
		),
	}
}

pub async fn proxy(req: HyperRequest<Body>, format: &str) -> Result<HyperResponse<Body>, String> {
	let mut url = format!("{format}?{}", req.uri().query().unwrap_or_default());

	// For each parameter in request
	for (name, value) in &req.params() {
		// Fill the parameter value in the url
		url = url.replace(&format!("{{{name}}}"), value);
	}

	let mut headers: Vec<(String, String)> = Vec::new();

	// Copy useful headers from original request
	for &key in &["Range", "If-Modified-Since", "Cache-Control"] {
		if let Some(value) = req.headers().get(key) {
			headers.push((key.to_string(), String::from_utf8_lossy(value.as_bytes()).to_string()));
		}
	}

	// Add User-Agent header of the currently spoofed device
	{
		let client = OAUTH_CLIENT.load_full();
		headers.push(("User-Agent".into(), client.user_agent().to_string()));
	}

	// This is needed or Reddit will redirect us to a /media landing page that just renders the image.
	headers.push(("Accept".into(), "*/*".into()));

	let res = http::stream(&url, &headers).await?;

	let (mut parts, body) = res.into_parts();

	for key in [
		"access-control-expose-headers",
		"server",
		"vary",
		"etag",
		"x-cdn",
		"x-cdn-client-region",
		"x-cdn-name",
		"x-cdn-server-region",
		"x-reddit-cdn",
		"x-reddit-video-features",
		"nel",
		"report-to",
	] {
		parts.headers.remove(key);
	}

	Ok(HyperResponse::from_parts(parts, body))
}

/// Fetch a server-rendered HTML fragment from Reddit's web frontend.
///
/// These are the fragments reddit.com loads for logged-out visitors, so they
/// are sent without the spoofed OAuth client's headers. A `cursor` turns the
/// request into the form POST used to expand a "more comments" placeholder.
pub async fn shreddit_fragment(path: String, cursor: Option<String>) -> Result<String, String> {
	if !path.starts_with(SHREDDIT_PATH_PREFIX) {
		return Err(format!("Refusing to fetch non-shreddit path | {path}"));
	}

	if let Some(body) = shreddit_request(&path, cursor.as_deref()).await? {
		return Ok(body);
	}

	warn!("Reddit rate limited the shreddit session, starting a new one");
	clear_cookies();
	shreddit_request(&path, cursor.as_deref())
		.await?
		.ok_or_else(|| format!("Reddit is rate limiting comment requests. Try refreshing in a few seconds. | {path}"))
}

/// Returns `Ok(None)` when Reddit rate limits or challenges the session.
async fn shreddit_request(path: &str, cursor: Option<&str>) -> Result<Option<String>, String> {
	let url = format!("{ALTERNATIVE_REDDIT_URL_BASE}{path}");

	let mut headers: Vec<(String, String)> = Vec::new();

	// The shreddit endpoints are served by the web frontend, but Reddit answers
	// 403 with its HTML block page unless the request carries the anonymous app
	// OAuth token (and the spoofed Android device headers that go with it).
	{
		let client = OAUTH_CLIENT.load_full();
		for (key, value) in client.headers_map.clone() {
			if key.eq_ignore_ascii_case("content-type") {
				continue;
			}
			headers.push((key, value));
		}
	}
	headers.push(("Accept".into(), SHREDDIT_PARTIAL_ACCEPT.into()));

	if let Some(cookies) = cookie_header() {
		headers.push(("Cookie".into(), cookies));
	}

	let (method, body) = match cursor {
		Some(cursor) => (
			"POST",
			Some(url::form_urlencoded::Serializer::new(String::new()).append_pair("cursor", cursor).finish().into_bytes()),
		),
		None => ("GET", None),
	};

	if body.is_some() {
		headers.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
	}

	let reply = http::request(method, &url, &headers, body)
		.await
		.map_err(|e| format!("{e} | {path}"))?;

	store_cookies(&reply.headers);

	if reply.status == hyper::StatusCode::TOO_MANY_REQUESTS {
		return Ok(None);
	}
	if !reply.status.is_success() {
		return Err(format!("Reddit returned {} | {path}", reply.status));
	}

	let remaining = reply
		.headers
		.get("x-ratelimit-remaining")
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<f32>().ok());
	if remaining.is_some_and(|remaining| remaining < SHREDDIT_RATELIMIT_ROTATE_BELOW) {
		trace!("Shreddit session rate limit is low ({remaining:?} remaining), starting a new one");
		clear_cookies();
	}

	let body = reply.text();
	Ok((!body.contains(SHREDDIT_CHALLENGE_TITLE)).then_some(body))
}

/// The logged-out shreddit session cookie, as a `Cookie` header value.
fn cookie_header() -> Option<String> {
	let session = SHREDDIT_SESSION.lock().ok()?;
	if session.is_empty() {
		return None;
	}
	Some(session.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("; "))
}

fn store_cookies(headers: &hyper::HeaderMap) {
	let Ok(mut session) = SHREDDIT_SESSION.lock() else {
		return;
	};
	for value in headers.get_all(hyper::header::SET_COOKIE) {
		let Ok(value) = value.to_str() else { continue };
		let Some((pair, _attributes)) = value.split_once(';') else { continue };
		let Some((name, value)) = pair.split_once('=') else { continue };
		session.insert(name.trim().to_string(), value.trim().to_string());
	}
}

fn clear_cookies() {
	if let Ok(mut session) = SHREDDIT_SESSION.lock() {
		session.clear();
	}
}

/// Makes a GET request to Reddit at `path`. By default, this will honor HTTP
/// 3xx codes Reddit returns and will automatically redirect.
fn reddit_get(path: String, quarantine: bool) -> Boxed<Result<Reply, String>> {
	request("GET", path, true, quarantine, REDDIT_URL_BASE, REDDIT_URL_BASE_HOST)
}

/// Makes a HEAD request to Reddit at `path`, using the short URL base. This will
/// not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<Reply, String>> {
	request("HEAD", path, false, quarantine, base_path, host)
}

/// Makes a request to Reddit. If `redirect` is `true`, this will recurse on the
/// URL that Reddit provides in the Location HTTP header in its response.
fn request(method: &'static str, path: String, redirect: bool, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<Reply, String>> {
	// Keep the logical path unchanged for caches and diagnostics. Reddit blocks
	// literal anonymous comments routes, so only rewrite the outgoing URL.
	let transport_path = comments_transport_path(&path);
	let url = format!("{base_path}{transport_path}");

	let mut headers: Vec<(String, String)> = vec![
		("Host".into(), host.into()),
		(
			"Cookie".into(),
			if quarantine {
				"_options=%7B%22pref_quarantine_optin%22%3A%20true%2C%20%22pref_gated_sr_optin%22%3A%20true%7D".into()
			} else {
				"".into()
			},
		),
	];

	{
		let client = OAUTH_CLIENT.load_full();
		for (key, value) in client.headers_map.clone() {
			headers.push((key, value));
		}
	}

	// shuffle headers: https://github.com/redlib-org/redlib/issues/324
	fastrand::shuffle(&mut headers);

	async move {
		match http::request(method, &url, &headers, None).await {
			Ok(reply) => {
				// Reddit may respond with a 3xx. Decide whether or not to
				// redirect based on caller params.
				if reply.status.is_redirection() {
					if !redirect {
						return Ok(reply);
					};
					let location_header = reply.headers.get(header::LOCATION);
					if location_header.and_then(|h| h.to_str().ok()) == Some(ALTERNATIVE_REDDIT_URL_BASE) {
						return Err("Reddit response was invalid".to_string());
					}
					return request(
						method,
						location_header
							.map(|val| {
								// We need to make adjustments to the URI
								// we get back from Reddit. Namely, we
								// must:
								//
								//     1. Remove the authority (e.g.
								//     https://www.reddit.com) that may be
								//     present, so that we recurse on the
								//     path (and query parameters) as
								//     required.
								//
								//     2. Percent-encode the path.
								let new_path = percent_encode(val.as_bytes(), CONTROLS)
									.to_string()
									.trim_start_matches(REDDIT_URL_BASE)
									.trim_start_matches(ALTERNATIVE_REDDIT_URL_BASE)
									.to_string();
								format!("{new_path}{}raw_json=1", if new_path.contains('?') { "&" } else { "?" })
							})
							.unwrap_or_default()
							.to_string(),
						true,
						quarantine,
						base_path,
						host,
					)
					.await;
				};

				Ok(reply)
			}
			Err(e) => {
				dbg_msg!("{method} {REDDIT_URL_BASE}{path}: {}", e);

				Err(e)
			}
		}
	}
	.boxed()
}

/// Encode one separator in comments API routes to avoid Reddit's anonymous
/// request block while leaving every other path and the query string intact.
fn comments_transport_path(path: &str) -> Cow<'_, str> {
	let base = path.split('?').next().unwrap_or(path);
	let (separator, tail) = if let Some(tail) = base.strip_prefix("/comments/") {
		("/comments".len(), tail)
	} else if let Some(scoped) = base.strip_prefix("/r/") {
		let Some((subreddit, rest)) = scoped.split_once('/') else {
			return Cow::Borrowed(path);
		};
		let Some(tail) = rest.strip_prefix("comments/") else {
			return Cow::Borrowed(path);
		};
		if subreddit.is_empty() {
			return Cow::Borrowed(path);
		}
		("/r/".len() + subreddit.len(), tail)
	} else {
		return Cow::Borrowed(path);
	};

	let tail = tail.strip_suffix(".json").unwrap_or(tail).trim_end_matches('/');
	let mut segments = tail.split('/');
	let Some(post_id) = segments.next() else {
		return Cow::Borrowed(path);
	};
	let _slug = segments.next();
	let comment_id = segments.next();

	if post_id.is_empty() || comment_id.is_some_and(str::is_empty) || segments.next().is_some() {
		return Cow::Borrowed(path);
	}

	Cow::Owned(format!("{}%2F{}", &path[..separator], &path[separator + 1..]))
}

/// Remove the comments-route separator encoding if Reddit reflects our
/// transport-only rewrite in a redirect.
fn comments_logical_path(path: &str) -> String {
	let path = path.replace("/comments%2F", "/comments/").replace("/comments%2f", "/comments/");

	let Some(scoped) = path.strip_prefix("/r/") else {
		return path;
	};
	let Some((subreddit, tail)) = scoped.split_once("%2Fcomments/").or_else(|| scoped.split_once("%2fcomments/")) else {
		return path;
	};
	if subreddit.is_empty() || subreddit.contains('/') {
		return path;
	}
	format!("/r/{subreddit}/comments/{tail}")
}

/// Make a request to a Reddit API and parse the JSON response.
#[cached(size = 100, time = 30, result = true)]
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	// First, handle rolling over the OAUTH_CLIENT if need be.
	let current_rate_limit = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	let is_rolling_over = OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst);
	if current_rate_limit < 10 && !is_rolling_over {
		warn!("Rate limit {current_rate_limit} is low. Spawning force_refresh_token()");
		tokio::spawn(force_refresh_token());
	}
	#[allow(deprecated)] // try_update is newer than the project's MSRV.
	let _ = OAUTH_RATELIMIT_REMAINING.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| remaining.checked_sub(1));

	let reply = reddit_get(path.clone(), quarantine).await?;
	let status = reply.status;

	let reset: Option<String> = if let (Some(remaining), Some(reset), Some(used)) = (
		reply.headers.get("x-ratelimit-remaining").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
		reply.headers.get("x-ratelimit-reset").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
		reply.headers.get("x-ratelimit-used").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
	) {
		trace!(
			"Ratelimit remaining: Header says {remaining}, we have {current_rate_limit}. Resets in {reset}. Rollover: {}. Ratelimit used: {used}",
			if is_rolling_over { "yes" } else { "no" },
		);

		// If can parse remaining as a float, round to a u16 and save
		if let Ok(val) = remaining.parse::<f32>() {
			OAUTH_RATELIMIT_REMAINING.store(val.round() as u16, Ordering::SeqCst);
		}

		Some(reset)
	} else {
		None
	};

	if reply.body.is_empty() {
		// Rate limited, so spawn a force_refresh_token()
		tokio::spawn(force_refresh_token());
		return match reset {
			Some(val) => Err(format!(
				"Reddit rate limit exceeded. Try refreshing in a few seconds.\
				 Rate limit will reset in: {val}"
			)),
			None => Err("Reddit rate limit exceeded".to_string()),
		};
	}

	// Parse the response from Reddit as JSON
	match serde_json::from_slice::<Value>(&reply.body) {
		Ok(json) => {
			// If user is suspended
			if let Some(data) = json.get("data") {
				if let Some(is_suspended) = data.get("is_suspended").and_then(Value::as_bool) {
					if is_suspended {
						return Err("suspended".into());
					}
				}
			}

			// If Reddit returned an error
			if json["error"].is_i64() {
				// OAuth token has expired; http status 401
				if json["message"] == "Unauthorized" {
					error!("Forcing a token refresh");
					let () = force_refresh_token().await;
					return Err("OAuth token has expired. Please refresh the page!".to_string());
				}

				// Handle quarantined
				if json["reason"] == "quarantined" {
					return Err("quarantined".into());
				}
				// Handle gated
				if json["reason"] == "gated" {
					return Err("gated".into());
				}
				// Handle private subs
				if json["reason"] == "private" {
					return Err("private".into());
				}
				// Handle banned subs
				if json["reason"] == "banned" {
					return Err("banned".into());
				}

				Err(format!("Reddit error {} \"{}\": {} | {path}", json["error"], json["reason"], json["message"]))
			} else {
				Ok(json)
			}
		}
		Err(e) => {
			error!("Got an invalid response from reddit {e}. Status code: {status}");
			if status.is_server_error() {
				Err("Reddit is having issues, check if there's an outage".to_string())
			} else if status == hyper::StatusCode::FORBIDDEN {
				// The body was Reddit's HTML block page rather than JSON.
				Err(format!(
					"Reddit refused this request (403 Forbidden). This endpoint is not available to anonymous clients, so Redlib cannot load it right now. | {path}"
				))
			} else {
				Err(format!("Failed to parse page JSON data: {e} | {path}"))
			}
		}
	}
}

async fn self_check(sub: &str) -> Result<(), String> {
	let query = format!("/r/{sub}/hot.json?&raw_json=1");

	match Post::fetch(&query, true).await {
		Ok(_) => Ok(()),
		Err(e) => Err(e),
	}
}

pub async fn rate_limit_check() -> Result<(), String> {
	// First, test the Oauth client: we can perform a rate limit check if the OAuth backend is MobileSpoof; if GenericWeb, we skip the check.
	if matches!(OAUTH_CLIENT.load().backend, OauthBackendImpl::GenericWeb(_)) {
		warn!("[⚠️] Cannot perform rate limit check, running as GenericWeb. Skipping check.");
		return Ok(());
	}

	// First, check a subreddit.
	self_check("reddit").await?;
	// This will reduce the rate limit to 99. Assert this check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 1 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}
	// Now, we switch out the OAuth client.
	// This checks for the IP rate limit association.
	force_refresh_token().await;
	// Now, check a new sub to break cache.
	self_check("rust").await?;
	// Again, assert the rate limit check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 2 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use {crate::config::get_setting, sealed_test::prelude::*};

	const POPULAR_URL: &str = "/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL";

	#[test]
	fn test_comments_transport_path_encodes_scoped_and_short_routes() {
		for (path, expected) in [
			(
				"/r/rust/comments/18t5968/title/.json?sort=new&raw_json=1",
				"/r/rust%2Fcomments/18t5968/title/.json?sort=new&raw_json=1",
			),
			("/r/rust/comments/18t5968/_/kfbqlbc.json?context=3", "/r/rust%2Fcomments/18t5968/_/kfbqlbc.json?context=3"),
			("/comments/18t5968.json?raw_json=1", "/comments%2F18t5968.json?raw_json=1"),
			("/comments/18t5968/_/kfbqlbc.json?context=3", "/comments%2F18t5968/_/kfbqlbc.json?context=3"),
			("/r/comments/comments/18t5968.json", "/r/comments%2Fcomments/18t5968.json"),
		] {
			assert_eq!(comments_transport_path(path), expected);
			assert_eq!(comments_transport_path(expected), expected, "encoding must be idempotent");
		}
	}

	#[test]
	fn test_comments_transport_path_preserves_other_routes_and_queries() {
		for path in [
			"/r/popular/hot.json?after=t3_abc",
			"/r/rust/comments.json",
			"/r/rust/commentsXYZ/abc.json",
			"/r/rust/wiki/comments/abc.json",
			"/r//comments/abc.json",
			"/comments/.json",
			"/comments.json",
			"/user/example/comments.json",
			"/user/example/comments/abc.json",
			"/search.json?q=/r/rust/comments/abc",
			"https://example.com/r/rust/comments/abc.json",
			"//example.com/r/rust/comments/abc.json",
		] {
			assert!(matches!(comments_transport_path(path), Cow::Borrowed(_)), "{path}");
		}

		let path = "/r/rust/comments/abc.json?sort=new&context=3&q=a%2Bb&empty=&sort=top";
		assert_eq!(comments_transport_path(path), "/r/rust%2Fcomments/abc.json?sort=new&context=3&q=a%2Bb&empty=&sort=top");
	}

	#[test]
	fn test_comments_logical_path_removes_transport_encoding() {
		for (path, expected) in [
			("/r/rust%2Fcomments/18t5968/title/kfbqlbc/", "/r/rust/comments/18t5968/title/kfbqlbc/"),
			("/r/rust%2fcomments/18t5968/title/kfbqlbc/", "/r/rust/comments/18t5968/title/kfbqlbc/"),
			("/comments%2F18t5968/title/", "/comments/18t5968/title/"),
			("/r/rust%2Fwiki/page", "/r/rust%2Fwiki/page"),
		] {
			assert_eq!(comments_logical_path(path), expected);
		}
	}

	#[tokio::test]
	async fn test_shreddit_fragment_rejects_other_paths() {
		for path in ["/api/v1/me", "https://evil.example/svc/shreddit/x", "/svc/shreddit"] {
			let error = shreddit_fragment(path.to_string(), None).await.unwrap_err();
			assert!(error.starts_with("Refusing to fetch non-shreddit path"), "{path}: {error}");
		}
	}
	#[tokio::test(flavor = "multi_thread")]
	async fn test_rate_limit_check() {
		rate_limit_check().await.unwrap();
	}

	#[test]
	#[sealed_test(env = [("REDLIB_DEFAULT_SUBSCRIPTIONS", "rust")])]
	fn test_default_subscriptions() {
		tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
			let subscriptions = get_setting("REDLIB_DEFAULT_SUBSCRIPTIONS");
			assert!(subscriptions.is_some());

			// check rate limit
			rate_limit_check().await.unwrap();
		});
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_localization_popular() {
		let val = json(POPULAR_URL.to_string(), false).await.unwrap();
		assert_eq!("GLOBAL", val["data"]["geo_filter"].as_str().unwrap());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_obfuscated_share_link() {
		let share_link = "/r/rust/s/kPgq8WNHRK".into();
		// Correct link without share parameters
		let canonical_link = "/r/rust/comments/18t5968/why_use_tuple_struct_over_standard_struct/kfbqlbc/".into();
		assert_eq!(canonical_path(share_link, 3).await, Ok(Some(canonical_link)));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_private_sub() {
		let link = json("/r/suicide/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("private".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_banned_sub() {
		let link = json("/r/aaa/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("banned".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_gated_sub() {
		// quarantine to false to specifically catch when we _don't_ catch it
		let link = json("/r/drugs/about.json?raw_json=1".into(), false).await;
		assert!(link.is_err());
		assert_eq!(link, Err("gated".into()));
	}
}
