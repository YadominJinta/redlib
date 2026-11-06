//! Vercel entry point (`api/index.rs`).
//!
//! Vercel's Rust runtime builds every `[[bin]]` pointing into `api/` and runs it
//! as one serverless function, so this file is the binary target declared in
//! `Cargo.toml` and `vercel.json` rewrites every path to it.
//!
//! The runtime hands requests over through `vercel_runtime`, whose `Request` is a
//! hyper 1 type while the whole application is written against hyper 0.14. The
//! two ends are bridged here; everything below is the same `app()` the server
//! binary serves, so routes, templates and the GraphQL client are untouched.

use std::sync::LazyLock;

use http_body_util::BodyExt;
// The application speaks hyper 0.14; `vercel_runtime` speaks hyper 1.
use hyper as hyper_0_14;
use log::info;
use redlib::client::OAUTH_CLIENT;
use redlib::server::Server;
use redlib::{app, config, instance_info};
use vercel_runtime::{run, service_fn, Error, Request, Response, ResponseBody};

/// The route table, built once per process.
static APP: LazyLock<Server> = LazyLock::new(|| app::app(None));

#[tokio::main]
async fn main() -> Result<(), Error> {
	_ = dotenvy::dotenv();
	pretty_env_logger::init();
	warm();

	run(service_fn(handler)).await
}

/// Evaluate the statics up front, so the first invocation does not pay for the
/// configuration, the instance info and the OAuth token request.
fn warm() {
	info!("Evaluating config.");
	LazyLock::force(&config::CONFIG);
	info!("Evaluating instance info.");
	LazyLock::force(&instance_info::INSTANCE_INFO);
	info!("Creating OAUTH client.");
	LazyLock::force(&OAUTH_CLIENT);
}

async fn handler(req: Request) -> Result<Response<ResponseBody>, Error> {
	let request = match bridge_request(req).await {
		Ok(request) => request,
		Err(error) => return error_response(400, &error),
	};

	match APP.handle(request).await {
		Ok(response) => bridge_response(response).await,
		// `handle` reports handler failures as a message, exactly as the socket
		// server does, and the default headers were already attached there.
		Err(message) => error_response(500, &message),
	}
}

/// hyper 1 request (the runtime's type) to the hyper 0.14 request the app uses.
async fn bridge_request(req: Request) -> Result<hyper_0_14::Request<hyper_0_14::Body>, String> {
	let (parts, body) = req.into_parts();
	let bytes = body.collect().await.map_err(|error| format!("Could not read the request body: {error}"))?.to_bytes();

	let mut request = hyper_0_14::Request::builder().method(parts.method.as_str()).uri(parts.uri.to_string());
	{
		let headers = request.headers_mut().ok_or("Could not build the request")?;
		for (name, value) in parts.headers.iter() {
			// `append` keeps repeated headers, which cookies and caching rely on.
			headers.append(
				hyper_0_14::header::HeaderName::from_bytes(name.as_str().as_bytes()).map_err(|error| error.to_string())?,
				hyper_0_14::header::HeaderValue::from_bytes(value.as_bytes()).map_err(|error| error.to_string())?,
			);
		}
	}

	request.body(hyper_0_14::Body::from(bytes.to_vec())).map_err(|error| error.to_string())
}

/// The app's hyper 0.14 response to the runtime's hyper 1 response.
async fn bridge_response(response: hyper_0_14::Response<hyper_0_14::Body>) -> Result<Response<ResponseBody>, Error> {
	let (parts, body) = response.into_parts();
	// hyper 0.14's body speaks http-body 0.4, so collect it with hyper's own helper.
	let bytes = hyper_0_14::body::to_bytes(body)
		.await
		.map_err(|error| format!("Could not read the response body: {error}"))?;

	let mut response = Response::builder().status(parts.status.as_u16());
	{
		let headers = response.headers_mut().ok_or("Could not build the response")?;
		for (name, value) in parts.headers.iter() {
			headers.append(name.as_str().parse::<http::HeaderName>()?, http::HeaderValue::from_bytes(value.as_bytes())?);
		}
	}

	Ok(response.body(ResponseBody::from(bytes))?)
}

fn error_response(status: u16, message: &str) -> Result<Response<ResponseBody>, Error> {
	Ok(
		Response::builder()
			.status(status)
			.header("content-type", "text/plain; charset=utf-8")
			.body(ResponseBody::from(message))?,
	)
}
