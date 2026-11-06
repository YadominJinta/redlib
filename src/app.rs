//! The HTTP application: the route table and the static responses, shared by the
//! `redlib` server binary and the serverless entry point so both serve exactly
//! the same routes.

use cached::proc_macro::cached;
use futures_lite::FutureExt;
use hyper::{header::HeaderValue, Body, Request, Response};

use crate::client::{canonical_path, proxy};
use crate::headers;
use crate::server::{self, RequestExt};
use crate::utils::{error, redirect, ThemeAssets};
use crate::{config, duplicates, instance_info, post, search, settings, subreddit, user};

// Required for the manifest to be valid
async fn pwa_logo() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/png")
			.body(include_bytes!("../static/logo.png").as_ref().into())
			.unwrap_or_default(),
	)
}

// Required for iOS App Icons
async fn iphone_logo() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/png")
			.body(include_bytes!("../static/apple-touch-icon.png").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn favicon() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/vnd.microsoft.icon")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_bytes!("../static/favicon.ico").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn font() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "font/woff2")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_bytes!("../static/Inter.var.woff2").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn opensearch() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/opensearchdescription+xml")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_bytes!("../static/opensearch.xml").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn resource(body: &str, content_type: &str, cache: bool) -> Result<Response<Body>, String> {
	let mut res = Response::builder()
		.status(200)
		.header("content-type", content_type)
		.body(body.to_string().into())
		.unwrap_or_default();

	if cache {
		if let Ok(val) = HeaderValue::from_str("public, max-age=1209600, s-maxage=86400") {
			res.headers_mut().insert("Cache-Control", val);
		}
	}

	Ok(res)
}

async fn style() -> Result<Response<Body>, String> {
	let mut res = include_str!("../static/style.css").to_string();
	for file in ThemeAssets::iter() {
		res.push('\n');
		let theme = ThemeAssets::get(file.as_ref()).unwrap();
		res.push_str(std::str::from_utf8(theme.data.as_ref()).unwrap());
	}
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "text/css")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(res.to_string().into())
			.unwrap_or_default(),
	)
}

/// Builds the route table and the headers every response carries.
///
/// `hsts` is the `--hsts` value for `Strict-Transport-Security`, which only a
/// deployment serving plain HTTP in front of a TLS terminator needs.
pub fn app(hsts: Option<&str>) -> server::Server {
	let mut app = server::Server::new();

	// Define default headers (added to all responses)
	// Redirected media is fetched by the browser from Reddit's CDN, which the
	// policy has to allow; otherwise media stays same-origin through the proxy.
	let (img_src, media_src) = if crate::client::serverless_media_redirect() {
		(
			"img-src 'self' data: https://*.redd.it https://*.redditmedia.com https://*.redditstatic.com;",
			"media-src 'self' data: blob: about: https://*.redd.it https://*.redditmedia.com;",
		)
	} else {
		("img-src 'self' data:;", "media-src 'self' data: blob: about:;")
	};

	app.default_headers = headers! {
		"Referrer-Policy" => "no-referrer",
		"X-Content-Type-Options" => "nosniff",
		"X-Frame-Options" => "DENY",
		"Content-Security-Policy" => &format!("default-src 'none'; font-src 'self'; script-src 'self' blob:; manifest-src 'self'; {media_src} style-src 'self' 'unsafe-inline'; base-uri 'none'; {img_src} form-action 'self'; frame-ancestors 'none'; connect-src 'self'; worker-src blob:;")
	};

	if let Some(expire_time) = hsts {
		if let Ok(val) = HeaderValue::from_str(&format!("max-age={expire_time}")) {
			app.default_headers.insert("Strict-Transport-Security", val);
		}
	}

	// Read static files
	app.at("/style.css").get(|_| style().boxed());
	app
		.at("/manifest.json")
		.get(|_| resource(include_str!("../static/manifest.json"), "application/json", false).boxed());
	app.at("/robots.txt").get(|_| {
		resource(
			if match config::get_setting("REDLIB_ROBOTS_DISABLE_INDEXING") {
				Some(val) => val == "on",
				None => false,
			} {
				"User-agent: *\nDisallow: /"
			} else {
				"User-agent: *\nDisallow: /u/\nDisallow: /user/"
			},
			"text/plain",
			true,
		)
		.boxed()
	});
	app.at("/favicon.ico").get(|_| favicon().boxed());
	app.at("/logo.png").get(|_| pwa_logo().boxed());
	app.at("/Inter.var.woff2").get(|_| font().boxed());
	app.at("/touch-icon-iphone.png").get(|_| iphone_logo().boxed());
	app.at("/apple-touch-icon.png").get(|_| iphone_logo().boxed());
	app.at("/opensearch.xml").get(|_| opensearch().boxed());
	app
		.at("/playHLSVideo.js")
		.get(|_| resource(include_str!("../static/playHLSVideo.js"), "text/javascript", false).boxed());
	app
		.at("/hls.min.js")
		.get(|_| resource(include_str!("../static/hls.min.js"), "text/javascript", false).boxed());
	app
		.at("/highlighted.js")
		.get(|_| resource(include_str!("../static/highlighted.js"), "text/javascript", false).boxed());
	app
		.at("/check_update.js")
		.get(|_| resource(include_str!("../static/check_update.js"), "text/javascript", false).boxed());
	app.at("/copy.js").get(|_| resource(include_str!("../static/copy.js"), "text/javascript", false).boxed());

	app.at("/commits.atom").get(|_| async move { proxy_commit_info().await }.boxed());
	app.at("/instances.json").get(|_| async move { proxy_instances().await }.boxed());

	// Proxy media through Redlib
	app.at("/vid/:id/:size").get(|r| proxy(r, "https://v.redd.it/{id}/DASH_{size}").boxed());
	app.at("/hls/:id/*path").get(|r| proxy(r, "https://v.redd.it/{id}/{path}").boxed());
	app.at("/img/*path").get(|r| proxy(r, "https://i.redd.it/{path}").boxed());
	app.at("/thumb/:point/:id").get(|r| proxy(r, "https://{point}.thumbs.redditmedia.com/{id}").boxed());
	app.at("/emoji/:id/:name").get(|r| proxy(r, "https://emoji.redditmedia.com/{id}/{name}").boxed());
	app
		.at("/emote/:subreddit_id/:filename")
		.get(|r| proxy(r, "https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/{subreddit_id}/{filename}").boxed());
	app
		.at("/preview/:loc/award_images/:fullname/:id")
		.get(|r| proxy(r, "https://{loc}view.redd.it/award_images/{fullname}/{id}").boxed());
	app.at("/preview/:loc/:id").get(|r| proxy(r, "https://{loc}view.redd.it/{id}").boxed());
	app.at("/style/*path").get(|r| proxy(r, "https://styles.redditmedia.com/{path}").boxed());
	app.at("/static/*path").get(|r| proxy(r, "https://www.redditstatic.com/{path}").boxed());

	// Browse user profile
	app
		.at("/u/:name")
		.get(|r| async move { Ok(redirect(&format!("/user/{}", r.param("name").unwrap_or_default()))) }.boxed());
	app.at("/u/:name/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/u/:name/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());

	app.at("/user/[deleted]").get(|req| error(req, "User has deleted their account").boxed());
	app.at("/user/:name.rss").get(|r| user::rss(r).boxed());
	app.at("/user/:name").get(|r| user::profile(r).boxed());
	app.at("/user/:name/:listing").get(|r| user::profile(r).boxed());
	app.at("/user/:name/comments/:id").get(|r| post::item(r).boxed());
	app.at("/user/:name/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/user/:name/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());

	// Configure settings
	app.at("/settings").get(|r| settings::get(r).boxed()).post(|r| settings::set(r).boxed());
	app.at("/settings/restore").get(|r| settings::restore(r).boxed());
	app.at("/settings/encoded-restore").post(|r| settings::encoded_restore(r).boxed());
	app.at("/settings/update").get(|r| settings::update(r).boxed());

	// RSS Subscriptions
	app.at("/r/:sub.rss").get(|r| subreddit::rss(r).boxed());

	// Subreddit services
	app
		.at("/r/:sub")
		.get(|r| subreddit::community(r).boxed())
		.post(|r| subreddit::add_quarantine_exception(r).boxed());

	app
		.at("/r/u_:name")
		.get(|r| async move { Ok(redirect(&format!("/user/{}", r.param("name").unwrap_or_default()))) }.boxed());

	app.at("/r/:sub/subscribe").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/unsubscribe").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/filter").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/unfilter").post(|r| subreddit::subscriptions_filters(r).boxed());

	app.at("/r/:sub/comments/:id").get(|r| post::item(r).boxed());
	app.at("/r/:sub/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/r/:sub/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());
	app.at("/comments/:id").get(|r| post::item(r).boxed());
	app.at("/comments/:id/comments").get(|r| post::item(r).boxed());
	app.at("/comments/:id/comments/:comment_id").get(|r| post::item(r).boxed());
	app.at("/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());

	app.at("/r/:sub/duplicates/:id").get(|r| duplicates::item(r).boxed());
	app.at("/r/:sub/duplicates/:id/:title").get(|r| duplicates::item(r).boxed());
	app.at("/duplicates/:id").get(|r| duplicates::item(r).boxed());
	app.at("/duplicates/:id/:title").get(|r| duplicates::item(r).boxed());

	app.at("/r/:sub/search").get(|r| search::find(r).boxed());

	app
		.at("/r/:sub/w")
		.get(|r| async move { Ok(redirect(&format!("/r/{}/wiki", r.param("sub").unwrap_or_default()))) }.boxed());
	app
		.at("/r/:sub/w/*page")
		.get(|r| async move { Ok(redirect(&format!("/r/{}/wiki/{}", r.param("sub").unwrap_or_default(), r.param("wiki").unwrap_or_default()))) }.boxed());
	app.at("/r/:sub/wiki").get(|r| subreddit::wiki(r).boxed());
	app.at("/r/:sub/wiki/*page").get(|r| subreddit::wiki(r).boxed());

	app.at("/r/:sub/about/sidebar").get(|r| subreddit::sidebar(r).boxed());

	app.at("/r/:sub/:sort").get(|r| subreddit::community(r).boxed());

	// Front page
	app.at("/").get(|r| subreddit::community(r).boxed());

	// View Reddit wiki
	app.at("/w").get(|_| async { Ok(redirect("/wiki")) }.boxed());
	app
		.at("/w/*page")
		.get(|r| async move { Ok(redirect(&format!("/wiki/{}", r.param("page").unwrap_or_default()))) }.boxed());
	app.at("/wiki").get(|r| subreddit::wiki(r).boxed());
	app.at("/wiki/*page").get(|r| subreddit::wiki(r).boxed());

	// Search all of Reddit
	app.at("/search").get(|r| search::find(r).boxed());

	// Handle about pages
	app.at("/about").get(|req| error(req, "About pages aren't added yet").boxed());

	// Instance info page
	app.at("/info").get(|r| instance_info::instance_info(r).boxed());
	app.at("/info.:extension").get(|r| instance_info::instance_info(r).boxed());

	// Handle obfuscated share links.
	// Note that this still forces the server to follow the share link to get to the post, so maybe this wants to be updated with a warning before it follow it
	app.at("/r/:sub/s/:id").get(|req: Request<Body>| {
		Box::pin(async move {
			let sub = req.param("sub").unwrap_or_default();
			match req.param("id").as_deref() {
				// Share link
				Some(id) if (8..12).contains(&id.len()) => match canonical_path(format!("/r/{sub}/s/{id}"), 3).await {
					Ok(Some(path)) => Ok(redirect(&path)),
					Ok(None) => error(req, "Post ID is invalid. It may point to a post on a community that has been banned.").await,
					Err(e) => error(req, &e).await,
				},

				// Error message for unknown pages
				_ => error(req, "Nothing here").await,
			}
		})
	});

	app.at("/:id").get(|req: Request<Body>| {
		Box::pin(async move {
			match req.param("id").as_deref() {
				// Sort front page
				Some("best" | "hot" | "new" | "top" | "rising" | "controversial") => subreddit::community(req).await,

				// Short link for post
				Some(id) if (5..8).contains(&id.len()) => match canonical_path(format!("/comments/{id}"), 3).await {
					Ok(path_opt) => match path_opt {
						Some(path) => Ok(redirect(&path)),
						None => error(req, "Post ID is invalid. It may point to a post on a community that has been banned.").await,
					},
					Err(e) => error(req, &e).await,
				},

				// Error message for unknown pages
				_ => error(req, "Nothing here").await,
			}
		})
	});

	// Default service in case no routes match
	app.at("/*").get(|req| error(req, "Nothing here").boxed());

	app
}

pub async fn proxy_commit_info() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/atom+xml")
			.body(Body::from(fetch_commit_info().await))
			.unwrap_or_default(),
	)
}

#[cached(time = 600)]
async fn fetch_commit_info() -> String {
	let url = "https://github.com/redlib-org/redlib/commits/main.atom";

	crate::http::request("GET", url, &[], None).await.expect("Failed to request GitHub").text()
}

pub async fn proxy_instances() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/json")
			.body(Body::from(fetch_instances().await))
			.unwrap_or_default(),
	)
}

#[cached(time = 600)]
async fn fetch_instances() -> String {
	let url = "https://raw.githubusercontent.com/redlib-org/redlib-instances/refs/heads/main/instances.json";

	crate::http::request("GET", url, &[], None).await.expect("Failed to request GitHub").text()
}
