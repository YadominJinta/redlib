use crate::{
	client::{OAUTH_CLIENT, OAUTH_IS_ROLLING_OVER, OAUTH_RATELIMIT_REMAINING},
	oauth_resources::ANDROID_APP_VERSION_LIST,
};
use base64::{engine::general_purpose, Engine as _};
use log::{error, info, trace, warn};
use serde_json::json;
use std::{collections::HashMap, sync::atomic::Ordering, time::Duration};
use tegen::tegen::TextGenerator;
use tokio::time::{error::Elapsed, timeout};

const REDDIT_ANDROID_OAUTH_CLIENT_ID: &str = "ohXpoqrZYub1kg";

const AUTH_ENDPOINT: &str = "https://www.reddit.com";

const OAUTH_TIMEOUT: Duration = Duration::from_secs(5);

// Response from OAuth backend authentication
#[derive(Debug, Clone)]
pub struct OauthResponse {
	pub token: String,
	pub expires_in: u64,
	pub additional_headers: HashMap<String, String>,
}

// Trait for OAuth backend implementations
trait OauthBackend: Send + Sync {
	fn authenticate(&mut self) -> impl std::future::Future<Output = Result<OauthResponse, AuthError>> + Send;
	fn user_agent(&self) -> &str;
	fn get_headers(&self) -> HashMap<String, String>;
}

// OAuth backend implementations
#[derive(Debug, Clone)]
pub(crate) enum OauthBackendImpl {
	MobileSpoof(MobileSpoofAuth),
	GenericWeb(GenericWebAuth),
}

impl OauthBackend for OauthBackendImpl {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.authenticate().await,
			OauthBackendImpl::GenericWeb(backend) => backend.authenticate().await,
		}
	}

	fn user_agent(&self) -> &str {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.user_agent(),
			OauthBackendImpl::GenericWeb(backend) => backend.user_agent(),
		}
	}

	fn get_headers(&self) -> HashMap<String, String> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.get_headers(),
			OauthBackendImpl::GenericWeb(backend) => backend.get_headers(),
		}
	}
}

// Spoofed client for Android devices
#[derive(Debug, Clone)]
pub struct Oauth {
	pub(crate) headers_map: HashMap<String, String>,
	expires_in: u64,
	pub(crate) backend: OauthBackendImpl,
}

impl Oauth {
	/// Create a new OAuth client
	pub(crate) async fn new() -> Self {
		Self::new_with_session(HashMap::new()).await
	}

	/// Headers that identify this logged-out session. Reddit hands the `x-reddit-loid`
	/// out once and expects it back on later requests, so a token rollover has to keep
	/// it: a client whose loid changes on every refresh has no history at all.
	pub(crate) fn session_headers(&self) -> HashMap<String, String> {
		self.headers_map
			.iter()
			.filter(|(key, _)| key.eq_ignore_ascii_case("x-reddit-loid") || key.eq_ignore_ascii_case("x-reddit-session"))
			.map(|(key, value)| (key.clone(), value.clone()))
			.collect()
	}

	/// Create a new OAuth client, reusing the session headers of a previous one.
	pub(crate) async fn new_with_session(session: HashMap<String, String>) -> Self {
		// Try MobileSpoofAuth first, then fall back to GenericWebAuth
		let mut failure_count = 0;
		let mut backend = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(session.clone()));

		loop {
			let attempt = Self::new_with_timeout_with_backend(backend.clone()).await;
			match attempt {
				Ok(Ok(oauth)) => {
					info!("[✅] Successfully created OAuth client");
					return oauth;
				}
				Ok(Err(e)) => {
					error!(
						"[⛔] Failed to create OAuth client: {}. Retrying in 5 seconds...",
						match e {
							AuthError::Http(error) => error,
							AuthError::SerdeDeserialize(error) => error.to_string(),
							AuthError::Field((value, error)) => format!("{error}\n{value}"),
						}
					);
				}
				Err(_) => {
					error!("[⛔] Failed to create OAuth client before timeout. Retrying in 5 seconds...");
				}
			}

			failure_count += 1;

			// Switch to GenericWeb after 5 failures with MobileSpoof
			if matches!(backend, OauthBackendImpl::MobileSpoof(_)) && failure_count >= 5 {
				warn!("[🔄] MobileSpoofAuth failed 5 times. Falling back to GenericWebAuth...");
				backend = OauthBackendImpl::GenericWeb(GenericWebAuth::new(session.clone()));
			}

			// Crash after 10 total failures
			if failure_count >= 10 {
				error!("[⛔] Failed to create OAuth client (mobile + generic)");
				std::process::exit(1);
			}

			tokio::time::sleep(OAUTH_TIMEOUT).await;
		}
	}

	async fn new_with_timeout_with_backend(mut backend: OauthBackendImpl) -> Result<Result<Self, AuthError>, Elapsed> {
		timeout(OAUTH_TIMEOUT, async move {
			let response = backend.authenticate().await?;

			// Build headers_map from backend headers + Authorization header
			let mut headers_map = backend.get_headers();
			headers_map.insert("Authorization".to_owned(), format!("Bearer {}", response.token));
			headers_map.extend(response.additional_headers);

			Ok(Self {
				headers_map,
				expires_in: response.expires_in,
				backend,
			})
		})
		.await
	}

	pub fn user_agent(&self) -> &str {
		self.backend.user_agent()
	}
}

#[derive(Debug)]
enum AuthError {
	Http(String),
	SerdeDeserialize(serde_json::Error),
	Field((serde_json::Value, &'static str)),
}

impl From<serde_json::Error> for AuthError {
	fn from(err: serde_json::Error) -> Self {
		AuthError::SerdeDeserialize(err)
	}
}

pub async fn token_daemon() {
	// Monitor for refreshing token
	loop {
		// Get expiry time - be sure to not hold the read lock
		let expires_in = { OAUTH_CLIENT.load_full().expires_in };

		// sleep for the expiry time minus 2 minutes
		let duration = Duration::from_secs(expires_in - 120);

		info!("[⏳] Waiting for {duration:?} seconds before refreshing OAuth token...");

		tokio::time::sleep(duration).await;

		info!("[⌛] {duration:?} Elapsed! Refreshing OAuth token...");

		// Refresh token - in its own scope
		{
			force_refresh_token().await;
		}
	}
}

pub async fn force_refresh_token() {
	if OAUTH_IS_ROLLING_OVER.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		trace!("Skipping refresh token roll over, already in progress");
		return;
	}

	trace!("Rolling over refresh token. Current rate limit: {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst));
	// Keep the loid/session of the client being replaced: the token is new, the
	// device is supposed to be the same one.
	let session = OAUTH_CLIENT.load_full().session_headers();
	let new_client = Oauth::new_with_session(session).await;
	OAUTH_CLIENT.swap(new_client.into());
	OAUTH_RATELIMIT_REMAINING.store(99, Ordering::SeqCst);
	OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
}

#[derive(Debug, Clone, Default)]
struct Device {
	oauth_id: String,
	initial_headers: HashMap<String, String>,
	headers: HashMap<String, String>,
	user_agent: String,
}

// MobileSpoofAuth backend - spoofs an Android mobile device
#[derive(Debug, Clone)]
pub struct MobileSpoofAuth {
	device: Device,
	additional_headers: HashMap<String, String>,
}

impl MobileSpoofAuth {
	fn new(session: HashMap<String, String>) -> Self {
		Self {
			device: Device::new(),
			additional_headers: session,
		}
	}
}

impl OauthBackend for MobileSpoofAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = format!("{AUTH_ENDPOINT}/auth/v2/oauth/access-token/loid");

		let mut headers: Vec<(String, String)> = self.device.initial_headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

		// Set up HTTP Basic Auth - basically just the const OAuth ID's with no password,
		// Base64-encoded. https://en.wikipedia.org/wiki/Basic_access_authentication
		// This could be constant, but I don't think it's worth it. OAuth ID's can change
		// over time and we want to be flexible.
		let auth = general_purpose::STANDARD.encode(format!("{}:", self.device.oauth_id));
		headers.push(("Authorization".into(), format!("Basic {auth}")));
		headers.push(("Content-Type".into(), "application/json; charset=UTF-8".into()));

		// Set JSON body. I couldn't tell you what this means. But that's what the client sends
		let body = json!({
				"scopes": ["*","email", "pii"]
		})
		.to_string();

		trace!("Sending token request to {url}...");

		// Send request
		let resp = crate::http::request("POST", &url, &headers, Some(body.into_bytes()))
			.await
			.map_err(AuthError::Http)?;

		trace!("Received response with status {} and length {:?}", resp.status, resp.headers.get("content-length"));
		trace!("OAuth headers: {:#?}", resp.headers);

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers.get("x-reddit-loid") {
			self.additional_headers.insert("x-reddit-loid".to_owned(), header.to_str().unwrap_or_default().to_string());
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers.get("x-reddit-session") {
			self.additional_headers.insert("x-reddit-session".to_owned(), header.to_str().unwrap_or_default().to_string());
		}

		trace!("Serializing response...");

		// Serialize response
		let json: serde_json::Value = serde_json::from_slice(&resp.body).map_err(AuthError::SerdeDeserialize)?;

		trace!("Accessing relevant fields...");

		// Save token and expiry
		let token = json
			.get("access_token")
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token")))?
			.as_str()
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token: as_str")))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in")))?
			.as_u64()
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in: as_u64")))?;

		info!("[✅] Success - Retrieved token \"{}...\", expires in {}", &token[..32], expires_in);

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.device.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		let mut headers = self.device.headers.clone();
		headers.extend(self.additional_headers.clone());
		headers
	}
}

// GenericWebAuth backend - simple web-based authentication
#[derive(Debug, Clone)]
pub struct GenericWebAuth {
	device_id: String,
	user_agent: String,
	additional_headers: HashMap<String, String>,
}

impl GenericWebAuth {
	fn new(session: HashMap<String, String>) -> Self {
		// Generate random 20-character alphanumeric device_id
		let device_id: String = (0..20)
			.map(|_| {
				let idx = fastrand::usize(..62);
				let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
				chars[idx] as char
			})
			.collect();

		info!("[🔄] Using GenericWebAuth with device_id: \"{device_id}\"");

		Self {
			device_id,
			user_agent: fake_user_agent::get_rua().to_owned(),
			additional_headers: session,
		}
	}
}

impl OauthBackend for GenericWebAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = "https://www.reddit.com/api/v1/access_token";

		// Add minimal headers
		let mut headers: Vec<(String, String)> = vec![
			("Host".into(), "www.reddit.com".into()),
			("User-Agent".into(), self.user_agent.clone()),
			("Accept".into(), "*/*".into()),
			("Accept-Language".into(), "en-US,en;q=0.5".into()),
			("Authorization".into(), "Basic M1hmQkpXbGlIdnFBQ25YcmZJWWxMdzo=".into()),
			("Content-Type".into(), "application/x-www-form-urlencoded".into()),
			("Sec-GPC".into(), "1".into()),
			("Connection".into(), "keep-alive".into()),
		];

		// Set up form body
		let body_str = format!("grant_type=https%3A%2F%2Foauth.reddit.com%2Fgrants%2Finstalled_client&device_id={}", self.device_id);

		trace!("Sending GenericWebAuth token request to {url}...");

		// Send request
		let resp = crate::http::request("POST", url, &headers, Some(body_str.into_bytes()))
			.await
			.map_err(AuthError::Http)?;

		headers.clear();

		trace!("Received response with status {} and length {:?}", resp.status, resp.headers.get("content-length"));
		trace!("GenericWebAuth headers: {:#?}", resp.headers);

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers.get("x-reddit-loid") {
			self.additional_headers.insert("x-reddit-loid".to_owned(), header.to_str().unwrap_or_default().to_string());
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers.get("x-reddit-session") {
			self.additional_headers.insert("x-reddit-session".to_owned(), header.to_str().unwrap_or_default().to_string());
		}

		trace!("Serializing GenericWebAuth response...");

		// Serialize response
		let json: serde_json::Value = serde_json::from_slice(&resp.body).map_err(AuthError::SerdeDeserialize)?;

		trace!("Accessing relevant fields...");

		// Parse response - access_token, token_type, device_id, expires_in, scope
		let token = json
			.get("access_token")
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token")))?
			.as_str()
			.ok_or_else(|| AuthError::Field((json.clone(), "access_token: as_str")))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in")))?
			.as_u64()
			.ok_or_else(|| AuthError::Field((json.clone(), "expires_in: as_u64")))?;

		info!(
			"[✅] GenericWebAuth success - Retrieved token \"{}...\", expires in {}",
			&token[..32.min(token.len())],
			expires_in
		);

		// Insert a few necessary headers
		self.additional_headers.insert("Origin".to_owned(), "https://www.reddit.com".to_owned());
		self.additional_headers.insert("User-Agent".to_owned(), self.user_agent.to_owned());

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		self.additional_headers.clone()
	}
}

impl Device {
	fn android() -> Self {
		// Generate uuid. The app uses `UUID.randomUUID()` for the install id and sends
		// the same value as both `x-reddit-device-id` and `Client-Vendor-ID`.
		let uuid = uuid::Uuid::new_v4().to_string();

		// `Device-Name` is `Build.MANUFACTURER;Build.MODEL`, and the
		// dpr/width/height triplet is that screen's values in dp (u6o.java).
		let (device_name, dpr, width, height, first_release) = choose_device();

		// Generate a user-agent that matches the shipped build this client models.
		// `R.string.fmt_user_agent` is `Reddit/Version %1$s/Build %2$s/Android %3$s`:
		// versionName, versionCode and `Build.VERSION.RELEASE`. The release can only
		// be one the chosen model actually ran, and the app needs 12L (minSdk 32).
		let android_app_version = choose(ANDROID_APP_VERSION_LIST).to_string();
		let android_version = fastrand::u8(first_release.max(12)..=16).to_string();

		let android_user_agent = format!("Reddit/{android_app_version}/Android {android_version}");

		// `x-reddit-qos` is a key=value pair, not a bare number (uh80.java).
		let qos = format!("down-rate-mbps={:.3}", fastrand::u32(5_000..=60_000) as f32 / 1000.0);

		let codecs = TextGenerator::new().generate("available-codecs=video/avc, video/hevc{, video/x-vnd.on2.vp9|}");

		// Android device headers, as `NetworkHeaderInterceptor` adds them.
		let headers: HashMap<String, String> = HashMap::from([
			("User-Agent".into(), android_user_agent.clone()),
			("Client-Vendor-ID".into(), uuid.clone()),
			("x-reddit-device-id".into(), uuid.clone()),
			("Device-Name".into(), device_name.to_string()),
			("x-reddit-dpr".into(), dpr.to_string()),
			("x-reddit-width".into(), width.to_string()),
			("x-reddit-height".into(), height.to_string()),
			("x-reddit-compression".into(), "1".into()),
			("X-Reddit-QoS".into(), qos),
			("X-Reddit-Media-Codecs".into(), codecs),
			("Accept-Language".into(), "en-US,en;q=0.9".into()),
		]);

		info!("[🔄] Spoofing Android client with headers: {headers:?}, uuid: \"{uuid}\", and OAuth ID \"{REDDIT_ANDROID_OAUTH_CLIENT_ID}\"");

		Self {
			oauth_id: REDDIT_ANDROID_OAUTH_CLIENT_ID.to_string(),
			headers: headers.clone(),
			initial_headers: headers,
			user_agent: android_user_agent,
		}
	}
	fn new() -> Self {
		// See https://github.com/redlib-org/redlib/issues/8
		Self::android()
	}
}

fn choose<T: Copy>(list: &[T]) -> T {
	*fastrand::choose_multiple(list.iter(), 1)[0]
}

/// Real device profiles: (MANUFACTURER;MODEL, density, width dp, height dp, first Android release).
///
/// The screen headers are only plausible together, so they are chosen as one
/// profile instead of three independent random numbers. `first Android release`
/// keeps the User-Agent's `Android <release>` at or above the version the model
/// shipped with: a Pixel 7 never ran Android 12, and that pairing is exactly the
/// kind of combination a real client never produces.
const ANDROID_DEVICES: &[(&str, &str, u32, u32, u8)] = &[
	("Google;Pixel 8", "2.625", 412, 915, 14),
	("Google;Pixel 7", "2.625", 412, 892, 13),
	("Google;Pixel 7a", "2.625", 412, 892, 13),
	("samsung;SM-S918B", "2.8125", 384, 855, 13),
	("samsung;SM-A546B", "2.625", 384, 866, 13),
	("OnePlus;CPH2449", "3.0", 360, 800, 13),
	("motorola;moto g84 5G", "2.625", 412, 915, 13),
	("Xiaomi;2312DRA50G", "2.75", 405, 900, 13),
];

fn choose_device() -> (&'static str, &'static str, u32, u32, u8) {
	ANDROID_DEVICES[fastrand::usize(..ANDROID_DEVICES.len())]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	async fn test_mobile_spoof_backend() {
		// Test MobileSpoofAuth backend specifically
		let mut backend = MobileSpoofAuth::new(HashMap::new());
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
		assert!(!backend.get_headers().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "todolol"]
	async fn test_generic_web_backend() {
		// Test GenericWebAuth backend specifically
		let mut backend = GenericWebAuth::new(HashMap::new());
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_client() {
		// Integration test - tests the overall Oauth client
		assert!(OAUTH_CLIENT.load_full().headers_map.contains_key("Authorization"));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_client_refresh() {
		force_refresh_token().await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_token_exists() {
		let client = OAUTH_CLIENT.load_full();
		let auth_header = client.headers_map.get("Authorization").unwrap();
		assert!(auth_header.starts_with("Bearer "));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_headers_len() {
		assert!(OAUTH_CLIENT.load_full().headers_map.len() >= 3);
	}

	#[test]
	fn test_creating_device() {
		Device::new();
	}

	#[test]
	fn test_device_profiles_stay_plausible() {
		for (name, dpr, width, height, first_release) in ANDROID_DEVICES {
			assert!(name.contains(';'), "{name} must be MANUFACTURER;MODEL");
			assert!(*width >= 320 && *height >= 480, "{name} screen is implausible");
			assert!(dpr.parse::<f32>().is_ok(), "{name} density is not a number");
			// A model cannot be sent with an Android release it never shipped with.
			assert!(*first_release >= 12, "{name} predates the app's minSdk");
			assert!(*first_release <= 16, "{name} ships with an unknown release");
		}
	}

	#[test]
	fn test_creating_backends() {
		// Test that both backends can be created
		MobileSpoofAuth::new(HashMap::new());
		GenericWebAuth::new(HashMap::new());
	}

	#[test]
	fn test_session_headers_survive_a_rollover() {
		// The loid identifies the device to Reddit, so a token refresh must reuse it.
		let mut headers_map = HashMap::new();
		headers_map.insert("x-reddit-loid".to_string(), "loid-value".to_string());
		headers_map.insert("x-reddit-session".to_string(), "session-value".to_string());
		headers_map.insert("Authorization".to_string(), "Bearer token".to_string());

		let previous = Oauth {
			headers_map,
			expires_in: 1,
			backend: OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(HashMap::new())),
		};

		let session = previous.session_headers();
		assert_eq!(session.len(), 2);
		assert_eq!(session.get("x-reddit-loid").map(String::as_str), Some("loid-value"));
		assert_eq!(session.get("x-reddit-session").map(String::as_str), Some("session-value"));
		assert!(!session.contains_key("Authorization"));
	}
}
