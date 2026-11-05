//! Reddit data through the Android GraphQL API.
//!
//! Reddit's edge answers the anonymous JSON endpoints for `new`, `rising`,
//! search and every comments route with its HTML block page
//! ("blocked by network security"), while the same data is served to the mobile
//! apps over GraphQL. The persisted operations used here ship in the official
//! Android app; raw queries are rejected by the server.
//!
//! `X-APOLLO-OPERATION-ID` is the SHA-256 of the operation's document, and the
//! server only answers hashes it has stored for a shipped app version, so the
//! constants below have to be refreshed from a new APK when Reddit rotates them.
//!
//! Results are converted back into the API's `Listing` shape so that
//! `parse_post`, `parse_comments` and the templates stay unchanged.

use crate::client::OAUTH_CLIENT;
use crate::http;
use chrono::DateTime;
use log::{trace, warn};
use serde_json::{json, Map, Value};

const GQL_URL: &str = "https://gql-fed.reddit.com/";

/// Android `SubredditFeedElements` persisted operation (full post objects).
/// Older than the 2026.40.0 build but still served; `SUBREDDIT_FEED_SDUI` is the
/// fallback if it is ever retired.
const SUBREDDIT_FEED: &str = "SubredditFeedElements";
const SUBREDDIT_FEED_HASH: &str = "97046e2a48050f10a507db354a8da60955ffccb586dcf63163bdfc6610d5cf7a";

/// Android `SubredditFeedSdui` persisted operation (2026.40.0). Returns server
/// driven UI cells; only the post ids are used here.
const SUBREDDIT_FEED_SDUI: &str = "SubredditFeedSdui";
const SUBREDDIT_FEED_SDUI_HASH: &str = "7af4e34b0c0c971bb170ea13d7df5973d0b121fb6dcc8f01fc9b3d1c701bc407";

/// Android `PostsByIds` persisted operation, used to turn the ids from the SDUI
/// feed back into full post objects.
const POSTS_BY_IDS: &str = "PostsByIds";
const POSTS_BY_IDS_HASH: &str = "39a31c427cbab8f889ab5b57b2c048bd6fc331646ca2efe6d89e89d8b952a7e5";

/// Android `SearchPosts` persisted operation (2026.40.0).
const SEARCH_POSTS: &str = "SearchPosts";
const SEARCH_POSTS_HASH: &str = "ddff1442845558a4cdf275235ee7baeb8c9687175169098af9cab8d5d975c314";

/// Android `PostComments` persisted operation (2026.40.0).
const POST_COMMENTS: &str = "PostComments";
const POST_COMMENTS_HASH: &str = "dbf0d56d3f416e9c586c7ef7096d1467d0625d11626fd30c2295b98fd95a81ba";

/// Android `SubmittedPostsFeedSdui` persisted operation (2026.40.0).
const SUBMITTED_POSTS: &str = "SubmittedPostsFeedSdui";
const SUBMITTED_POSTS_HASH: &str = "8135e0b308a5b3b0ef6693d820349678fa7fc8a0b8fffffa09eee29c21894c0c";

/// The two cross-subreddit feeds have their own SDUI operations.
const POPULAR_FEED: (&str, &str) = ("PopularFeedSdui", "64d20323c98dede2fc39bb738784a737b3279fd86d2c7a059ee3908638ff4c30");
const ALL_FEED: (&str, &str) = ("AllFeedSdui", "c2c71fa2d5160cc89193d8d93b973eb2d4ce943ea35139f3d31a07676e6fe822");

/// Android `SubredditInfoByName` persisted operation (2026.40.0).
const SUBREDDIT_INFO: &str = "SubredditInfoByName";
const SUBREDDIT_INFO_HASH: &str = "59bd458a25baeb82358f407720b5da7636dac2c7fa14ebd045c04fc6d659d2df";

/// Android `SubredditWiki` persisted operation (2026.40.0).
const SUBREDDIT_WIKI: &str = "SubredditWiki";
const SUBREDDIT_WIKI_HASH: &str = "32f9314683f033a76812f772bbba771958760286c5cd28c558071a3ff4eb231f";

/// Android `UserProfile` persisted operation (2026.40.0).
const USER_PROFILE: &str = "UserProfile";
const USER_PROFILE_HASH: &str = "a05c628c5469c6b107c85988a8b1d0b79a2e7b6150ebd3a449c67e226e975ddc";

/// Android `GetUnavailableSubredditReason` persisted operation (2026.40.0),
/// used to keep the API's "banned"/"private"/"gated" messages.
const SUBREDDIT_REASON: &str = "GetUnavailableSubredditReason";
const SUBREDDIT_REASON_HASH: &str = "bc0e5610e309f694443e5822a22e7c0e52180351b488d02bed0a44cf5c6a888f";

/// Every sort the app's feed operations accept.
const FEED_SORTS: &[&str] = &["hot", "new", "top", "rising", "controversial"];

/// Whether a legacy JSON path can be served by the app's GraphQL API.
///
/// Multireddit and subscription feeds (`/r/a+b/...`) stay on REST, because there
/// is no persisted operation for them.
pub fn handles(path: &str) -> bool {
	let base = path.split('?').next().unwrap_or(path);

	if base.starts_with("/search") || base.starts_with("/r/") && base.contains("/search") {
		return true;
	}
	if by_id_path(base).is_some() || user_path(base).is_some() {
		return true;
	}

	let Some((subreddit, tail)) = split_subreddit(base) else {
		return false;
	};
	if subreddit.is_empty() || subreddit.contains('+') || subreddit.contains("%2B") {
		return false;
	}

	if tail == "about.json" {
		return true;
	}
	if let Some(page) = tail.strip_prefix("wiki/").and_then(|page| page.strip_suffix(".json")) {
		return !page.is_empty() && !page.contains('/');
	}

	tail.strip_suffix(".json").is_some_and(|sort| FEED_SORTS.contains(&sort))
}

/// Splits `/r/<sub>/<tail>` into its two parts.
fn split_subreddit(base: &str) -> Option<(&str, &str)> {
	base.strip_prefix("/r/")?.split_once('/')
}

/// The post id in `/by_id/t3_<id>.json`.
fn by_id_path(base: &str) -> Option<String> {
	let id = base.strip_prefix("/by_id/t3_")?.strip_suffix(".json")?;
	(!id.is_empty() && !id.contains('/')).then(|| id.to_string())
}

/// What a `/user/<name>/<listing>.json` path asks for.
enum UserRequest {
	About(String),
	Submitted(String),
}

fn user_path(base: &str) -> Option<UserRequest> {
	let (name, tail) = base.strip_prefix("/user/")?.split_once('/')?;
	if name.is_empty() || name.contains('/') {
		return None;
	}
	match tail {
		"about.json" => Some(UserRequest::About(name.to_string())),
		"submitted.json" => Some(UserRequest::Submitted(name.to_string())),
		_ => None,
	}
}

/// Serves a legacy JSON path over the app's GraphQL API.
///
/// The returned value keeps the JSON API's shape, so the parsers and templates
/// that consumed the REST response are unchanged.
pub async fn json(path: &str, quarantine: bool) -> Result<Value, String> {
	let base = path.split('?').next().unwrap_or(path);

	if let Some(id) = by_id_path(base) {
		return post_by_id(&id).await;
	}

	if let Some(request) = user_path(base) {
		return match request {
			UserRequest::About(name) => user_about(&name).await,
			UserRequest::Submitted(name) => user_submitted(&name, path).await,
		};
	}

	if let Some((subreddit, tail)) = split_subreddit(base) {
		if tail == "about.json" {
			return subreddit_about(subreddit, quarantine).await;
		}
		if let Some(page) = tail.strip_prefix("wiki/").and_then(|page| page.strip_suffix(".json")) {
			return wiki_page(subreddit, page).await;
		}
	}

	if base.contains("search") {
		return search(path).await;
	}

	feed(path).await
}

/// `/r/<sub>/<sort>.json?...` for every sort the app serves.
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

	// `popular` and `all` are not subreddits: the app has a dedicated SDUI
	// operation for each, and the subreddit feed answers `UnavailableSubreddit`.
	if subreddit == "popular" || subreddit == "all" {
		let (operation, hash) = if subreddit == "popular" { POPULAR_FEED } else { ALL_FEED };
		let root = if subreddit == "popular" { "popularV3" } else { "allV3" };
		return feed_via_sdui(operation, hash, root, &variables, path).await;
	}

	let elements = match query(SUBREDDIT_FEED, SUBREDDIT_FEED_HASH, variables.clone()).await {
		Ok(response) => response["data"]["postFeed"]["elements"].as_object().cloned(),
		Err(error) => {
			// The persisted operation may be retired by a newer app version.
			warn!("SubredditFeedElements failed ({error}), falling back to SubredditFeedSdui");
			None
		}
	};

	if let Some(elements) = elements {
		let listing = to_listing(&elements, path);
		trace!(
			"GraphQL returned {} posts",
			listing["data"]["children"].as_array().map(Vec::len).unwrap_or(0)
		);
		return Ok(listing);
	}

	feed_via_sdui(SUBREDDIT_FEED_SDUI, SUBREDDIT_FEED_SDUI_HASH, "subredditV3", &variables, path).await
}

/// Fallback listing path for the feeds the app serves as presentation cells:
/// the ids sit in the cell group's `groupId` and are resolved back into full
/// posts afterwards.
async fn feed_via_sdui(operation: &str, hash: &str, root: &str, variables: &Value, path: &str) -> Result<Value, String> {
	let mut sdui_variables = json!({
		"sort": variables["sort"].clone(),
		"time": "ALL",
	});
	if let Some(subreddit) = variables.get("subredditName") {
		sdui_variables["subredditName"] = subreddit.clone();
	}
	if let Some(after) = variables.get("after") {
		sdui_variables["after"] = after.clone();
	}
	if let Some(range) = variables.get("range") {
		sdui_variables["time"] = range.clone();
	}

	let response = query(operation, hash, sdui_variables).await?;
	let elements = response["data"][root]["elements"]
		.as_object()
		.ok_or_else(|| format!("Unexpected GraphQL response | {path}"))?;

	let after = feed_cursor(elements).to_string();
	let posts = posts_of_elements(elements).await?;
	Ok(posts_listing(posts, &after, path))
}

/// One page of posts, in the API's `Listing` shape.
fn posts_listing(posts: Vec<Value>, after: &str, path: &str) -> Value {
	let children: Vec<Value> = posts
		.iter()
		.filter_map(Value::as_object)
		.map(|post| json!({ "kind": "t3", "data": to_post(post) }))
		.collect();

	let mut data = json!({
		"after": if after.is_empty() { Value::Null } else { Value::String(after.to_string()) },
		"before": Value::Null,
		"dist": children.len(),
		"children": children,
		"path": path,
	});
	// The JSON API echoes the geo filter it applied, and `/r/popular` is the one
	// caller that reads it back.
	if let Some(geo_filter) = param(path, "geo_filter") {
		data["geo_filter"] = Value::String(geo_filter);
	}

	json!({ "kind": "Listing", "data": data })
}

/// The cursor of an SDUI feed page.
fn feed_cursor(elements: &Map<String, Value>) -> &str {
	elements["pageInfo"]["endCursor"].as_str().unwrap_or_default()
}

/// Resolves the post ids of an SDUI feed page into full post objects.
async fn posts_of_elements(elements: &Map<String, Value>) -> Result<Vec<Value>, String> {
	let ids: Vec<Value> = elements["edges"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|edge| {
			let node = edge.get("node")?;
			let id = node["groupId"].as_str().or_else(|| node["id"].as_str())?;
			id.starts_with("t3_").then(|| Value::String(id.to_string()))
		})
		.collect();

	posts_by_ids(&ids).await
}

async fn posts_by_ids(ids: &[Value]) -> Result<Vec<Value>, String> {
	if ids.is_empty() {
		return Ok(Vec::new());
	}
	let response = query(POSTS_BY_IDS, POSTS_BY_IDS_HASH, json!({ "ids": ids, "includeSubredditInPosts": true })).await?;
	Ok(response["data"]["postsInfoByIds"].as_array().cloned().unwrap_or_default())
}

/// `/by_id/t3_<id>.json`, as a one-post `Listing`.
async fn post_by_id(id: &str) -> Result<Value, String> {
	let posts = posts_by_ids(&[Value::String(format!("t3_{id}"))]).await?;
	let Some(post) = posts.first().filter(|post| post.is_object()) else {
		return Err("Post not found".to_string());
	};
	Ok(json!({
		"kind": "Listing",
		"data": { "children": [{ "kind": "t3", "data": to_post(post.as_object().unwrap_or(&Map::new())) }] }
	}))
}

/// `/r/<sub>/about.json`, in the API's subreddit shape.
async fn subreddit_about(subreddit: &str, quarantine: bool) -> Result<Value, String> {
	let variables = json!({
		"subredditName": subreddit,
		"includeRecapFields": false,
		"includeDevvitData": false,
		// Quarantined and gated communities need the logged-out opt-in.
		"loggedOutIsOptedIn": quarantine,
	});
	let response = query(SUBREDDIT_INFO, SUBREDDIT_INFO_HASH, variables).await?;
	let sub = &response["data"]["subredditInfoByName"];

	// Reddit answers `UnavailableSubreddit`/`PrivateSubreddit` for the
	// communities it hides. `subreddit.rs` distinguishes those cases by message,
	// so keep reporting the reason rather than a generic failure.
	if sub["__typename"].as_str() != Some("Subreddit") {
		return Err(unavailable_subreddit(subreddit).await);
	}
	if sub["isQuarantined"].as_bool().unwrap_or_default() && !quarantine {
		return Err("quarantined".to_string());
	}

	let markdown = sub["description"]["markdown"].as_str().unwrap_or_default();
	let description_html = if markdown.is_empty() { String::new() } else { markdown_to_html(markdown) };

	// The JSON API has both `community_icon` and `icon_img`; the GraphQL node
	// carries one icon plus its smaller legacy variant.
	let icon = sub["styles"]["icon"].as_str().unwrap_or_default();
	let legacy_icon = sub["styles"]["legacyIcon"]["url"].as_str().unwrap_or_default();

	// There is no "online now" count in the GraphQL node, so activity comes from
	// the weekly counter instead of the API's `accounts_active`.
	let active = sub["communityStats"]["weeklyActiveUsersCount"].as_u64().unwrap_or_default();
	let wiki_enabled = sub["wiki"]["index"]["pageTree"].as_array().is_some_and(|pages| !pages.is_empty());

	Ok(json!({
		"kind": "t5",
		"data": {
			"display_name": sub["name"].as_str().unwrap_or(subreddit),
			"title": sub["title"].as_str().unwrap_or_default(),
			"public_description": sub["publicDescriptionText"].as_str().unwrap_or_default(),
			"description_html": description_html,
			"community_icon": icon,
			"icon_img": if legacy_icon.is_empty() { icon } else { legacy_icon },
			"subscribers": sub["subscribersCount"].as_u64().unwrap_or_default(),
			"accounts_active": active,
			"wiki_enabled": wiki_enabled,
			"over18": sub["isNsfw"].as_bool().unwrap_or_default(),
		}
	}))
}

/// Why Reddit does not serve this community, in the API's own wording.
///
/// The subreddit query itself answers with a fragment carrying `forbiddenReason`
/// for banned and private communities. Gated ones are hidden by
/// `SubredditInfoByName` alone, so a resolvable subreddit there means "gated".
async fn unavailable_subreddit(subreddit: &str) -> String {
	fn reason(node: &Value) -> Option<String> {
		match node["forbiddenReason"].as_str() {
			Some("BANNED") => Some("banned".to_string()),
			Some("PRIVATE") => Some("private".to_string()),
			Some(other) => Some(other.to_ascii_lowercase()),
			None => None,
		}
	}

	let Ok(response) = query(SUBREDDIT_REASON, SUBREDDIT_REASON_HASH, json!({ "subredditName": subreddit })).await else {
		return "unavailable".to_string();
	};
	let node = &response["data"]["subredditInfoByName"];

	if let Some(reason) = reason(node) {
		return reason;
	}
	if node["__typename"].as_str() == Some("Subreddit") {
		return "gated".to_string();
	}
	"unavailable".to_string()
}

/// `/user/<name>/about.json`, in the API's account shape.
async fn user_about(name: &str) -> Result<Value, String> {
	let variables = json!({ "name": name, "includePremiumAvatarTreatment": false });
	let response = query(USER_PROFILE, USER_PROFILE_HASH, variables).await?;
	let redditor = &response["data"]["redditorInfoByName"];
	if !redditor.is_object() || redditor["__typename"] != "Redditor" {
		return Err(format!("User {name} not found"));
	}

	let profile = &redditor["profile"];
	let created = DateTime::parse_from_str(profile["createdAt"].as_str().unwrap_or_default(), "%Y-%m-%dT%H:%M:%S%.f%z")
		.map(|date| date.timestamp() as f64)
		.unwrap_or_default();

	Ok(json!({
		"kind": "t2",
		"data": {
			"name": redditor["name"].as_str().unwrap_or(name),
			"created": created,
			"total_karma": redditor["karma"]["total"].as_i64().unwrap_or_default(),
			"link_karma": redditor["karma"]["fromPosts"].as_i64().unwrap_or_default(),
			"comment_karma": redditor["karma"]["fromComments"].as_i64().unwrap_or_default(),
			"subreddit": {
				"title": profile["title"].as_str().unwrap_or(name),
				"icon_img": profile["styles"]["icon"].as_str().unwrap_or_default(),
				"banner_img": profile["styles"]["profileBanner"].as_str().unwrap_or_default(),
				"public_description": profile["publicDescriptionText"].as_str().unwrap_or_default(),
				"over_18": profile["isNsfw"].as_bool().unwrap_or_default(),
			},
		}
	}))
}

/// `/user/<name>/submitted.json`, as a post `Listing`.
async fn user_submitted(name: &str, path: &str) -> Result<Value, String> {
	let mut variables = json!({
		"username": name,
		"sort": param(path, "sort").map(|sort| sort.to_uppercase()).unwrap_or_else(|| "NEW".to_string()),
		"time": "ALL",
	});
	if let Some(after) = param(path, "after").filter(|after| !after.is_empty()) {
		variables["after"] = Value::String(after);
	}
	if let Some(range) = param(path, "t").filter(|range| !range.is_empty()) {
		variables["time"] = Value::String(range.to_uppercase());
	}

	let response = query(SUBMITTED_POSTS, SUBMITTED_POSTS_HASH, variables).await?;
	let elements = response["data"]["profileFeed"]["elements"]
		.as_object()
		.ok_or_else(|| format!("Unexpected GraphQL response | {path}"))?;

	let after = feed_cursor(elements).to_string();
	let posts = posts_of_elements(elements).await?;
	Ok(posts_listing(posts, &after, path))
}

/// `/r/<sub>/wiki/<page>.json`, in the API's wiki shape.
async fn wiki_page(subreddit: &str, page: &str) -> Result<Value, String> {
	let variables = json!({
		"subredditName": subreddit,
		"includePage": true,
		"includePageContent": true,
		"includePageTitle": true,
		"includeMediaFragment": false,
		"includeIsRevisable": false,
		"includeRevisionsCount": false,
		"pageName": page,
	});
	let response = query(SUBREDDIT_WIKI, SUBREDDIT_WIKI_HASH, variables).await?;
	let page_node = &response["data"]["subredditInfoByName"]["wiki"]["page"];

	// Wiki content is rich text JSON, not markdown.
	let mut content_html = rtjson_to_html(&page_node["content"]["richtext"]);
	if content_html.is_empty() {
		// Keep the page's own placeholder instead of rendering a blank page.
		content_html = "<h3>Wiki not found</h3>".to_string();
	}

	Ok(json!({
		"kind": "wikipage",
		"data": {
			"content_html": content_html,
			"title": page_node["pageTitle"].as_str().unwrap_or(page),
			"path": page,
		}
	}))
}

/// Renders Reddit's rich text JSON (RTJSON) into HTML.
///
/// Wiki pages and subreddit descriptions only ship `richtext`, so the blocks it
/// can contain are covered here: paragraphs, headings, rules, lists, links,
/// mentions and media.
fn rtjson_to_html(richtext: &Value) -> String {
	fn escape(text: &str) -> String {
		text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
	}

	fn inline(node: &Value, html: &mut String) {
		match node["e"].as_str().unwrap_or_default() {
			"text" | "raw" => html.push_str(&escape(node["t"].as_str().unwrap_or_default())),
			"link" => {
				let url = escape(node["u"].as_str().unwrap_or_default());
				let text = escape(node["t"].as_str().unwrap_or_default());
				html.push_str(&format!("<a href=\"{url}\">{text}</a>"));
			}
			"r/" | "u/" => {
				let mention = escape(node["t"].as_str().unwrap_or_default());
				html.push_str(&format!("<a href=\"/{mention}\">{mention}</a>"));
			}
			"emoji" => html.push_str(&escape(node["t"].as_str().unwrap_or_default())),
			"img" | "gif" => {
				if let Some(url) = node["u"].as_str().filter(|url| !url.is_empty()) {
					html.push_str(&format!("<img src=\"{}\" alt=\"\">", escape(url)));
				}
			}
			// Anything else: keep its children's text rather than dropping it.
			_ => blocks(node, html),
		}
	}

	fn blocks(node: &Value, html: &mut String) {
		for child in node["c"].as_array().into_iter().flatten() {
			inline(child, html);
		}
	}

	fn block(node: &Value, html: &mut String) {
		match node["e"].as_str().unwrap_or_default() {
			"par" => {
				html.push_str("<p>");
				blocks(node, html);
				html.push_str("</p>");
			}
			"h" => {
				let level = node["l"].as_u64().unwrap_or(1).clamp(1, 6);
				html.push_str(&format!("<h{level}>"));
				blocks(node, html);
				html.push_str(&format!("</h{level}>"));
			}
			"hr" => html.push_str("<hr>"),
			"list" => {
				let tag = if node["o"].as_bool().unwrap_or(false) { "ol" } else { "ul" };
				html.push_str(&format!("<{tag}>"));
				for item in node["c"].as_array().into_iter().flatten() {
					html.push_str("<li>");
					for child in item["c"].as_array().into_iter().flatten() {
						block(child, html);
					}
					html.push_str("</li>");
				}
				html.push_str(&format!("</{tag}>"));
			}
			_ => blocks(node, html),
		}
	}

	let Some(document) = richtext["document"].as_array() else {
		return escape(richtext.as_str().unwrap_or_default());
	};

	let mut html = String::new();
	for node in document {
		block(node, &mut html);
	}
	html
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
		// No default in the operation document, so it has to be sent explicitly.
		"filters": [],
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
	// `SubredditFeedElements` calls it `title`; `PostsByIds` (`postTitle: title`)
	// and the SDUI components use the alias.
	let title = match text("title") {
		title if title.is_empty() => text("postTitle"),
		title => title,
	};
	post.insert("title".into(), Value::String(title));
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

/// Comment tree of a post, as the API's comment `Listing`.
///
/// Reddit blocks `/comments/<id>.json` (and `/api/morechildren`) for anonymous
/// clients, so the tree comes from the app's `PostComments` operation. The
/// response is a flat pre-order list whose entries carry a `depth` and a
/// `parentId`, rebuilt here into the nested shape `parse_comments` expects.
pub async fn comments(post_id: &str, sort: &str, depth: u32) -> Result<Value, String> {
	let id = if post_id.starts_with("t3_") { post_id.to_string() } else { format!("t3_{post_id}") };

	let variables = json!({
		"id": id,
		"sortType": comment_sort(sort),
		// Ask for the whole tree up front: the API's separate "more comments"
		// request is blocked too, so anything beyond this arrives as a stub.
		"count": 200,
		"maxDepth": depth,
		"includeCommentsHtmlField": true,
		"includeAwards": true,
		"includeIsGildable": false,
		"includePremiumAvatarTreatment": false,
		"includeConsistencyToken": false,
	});

	let response = query(POST_COMMENTS, POST_COMMENTS_HASH, variables).await?;
	let forest = response["data"]["postInfoById"]["commentForest"]
		.as_object()
		.ok_or_else(|| "Unexpected GraphQL response for comments".to_string())?;

	Ok(comment_listing(forest))
}

/// redlib's comment sorts, mapped onto the app's `CommentSort` enum.
fn comment_sort(sort: &str) -> String {
	match sort.to_ascii_lowercase().as_str() {
		"top" => "TOP",
		"new" => "NEW",
		"controversial" => "CONTROVERSIAL",
		"old" => "OLD",
		"qa" => "QA",
		_ => "CONFIDENCE",
	}
	.to_string()
}

/// Rebuild a flat `CommentForest` into the API's nested comment `Listing`.
pub(crate) fn comment_listing(forest: &Map<String, Value>) -> Value {
	/// A legacy comment node plus the indices of its children.
	struct Node {
		data: Value,
		children: Vec<usize>,
	}

	fn build(nodes: &[Node], index: usize) -> Value {
		let replies: Vec<Value> = nodes[index].children.iter().map(|child| build(nodes, *child)).collect();
		let mut node = nodes[index].data.clone();
		node["data"]["replies"] = if replies.is_empty() {
			Value::String(String::new())
		} else {
			json!({ "kind": "Listing", "data": { "children": replies } })
		};
		node
	}

	let mut nodes: Vec<Node> = Vec::new();
	let mut roots: Vec<usize> = Vec::new();
	// Indices of the ancestors of the entry being read, by depth.
	let mut ancestors: Vec<usize> = Vec::new();

	let trees = forest["trees"].as_array().map(Vec::as_slice).unwrap_or_default();
	for entry in trees {
		let depth = entry["depth"].as_u64().unwrap_or_default() as usize;

		let (kind, data) = if let Some(comment) = entry.get("node").filter(|node| node.is_object()) {
			("t1", comment_data(comment))
		} else if let Some(more) = entry.get("more").filter(|more| more.is_object()) {
			// A "more comments" stub. `parse_comments` renders it as a link when it
			// hangs off a comment, and drops it when it hangs off the post.
			let data = json!({
				"id": more["cursor"].as_str().unwrap_or_default(),
				"parent_id": parent_id(entry["parentId"].as_str().unwrap_or_default()),
				"count": more["count"].as_i64().unwrap_or_default(),
			});
			("more", data)
		} else {
			continue;
		};

		let index = nodes.len();
		nodes.push(Node {
			data: json!({ "kind": kind, "data": data }),
			children: Vec::new(),
		});

		ancestors.truncate(depth);
		match ancestors.last() {
			Some(parent) => nodes[*parent].children.push(index),
			None => roots.push(index),
		}
		ancestors.push(index);
	}

	let children: Vec<Value> = roots.iter().map(|root| build(&nodes, *root)).collect();
	json!({ "kind": "Listing", "data": { "children": children } })
}

/// `parse_comments` splits a parent id on '_' and indexes both halves, so every
/// comment needs a `kind_id` shaped parent even when the server omits one.
fn parent_id(raw: &str) -> String {
	if raw.contains('_') { raw.to_string() } else { format!("t3_{raw}") }
}

/// Maps a GraphQL comment node onto the fields `build_comment` reads.
fn comment_data(node: &Value) -> Value {
	fn text(node: &Value, key: &str) -> String {
		node[key].as_str().unwrap_or_default().to_string()
	}

	fn flag(node: &Value, key: &str) -> bool {
		node[key].as_bool().unwrap_or(false)
	}

	// `content.html` is populated for only some comments, so the markdown Reddit
	// always sends is rendered when it is missing.
	let markdown = text(&node["content"], "markdown");
	let html = match text(&node["content"], "html") {
		html if html.is_empty() => markdown_to_html(&markdown),
		html => html,
	};

	// `distinguishedAs` is an enum object in some responses and a plain string in
	// others; the API and the templates use lower case.
	let distinguished = node["distinguishedAs"]
		.as_str()
		.or_else(|| node["distinguishedAs"]["as"].as_str())
		.unwrap_or_default()
		.to_ascii_lowercase();

	let created = DateTime::parse_from_str(&text(node, "createdAt"), "%Y-%m-%dT%H:%M:%S%.f%z")
		.map(|date| date.timestamp() as f64)
		.unwrap_or_default();
	let edited = DateTime::parse_from_str(&text(node, "editedAt"), "%Y-%m-%dT%H:%M:%S%.f%z")
		.map(|date| date.timestamp() as f64)
		.unwrap_or(0.0);

	let parent_id = parent_id(&text(node, "parentId"));

	// The app sends rich flair as a JSON string; the API uses an array.
	let rich_flair = match &node["authorFlair"]["richtext"] {
		Value::Array(items) => Value::Array(items.clone()),
		Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
		_ => Value::Null,
	};
	let author_flair = &node["authorFlair"];

	let awardings: Vec<Value> = node["awardings"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|total| {
			let award = &total["award"];
			let name = award["name"].as_str()?;
			Some(json!({
				"name": name,
				"description": award["description"].as_str().unwrap_or_default(),
				"resized_icons": [{ "url": award["static_icon_16"]["url"].as_str().unwrap_or_default() }],
				"count": 1,
			}))
		})
		.collect();

	json!({
		"id": text(node, "id").trim_start_matches("t1_").to_string(),
		"name": text(node, "id"),
		"parent_id": parent_id,
		"author": node["authorInfo"]["name"].as_str().unwrap_or("[deleted]"),
		"body": markdown,
		"body_html": html,
		"created_utc": created,
		"edited": if edited > 0.0 { json!(edited) } else { json!(false) },
		"score": node["score"].clone(),
		"score_hidden": flag(node, "isScoreHidden"),
		"stickied": flag(node, "isStickied"),
		"distinguished": distinguished,
		"author_flair_type": author_flair["type"].as_str().unwrap_or_default(),
		"author_flair_text": author_flair["text"].as_str().unwrap_or_default(),
		"author_flair_richtext": rich_flair,
		"author_flair_background_color": author_flair["template"]["backgroundColor"].as_str().unwrap_or_default(),
		"author_flair_text_color": author_flair["textColor"].as_str().unwrap_or_default(),
		"all_awardings": awardings,
	})
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
	fn test_handles_claims_every_graphql_path() {
		for path in [
			"/r/rust/new.json?raw_json=1",
			"/r/rust/rising.json?raw_json=1",
			"/r/rust/hot.json?raw_json=1",
			"/r/rust/top.json?t=week&raw_json=1",
			"/r/rust/controversial.json?raw_json=1",
			"/r/popular/hot.json?raw_json=1&geo_filter=GLOBAL",
			"/r/all/top.json?raw_json=1",
			"/search.json?q=rust&raw_json=1",
			"/r/rust/search.json?q=async&restrict_sr=on",
			"/r/rust/about.json?raw_json=1",
			"/r/rust/wiki/rules.json?raw_json=1",
			"/r/rust/wiki/index.json?raw_json=1",
			"/user/spez/about.json?raw_json=1",
			"/user/spez/submitted.json?sort=new&raw_json=1",
			"/by_id/t3_1xocux0.json?raw_json=1",
		] {
			assert!(handles(path), "{path}");
		}
	}

	#[test]
	fn test_handles_leaves_unsupported_paths_on_rest() {
		for path in [
			// Multireddit and subscription feeds have no persisted operation.
			"/r/rust+python/hot.json?raw_json=1",
			"/r/rust%2Bpython/hot.json?raw_json=1",
			// The user comment listing is trunkated to a preview by its operation.
			"/user/spez/comments.json?raw_json=1",
			// No operation, and still served by the JSON API.
			"/duplicates/1xocux0.json?raw_json=1",
			"/api/v1/me",
			"/comments/1xocux0.json",
			"/r/rust/comments/1xocux0.json",
			"/r/rust/wiki/config/description.json",
		] {
			assert!(!handles(path), "{path}");
		}
	}

	#[test]
	fn test_path_parsing() {
		assert_eq!(by_id_path("/by_id/t3_abc.json"), Some("abc".to_string()));
		assert_eq!(by_id_path("/by_id/t3_.json"), None);
		assert_eq!(by_id_path("/by_id/t1_abc.json"), None);

		assert!(matches!(user_path("/user/spez/about.json"), Some(UserRequest::About(name)) if name == "spez"));
		assert!(matches!(user_path("/user/spez/submitted.json"), Some(UserRequest::Submitted(name)) if name == "spez"));
		assert!(user_path("/user/spez/comments.json").is_none());

		assert_eq!(split_subreddit("/r/rust/hot.json"), Some(("rust", "hot.json")));
		assert_eq!(split_subreddit("/r/rust"), None);
	}

	#[test]
	fn test_rtjson_renders_blocks() {
		let richtext = json!({ "document": [
			{ "e": "h", "l": 2, "c": [{ "e": "raw", "t": "Rules" }] },
			{ "e": "par", "c": [
				{ "e": "text", "t": "Read the " },
				{ "e": "link", "t": "code of conduct", "u": "https://example.com/coc" },
				{ "e": "text", "t": " first." }
			] },
			{ "e": "hr" },
			{ "e": "list", "o": false, "c": [
				{ "e": "li", "c": [{ "e": "par", "c": [{ "e": "text", "t": "be nice" }] }] },
				{ "e": "li", "c": [{ "e": "par", "c": [{ "e": "r/", "t": "r/rust" }] }] }
			] }
		]});

		let html = rtjson_to_html(&richtext);
		assert!(html.contains("<h2>Rules</h2>"), "{html}");
		assert!(html.contains("<p>Read the <a href=\"https://example.com/coc\">code of conduct</a> first.</p>"), "{html}");
		assert!(html.contains("<hr>"), "{html}");
		assert!(html.contains("<ul><li><p>be nice</p></li><li><p><a href=\"/r/rust\">r/rust</a></p></li></ul>"), "{html}");
	}

	#[test]
	fn test_rtjson_escapes_text_and_keeps_plain_strings() {
		assert_eq!(rtjson_to_html(&json!("<script>")), "&lt;script&gt;");

		let richtext = json!({ "document": [{ "e": "par", "c": [{ "e": "text", "t": "a < b" }] }] });
		assert_eq!(rtjson_to_html(&richtext), "<p>a &lt; b</p>");
	}

	#[test]
	fn test_posts_listing_echoes_the_geo_filter() {
		let listing = posts_listing(Vec::new(), "", "/r/popular/hot.json?raw_json=1&geo_filter=GLOBAL");
		assert_eq!(listing["data"]["geo_filter"], "GLOBAL");
		assert!(listing["data"]["after"].is_null());

		let listing = posts_listing(Vec::new(), "", "/r/rust/hot.json?raw_json=1");
		assert!(listing["data"].get("geo_filter").is_none());
	}

	#[test]
	fn test_posts_listing_marks_the_next_cursor() {
		let posts = vec![json!({ "id": "t3_abc", "title": "Hello", "createdAt": "2026-11-02T08:54:01.782000+0000" })];
		let listing = posts_listing(posts, "dDNfYWJj", "/r/rust/hot.json");
		assert_eq!(listing["data"]["after"], "dDNfYWJj");
		assert_eq!(listing["data"]["children"][0]["data"]["id"], "abc");
		assert_eq!(listing["data"]["dist"], 1);
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
	fn test_posts_by_ids_title_alias() {
		// `PostsByIds` returns the title as `postTitle`, the feed operation as `title`.
		let node: Map<String, Value> = serde_json::from_value(json!({
			"id": "t3_abc",
			"postTitle": "Aliased",
			"createdAt": "2026-11-02T08:54:01.782000+0000",
			"content": { "markdown": "body" },
			"authorInfo": { "name": "alice" },
			"subreddit": { "name": "rust" }
		}))
		.unwrap();

		assert_eq!(to_post(&node)["title"], "Aliased");
	}

	#[test]
	fn test_comment_sorts_map_onto_the_app_enum() {
		assert_eq!(comment_sort("confidence"), "CONFIDENCE");
		assert_eq!(comment_sort("Top"), "TOP");
		assert_eq!(comment_sort("new"), "NEW");
		assert_eq!(comment_sort("controversial"), "CONTROVERSIAL");
		assert_eq!(comment_sort("old"), "OLD");
		assert_eq!(comment_sort("qa"), "QA");
		// Unknown sorts must still be a value the server accepts.
		assert_eq!(comment_sort("best"), "CONFIDENCE");
	}

	#[test]
	fn test_comment_forest_is_nested_by_depth() {
		// Pre-order, as `PostComments` returns it: the `depth` field carries the
		// nesting, and a `more` entry replaces the node it stands for.
		let forest: Map<String, Value> = serde_json::from_value(json!({
			"appliedSort": "CONFIDENCE",
			"pageInfo": { "commentCount": 3, "hasNextPage": false },
			"trees": [
				{ "depth": 0, "parentId": null, "node": { "id": "t1_a", "parentId": "t3_post", "score": 5, "createdAt": "2026-11-02T08:54:01.000000+0000", "content": { "markdown": "root" } } },
				{ "depth": 1, "parentId": "t1_a", "node": { "id": "t1_a1", "parentId": "t1_a", "score": 1, "createdAt": "2026-11-02T08:55:01.000000+0000", "content": { "markdown": "child" } } },
				{ "depth": 2, "parentId": "t1_a1", "more": { "count": 4, "cursor": "c1", "isTooDeepForCount": false } },
				{ "depth": 0, "parentId": null, "more": { "count": 9, "cursor": "c2", "isTooDeepForCount": false } },
				{ "depth": 0, "parentId": null, "node": { "id": "t1_b", "parentId": "t3_post", "score": 2, "createdAt": "2026-11-02T08:56:01.000000+0000", "content": { "markdown": "second" } } }
			]
		}))
		.unwrap();

		let listing = comment_listing(&forest);
		let roots = listing["data"]["children"].as_array().unwrap();
		assert_eq!(roots.len(), 3);
		assert_eq!(roots[0]["kind"], "t1");
		assert_eq!(roots[0]["data"]["id"], "a");
		// A placeholder without a parent still needs a `kind_id` shaped parent:
		// the comment parser indexes both halves of the split.
		assert_eq!(roots[1]["kind"], "more");
		assert_eq!(roots[1]["data"]["parent_id"], "t3_");

		let replies = roots[0]["data"]["replies"]["data"]["children"].as_array().unwrap();
		assert_eq!(replies.len(), 1);
		assert_eq!(replies[0]["data"]["id"], "a1");

		let deeper = replies[0]["data"]["replies"]["data"]["children"].as_array().unwrap();
		assert_eq!(deeper.len(), 1);
		assert_eq!(deeper[0]["kind"], "more");
		assert_eq!(deeper[0]["data"]["count"], 4);
		assert_eq!(deeper[0]["data"]["parent_id"], "t1_a1");

		// A comment without children still has to look like an empty Listing
		// string, which is what `parse_comments` tests for.
		assert_eq!(roots[2]["data"]["id"], "b");
		assert_eq!(roots[2]["data"]["replies"], "");
	}

	#[test]
	fn test_comment_node_is_mapped_to_api_shape() {
		let node: Value = json!({
			"id": "t1_abc",
			"parentId": "t1_parent",
			"authorInfo": { "name": "alice" },
			"content": { "markdown": "**bold**", "html": null },
			"score": 12,
			"isScoreHidden": true,
			"isStickied": true,
			"createdAt": "2026-11-02T08:54:01.000000+0000",
			"editedAt": "2026-11-02T09:54:01.000000+0000",
			"distinguishedAs": "MODERATOR",
			"authorFlair": { "text": "crab", "type": "richtext", "textColor": "dark", "richtext": "[{\"e\":\"text\",\"t\":\"hi\"}]", "template": { "backgroundColor": "#ffe600" } },
			"awardings": [{ "award": { "name": "Bravo", "description": "nice", "static_icon_16": { "url": "https://example.com/bravo.png" } } }]
		});

		let data = comment_data(&node);
		assert_eq!(data["id"], "abc");
		assert_eq!(data["parent_id"], "t1_parent");
		assert_eq!(data["author"], "alice");
		assert_eq!(data["score"], 12);
		assert!(data["score_hidden"].as_bool().unwrap());
		assert!(data["stickied"].as_bool().unwrap());
		// Templates style the author by this class and redlib collapses stickied
		// moderator comments, so it has to be the API's lower-case value.
		assert_eq!(data["distinguished"], "moderator");
		assert_eq!(data["author_flair_text"], "crab");
		assert_eq!(data["author_flair_background_color"], "#ffe600");
		assert_eq!(data["author_flair_richtext"][0]["t"], "hi");
		assert_eq!(data["all_awardings"][0]["name"], "Bravo");
		assert_eq!(data["all_awardings"][0]["resized_icons"][0]["url"], "https://example.com/bravo.png");
		// `content.html` is null here, so the markdown is rendered instead.
		assert!(data["body_html"].as_str().unwrap().contains("<strong>bold</strong>"));
		assert!(data["created_utc"].as_f64().unwrap() > 1_700_000_000.0);
		assert!(data["edited"].as_f64().unwrap() > 1_700_000_000.0);
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
