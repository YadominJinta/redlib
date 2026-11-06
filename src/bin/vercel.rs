//! Vercel entry point.
//!
//! Vercel's Rust runtime does not require a special handler type: it starts the
//! process with `VERCEL_DEV_PORT` and a Unix socket in `VERCEL_IPC_PATH`, waits
//! for the process to serve HTTP on `127.0.0.1:<port>` and to announce itself
//! with a NUL-terminated JSON message, then proxies invocations to it.
//!
//! That is exactly what redlib already is — an HTTP server — so this binary only
//! adds the announcement and reuses the very same route table as `redlib`.
//!
//! One process serves many invocations (it is reused while warm), so the shared
//! OAuth client, the response caches and the token refresh loop keep working as
//! they do on a long-running host.

use std::io::Write;
use std::sync::LazyLock;
use std::time::Instant;

use log::{info, warn};
use redlib::client::{rate_limit_check, OAUTH_CLIENT};
use redlib::{app, config, instance_info};

#[tokio::main]
async fn main() {
	_ = dotenvy::dotenv();
	pretty_env_logger::init();

	let started = Instant::now();

	let port: u16 = std::env::var("VERCEL_DEV_PORT").ok().and_then(|port| port.parse().ok()).unwrap_or(3000);
	let address = format!("127.0.0.1:{port}");

	// Warm the statics up front so the first invocation does not pay for the
	// token request; Vercel bills cold-start time but reuses the process after.
	info!("Evaluating config.");
	LazyLock::force(&config::CONFIG);
	info!("Evaluating instance info.");
	LazyLock::force(&instance_info::INSTANCE_INFO);
	info!("Creating OAUTH client.");
	LazyLock::force(&OAUTH_CLIENT);

	// The startup check costs four requests; on a serverless host every cold
	// start pays for them, so keep it behind an opt-in.
	if std::env::var("REDLIB_SERVERLESS_RATE_CHECK").is_ok_and(|value| value == "on") {
		match rate_limit_check().await {
			Ok(()) => info!("[✅] Rate limit check passed"),
			Err(error) => warn!("Rate limit check failed: {error}"),
		}
	}

	let server = app::app(None).listen(&address);
	println!("Running Redlib v{} on {address} (vercel)", env!("CARGO_PKG_VERSION"));

	announce(started, port);
	if let Err(error) = server.await {
		eprintln!("Server error: {error}");
	}
}

/// Tells the runtime the port to proxy to, on the socket it provided.
///
/// The protocol is the one `vercel_runtime` uses: a NUL-terminated JSON object
/// `{"type":"server-started","payload":{"initDuration":<ms>,"httpPort":<port>}}`.
#[cfg(unix)]
fn announce(started: Instant, port: u16) {
	use std::os::unix::net::UnixStream;

	let Some(path) = std::env::var("VERCEL_IPC_PATH").ok().filter(|path| !path.is_empty()) else {
		// Local `vercel dev` without an IPC socket.
		println!("No VERCEL_IPC_PATH, serving on port {port} without announcing");
		return;
	};

	let message = format!(
		"{{\"type\":\"server-started\",\"payload\":{{\"initDuration\":{},\"httpPort\":{port}}}}}\0",
		started.elapsed().as_millis()
	);

	match UnixStream::connect(&path).and_then(|mut socket| socket.write_all(message.as_bytes())) {
		Ok(()) => info!("[✅] Announced 127.0.0.1:{port} to the runtime"),
		Err(error) => eprintln!("Could not announce to {path}: {error}"),
	}
}

#[cfg(not(unix))]
fn announce(_started: Instant, _port: u16) {
	eprintln!("The Vercel entry point only runs on Unix");
}
