// Global specifiers
#![forbid(unsafe_code)]
#![allow(clippy::cmp_owned)]

use clap::{Arg, ArgAction, Command};
use std::sync::LazyLock;

use log::{info, warn};
use redlib::app;
use redlib::client::{rate_limit_check, OAUTH_CLIENT};
use redlib::{config, instance_info};

#[tokio::main]
async fn main() {
	// Load environment variables
	_ = dotenvy::dotenv();

	// Initialize logger
	pretty_env_logger::init();

	let matches = Command::new("Redlib")
		.version(env!("CARGO_PKG_VERSION"))
		.about("Private front-end for Reddit written in Rust ")
		.arg(Arg::new("ipv4-only").short('4').long("ipv4-only").help("Listen on IPv4 only").num_args(0))
		.arg(Arg::new("ipv6-only").short('6').long("ipv6-only").help("Listen on IPv6 only").num_args(0))
		.arg(
			Arg::new("redirect-https")
				.short('r')
				.long("redirect-https")
				.help("Redirect all HTTP requests to HTTPS (no longer functional)")
				.num_args(0),
		)
		.arg(
			Arg::new("address")
				.short('a')
				.long("address")
				.value_name("ADDRESS")
				.help("Sets address to listen on")
				.default_value("[::]")
				.num_args(1),
		)
		.arg(
			Arg::new("port")
				.short('p')
				.long("port")
				.value_name("PORT")
				.env("PORT")
				.help("Port to listen on")
				.default_value("8080")
				.action(ArgAction::Set)
				.num_args(1),
		)
		.arg(
			Arg::new("hsts")
				.short('H')
				.long("hsts")
				.value_name("EXPIRE_TIME")
				.help("HSTS header to tell browsers that this site should only be accessed over HTTPS")
				.default_value("604800")
				.num_args(1),
		)
		.get_matches();

	match rate_limit_check().await {
		Ok(()) => {
			info!("[✅] Rate limit check passed");
		}
		Err(e) => {
			let mut message = format!("Rate limit check failed: {e}");
			message += "\nThis may cause issues with the rate limit.";
			message += "\nPlease report this error with the above information.";
			message += "\nhttps://github.com/redlib-org/redlib/issues/new?assignees=sigaloid&labels=bug&title=%F0%9F%90%9B+Bug+Report%3A+Rate+limit+mismatch";
			warn!("{}", message);
			eprintln!("{message}");
		}
	}

	let address = matches.get_one::<String>("address").unwrap();
	let port = matches.get_one::<String>("port").unwrap();
	let hsts = matches.get_one("hsts").map(|m: &String| m.as_str());

	let ipv4_only = std::env::var("IPV4_ONLY").is_ok() || matches.get_flag("ipv4-only");
	let ipv6_only = std::env::var("IPV6_ONLY").is_ok() || matches.get_flag("ipv6-only");

	let listener = if ipv4_only {
		format!("0.0.0.0:{port}")
	} else if ipv6_only {
		format!("[::]:{port}")
	} else {
		[address, ":", port].concat()
	};

	println!("Starting Redlib...");

	// Force evaluation of statics. In instance_info case, we need to evaluate
	// the timestamp so deploy date is accurate - in config case, we need to
	// evaluate the configuration to avoid paying penalty at first request -
	// in OAUTH case, we need to retrieve the token to avoid paying penalty
	// at first request
	info!("Evaluating config.");
	LazyLock::force(&config::CONFIG);
	info!("Evaluating instance info.");
	LazyLock::force(&instance_info::INSTANCE_INFO);
	info!("Creating OAUTH client.");
	LazyLock::force(&OAUTH_CLIENT);

	let app = app::app(hsts);

	println!("Running Redlib v{} on {listener}!", env!("CARGO_PKG_VERSION"));

	// Run this server for... forever!
	if let Err(e) = app.listen(&listener).await {
		eprintln!("Server error: {e}");
	}
}
