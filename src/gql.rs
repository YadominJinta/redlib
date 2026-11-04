//! Reddit listings (new, rising, search) through the Android GraphQL API.
//!
//! Reddit's edge answers the anonymous JSON listing endpoints for `new`,
//! `rising` and search with its HTML block page, while the same data is served
//! to the mobile apps over GraphQL. The persisted operations used here are the
//! public Android ones (see <https://github.com/Amqx/reddit-gql>); raw queries
//! are rejected by the server.
//!
//! The result is converted back into the API's `Listing` shape so that
//! `parse_post` and the templates stay unchanged.

use crate::client::OAUTH_CLIENT;
use crate::http;
use chrono::DateTime;
use log::trace;
use serde_json::{json, Map, Value};

const GQL_URL: &str = "https://gql-fed.reddit.com/";

/// Android `SubredditFeedElements` persisted operation.
const SUBREDDIT_FEED: &str = "SubredditFeedElements";
const SUBREDDIT_FEED_HASH: &str = "97046e2a48050f10a507db354a8da60955ffccb586dcf63163bdfc6610d5cf7a";

/// Android `SearchPosts` persisted operation.
const SEARCH_POSTS: &str = "SearchPosts";
const SEARCH_POSTS_HASH: &str = "8610e33521c90caa8d12576800c18b708ce0d19df3dfac8e2aad55d9860e4bf9";

/// Listings that Reddit only serves over GraphQL.
pub fn handles(path: &str) -> bool {
	let base = path.split('?').next().unwrap_or(path);

	if base.starts_with("/search") || base.starts_with("/r/") && base.contains("/search") {
		return true;
	}

	matches!(sort_of(base), Some("new" | "rising"))
}

/// The subreddit sort in `/r/<sub>/<sort>.json`, if the path is one.
fn sort_of(base: &str) -> Option<&str> {
	let rest = base.strip_prefix("/r/")?;
	let (_, tail) = rest.split_once('/')?;
	let sort = tail.strip_suffix(".json")?;
	Some(sort)
}

pub async fn listing(path: String) -> Result<Value, String> {
	let base = path.split('?').next().unwrap_or(&path);

	if base.contains("search") {
		search(&path).await
	} else {
		feed(&path).await
	}
}

/// `/r/<sub>/new.json?...` and `/r/<sub>/rising.json?...`
async fn feed(path: &str) -> Result<Value, String> {
	let base = path.split('?').next().unwrap_or(path);
	let rest = base.strip_prefix("/r/").ok_or_else(|| format!("Not a subreddit listing | {path}"))?;
	let (subreddit, tail) = rest.split_once('/').ok_or_else(|| format!("Not a subreddit listing | {path}"))?;
	let sort = tail.strip_suffix(".json").unwrap_or(tail);

	let mut variables = json!({
		"subredditName": subreddit.replace("%2B", "+"),
		"includeSubredditInPosts": true,
		"sort": sort.to_uppercase(),
	});

	if let Some(range) = param(path, "after").filter(|after| !after.is_empty()) {
		variables["after"] = Value::String(range);
	}
	// `t` only means something for the ranked sorts.
	if let Some(range) = param(path, "t").filter(|range| !range.is_empty()) {
		variables["range"] = Value::String(range.to_uppercase());
	}

	let response = query(SUBREDDIT_FEED, SUBREDDIT_FEED_HASH, variables).await?;
	let elements = response
		["data"]["postFeed"]["elements"]
		.as_object()
		.ok_or_else(|| format!("Unexpected GraphQL response | {path}"))?;

	let listing = to_listing(elements, path);
	trace!(
		"GraphQL returned {} posts",
		listing["data"]["children"].as_array().map(Vec::len).unwrap_or(0)
	);
	Ok(listing)
}

/// `/search.json?...` and `/r/<sub>/search.json?...`
async fn search(path: &str) -> Result<Value, String> {
	let mut query_text = param(path, "q").unwrap_or_default();
	if query_text.is_empty() {
		return Err("No search query".to_string());
	}

	// `restrict_sr` has no GraphQL variable, but the search syntax does: scoping
	// the query to the subreddit keeps the result set identical to the API's.
	if param(path, "restrict_sr").is_some_and(|value| value != "off" && !value.is_empty()) {
		let sub = path.strip_prefix("/r/").and_then(|rest| rest.split_once('/')).map(|(sub, _)| sub.replace("%2B", "+"));
		if let Some(sub) = sub.filter(|sub| !sub.is_empty() && !query_text.contains("subreddit:")) {
			query_text = format!("{query_text} subreddit:{sub}");
		}
	}

	let mut variables = json!({
		"query": query_text,
		// Reddit validates the surface but accepts any of the known values.
		"productSurface": "ANDROID",
	});

	if let Some(sort) = param(path, "sort").filter(|sort| !sort.is_empty()) {
		variables["sort"] = Value::String(sort.to_uppercase());
	}
	if let Some(range) = param(path, "t").filter(|range| !range.is_empty()) {
		variables["range"] = Value::String(range.to_uppercase());
	}
	if let Some(after) = param(path, "after").filter(|after| !after.is_empty()) {
		variables["after"] = Value::String(after);
	}

	let response = query(SEARCH_POSTS, SEARCH_POSTS_HASH, variables).await?;
	let elements = response
		["data"]["search"]["general"]["posts"]
		.as_object()
		.ok_or_else(|| format!("Unexpected GraphQL response | {path}"))?;

	Ok(to_listing(elements, path))
}

async fn query(operation: &str, hash: &str, variables: Value) -> Result<Value, String> {
	let body = json!({
		"operationName": operation,
		"variables": variables,
		"extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } }
	})
	.to_string();

	let mut headers = vec![
		("Host".to_string(), "gql-fed.reddit.com".to_string()),
		("Content-Type".to_string(), "application/json; charset=UTF-8".to_string()),
	];
	{
		let client = OAUTH_CLIENT.load_full();
		for (key, value) in client.headers_map.clone() {
			if !key.eq_ignore_ascii_case("content-type") && !key.eq_ignore_ascii_case("host") {
				headers.push((key, value));
			}
		}
	}

	trace!("GraphQL {operation} request");
	let reply = http::request("POST", GQL_URL, &headers, Some(body.clone().into_bytes())).await?;

	if !reply.status.is_success() {
		return Err(format!("Reddit GraphQL returned {} | {operation}", reply.status));
	}

	let value: Value = serde_json::from_slice(&reply.body).map_err(|e| format!("Failed to parse GraphQL response: {e}"))?;

	if let Some(errors) = value["errors"].as_array().filter(|errors| !errors.is_empty()) {
		let message = errors
			.first()
			.and_then(|error| error["message"].as_str())
			.unwrap_or("unknown error");
		return Err(format!("Reddit GraphQL error: {message} | {operation}"));
	}

	Ok(value)
}

/// Turns a GraphQL `elements` object into the API's `Listing`.
fn to_listing(elements: &Map<String, Value>, path: &str) -> Value {
	let children: Vec<Value> = elements["edges"]
		.as_array()
		.map(|edges| {
			edges
				.iter()
				.filter_map(|edge| edge["node"].as_object())
				.map(|node| json!({"kind": "t3", "data": to_post(node)}))
				.collect()
		})
		.unwrap_or_default();

	let after = elements["pageInfo"]["endCursor"].as_str().unwrap_or_default();

	json!({
		"kind": "Listing",
		"data": {
			"after": if after.is_empty() { Value::Null } else { Value::String(after.to_string()) },
			"before": Value::Null,
			"dist": children.len(),
			"children": children,
			"path": path,
		}
	})
}

/// Maps a GraphQL post node onto the fields `parse_post` reads.
fn to_post(node: &Map<String, Value>) -> Value {
	let text = |key: &str| node.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
	let flag = |key: &str| node.get(key).and_then(Value::as_bool).unwrap_or(false);
	let nested = |parent: &str, key: &str| node.get(parent).and_then(|object| object.get(key)).and_then(Value::as_str).unwrap_or_default().to_string();

	let id = text("id");
	let markdown = nested("content", "markdown");
	let is_self = flag("isSelfPost");
	let permalink = text("permalink");
	let outbound = nested("outboundLink", "url");

	let created = DateTime::parse_from_str(&text("createdAt"), "%Y-%m-%dT%H:%M:%S%.f%z")
		.map(|date| date.timestamp() as f64)
		.unwrap_or_default();

	let mut post = Map::new();
	post.insert("id".into(), Value::String(id.trim_start_matches("t3_").to_string()));
	post.insert("name".into(), Value::String(id));
	post.insert("title".into(), Value::String(text("title")));
	post.insert("author".into(), Value::String(nested("authorInfo", "name")));
	post.insert("subreddit".into(), Value::String(nested("subreddit", "name")));
	post.insert("permalink".into(), Value::String(permalink));
	post.insert("is_self".into(), Value::Bool(is_self));
	post.insert("selftext".into(), Value::String(markdown.clone()));
	post.insert("selftext_html".into(), Value::String(markdown_to_html(&markdown)));
	post.insert("url".into(), Value::String(if outbound.is_empty() { text("url") } else { outbound }));
	post.insert("domain".into(), Value::String(text("domain")));
	post.insert("score".into(), node.get("score").cloned().unwrap_or(json!(0)));
	post.insert("upvote_ratio".into(), node.get("upvoteRatio").cloned().unwrap_or(json!(1.0)));
	post.insert("num_comments".into(), node.get("commentCount").cloned().unwrap_or(json!(0)));
	post.insert("created_utc".into(), json!(created));
	post.insert("over_18".into(), Value::Bool(flag("isNsfw")));
	post.insert("spoiler".into(), Value::Bool(flag("isSpoiler")));
	post.insert("stickied".into(), Value::Bool(flag("isStickied")));
	post.insert("locked".into(), Value::Bool(flag("isLocked")));
	post.insert("archived".into(), Value::Bool(flag("isArchived")));
	post.insert("is_gallery".into(), Value::Bool(node.get("gallery").is_some_and(|gallery| !gallery.is_null())));
	post.insert("link_flair_text".into(), Value::String(nested("flair", "text")));
	post.insert("author_flair_text".into(), Value::String(nested("authorFlair", "text")));
	post.insert("distinguished".into(), Value::String(nested("distinguishedAs", "as").to_lowercase()).or_distinguished(node));
	post.insert("thumbnail".into(), Value::String(thumbnail(node)));
	post.insert("post_hint".into(), Value::String(text("postHint")));
	post.insert("preview".into(), preview(node));

	Value::Object(post)
}

/// GraphQL reports moderation labels in caps; the API uses lower case.
trait OrDistinguished {
	fn or_distinguished(self, node: &Map<String, Value>) -> Value;
}

impl OrDistinguished for Value {
	fn or_distinguished(self, node: &Map<String, Value>) -> Value {
		match self.as_str() {
			Some("moderator") => json!("moderator"),
			Some("admin") => json!("admin"),
			Some("special") => json!("special"),
			_ => {
				let _ = node;
				Value::Null
			}
		}
	}
}

fn thumbnail(node: &Map<String, Value>) -> String {
	if let Some(uri) = node.get("thumbnail").and_then(|thumbnail| thumbnail.get("url")).and_then(Value::as_str) {
		return uri.to_string();
	}
	// Fall back to the first preview image so list view still shows something.
	node.get("media")
		.and_then(|media| media.get("preview"))
		.and_then(|preview| preview.get("url"))
		.and_then(Value::as_str)
		.unwrap_or_default()
		.to_string()
}

/// Shape a GraphQL media node into the `preview`/`secure_media` fields
/// `Media::parse` understands.
fn preview(node: &Map<String, Value>) -> Value {
	let Some(media) = node.get("media").filter(|media| media.is_object()) else {
		return Value::Null;
	};

	let Some(video) = media.get("redditVideo").filter(|video| video.is_object()) else {
		return Value::Null;
	};

	let mut reddit_video = Map::new();
	for (from, to) in [
		("fallbackUrl", "fallback_url"),
		("hlsUrl", "hls_url"),
		("dashUrl", "dash_url"),
		("isGif", "is_gif"),
		("height", "height"),
		("width", "width"),
	] {
		if let Some(value) = video.get(from) {
			reddit_video.insert(to.to_string(), value.clone());
		}
	}

	json!({ "reddit_video_preview": Value::Object(reddit_video) })
}

fn markdown_to_html(markdown: &str) -> String {
	if markdown.is_empty() {
		return String::new();
	}
	let mut html = String::new();
	pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(markdown));
	crate::utils::rewrite_urls(&html)
}

/// Reads a query string parameter out of a path.
fn param(path: &str, key: &str) -> Option<String> {
	let query = path.split_once('?')?.1;
	url::form_urlencoded::parse(query.as_bytes())
		.find(|(name, _)| name == key)
		.map(|(_, value)| value.into_owned())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_handles_only_blocked_listings() {
		for path in [
			"/r/rust/new.json?raw_json=1",
			"/r/rust/rising.json?raw_json=1",
			"/search.json?q=rust&raw_json=1",
			"/r/rust/search.json?q=async&restrict_sr=on",
		] {
			assert!(handles(path), "{path}");
		}

		for path in ["/r/rust/hot.json?raw_json=1", "/r/rust/top.json?t=week", "/r/rust/about.json", "/r/rust/comments/abc.json"] {
			assert!(!handles(path), "{path}");
		}
	}

	#[test]
	fn test_node_is_mapped_to_api_shape() {
		let node: Map<String, Value> = serde_json::from_value(json!({
			"id": "t3_abc",
			"title": "Hello",
			"createdAt": "2026-11-02T08:54:01.782000+0000",
			"content": { "markdown": "**bold**" },
			"isSelfPost": true,
			"isNsfw": false,
			"isStickied": true,
			"score": 42,
			"commentCount": 7,
			"upvoteRatio": 0.9,
			"permalink": "/r/rust/comments/abc/hello/",
			"authorInfo": { "name": "alice" },
			"subreddit": { "name": "rust" },
			"flair": { "text": "news" },
			"authorFlair": { "text": "crab" },
			"domain": "self.rust",
			"outboundLink": { "url": "https://example.com" }
		}))
		.unwrap();

		let post = to_post(&node);
		assert_eq!(post["id"], "abc");
		assert_eq!(post["author"], "alice");
		assert_eq!(post["subreddit"], "rust");
		assert_eq!(post["score"], 42);
		assert_eq!(post["num_comments"], 7);
		assert_eq!(post["link_flair_text"], "news");
		assert_eq!(post["url"], "https://example.com");
		assert!(post["stickied"].as_bool().unwrap());
		assert!(post["selftext_html"].as_str().unwrap().contains("<strong>bold</strong>"));
		assert!(post["created_utc"].as_f64().unwrap() > 1_700_000_000.0);
	}

	#[test]
	fn test_listing_reports_the_next_cursor() {
		let elements: Map<String, Value> = serde_json::from_value(json!({
			"pageInfo": { "hasNextPage": true, "endCursor": "dDNfYWJj" },
			"edges": [{ "node": { "id": "t3_abc", "title": "Hello", "createdAt": "2026-11-02T08:54:01.782000+0000" } }]
		}))
		.unwrap();

		let listing = to_listing(&elements, "/r/rust/new.json");
		assert_eq!(listing["data"]["after"], "dDNfYWJj");
		assert_eq!(listing["data"]["children"][0]["kind"], "t3");
		assert_eq!(listing["data"]["children"][0]["data"]["id"], "abc");
	}
}
