//! Shrinks large JSON tool results by what tells a model nothing: `node_id`s, HTML copies of text
//! that is there as plain text, empty values and, in results of known noisy sources (GitHub,
//! Linear), avatar and image URLs, colours and API links.
//!
//! Pure and deterministic: the same text gives the same result, so the answer can be decided once
//! when a tool result is produced and never has to be redone.

use serde_json::{Map, Value};

/// JSON shorter than this is left alone: there is little to gain, and a short answer is often
/// one whose every field matters.
pub const MIN_CHARS: usize = 2_000;

/// What [`slim_text`] made of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slimmed {
    /// The result as compact JSON, without the noise.
    pub text: String,
    /// How many fields and list items were removed; none when only the layout was made compact.
    pub dropped: usize,
}

/// The slimmed form of `text` as compact JSON, when `text` is a JSON object or array of at least
/// [`MIN_CHARS`] characters and slimming makes it shorter. `None` leaves the text as it is.
pub fn slim_text(text: &str) -> Option<Slimmed> {
    if text.len() < MIN_CHARS {
        return None;
    }
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let mut value: Value = serde_json::from_str(trimmed).ok()?;
    let dropped = slim(&mut value);
    let slimmed = serde_json::to_string(&value).ok()?;
    (slimmed.len() < text.len()).then_some(Slimmed {
        text: slimmed,
        dropped,
    })
}

/// `text` as indented JSON, one field to a line, so notes can cite line ranges. `None` when
/// `text` is not a JSON object or array.
pub fn pretty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

/// Removes the noise from `value` in place and says how many fields and items went.
pub fn slim(value: &mut Value) -> usize {
    let noisy = noisy_source(value);
    slim_value(value, noisy)
}

fn slim_value(value: &mut Value, noisy: bool) -> usize {
    match value {
        Value::Object(map) => slim_object(map, noisy),
        Value::Array(items) => {
            let mut dropped = 0;
            for item in items.iter_mut() {
                dropped += slim_value(item, noisy);
            }
            let before = items.len();
            items.retain(|item| !is_empty(item));
            dropped + before - items.len()
        }
        _ => 0,
    }
}

fn slim_object(map: &mut Map<String, Value>, noisy: bool) -> usize {
    let drop: Vec<String> = map
        .iter()
        .filter(|(key, value)| noise(key, value, map, noisy))
        .map(|(key, _)| key.clone())
        .collect();
    let mut dropped = drop.len();
    for key in drop {
        map.shift_remove(&key);
    }
    for value in map.values_mut() {
        dropped += slim_value(value, noisy);
    }
    let before = map.len();
    map.retain(|_, value| !is_empty(value));
    dropped + before - map.len()
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Whether `value` comes from an API known to pad its objects with pictures, colours and links
/// to itself: GitHub (`node_id`, `html_url`, `api.github.com`) or Linear (`linear.app`). Elsewhere
/// a colour or an image link may be what was asked for.
fn noisy_source(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(key.as_str(), "node_id" | "html_url" | "gravatar_id") || noisy_source(value)
        }),
        Value::Array(items) => items.iter().any(noisy_source),
        Value::String(s) => {
            let rest = s.split_once("://").map_or("", |(_, rest)| rest);
            let host = rest.split('/').next().unwrap_or("");
            host == "api.github.com" || host == "linear.app" || host.ends_with(".linear.app")
        }
        _ => false,
    }
}

/// Whether field `key` of `siblings` says nothing the model needs.
fn noise(key: &str, value: &Value, siblings: &Map<String, Value>, noisy: bool) -> bool {
    let lower = key.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "node_id" | "nodeid" | "gravatar_id" | "gravatarid" | "_links"
    ) || lower.ends_with("_node_id")
        || lower.ends_with("nodeid")
    {
        return true;
    }
    if noisy && is_colour(&lower) {
        return true;
    }
    if noisy
        && is_link(&lower)
        && let Value::String(s) = value
    {
        return media_link(&lower) || api_link(s) || (lower == "url" && siblings.contains_key("html_url"));
    }
    html_copy(key, siblings)
}

fn is_colour(lower: &str) -> bool {
    matches!(lower, "color" | "colour")
        || lower.ends_with("_color")
        || lower.ends_with("_colour")
        || lower.ends_with("color")
        || lower.ends_with("colour")
}

/// A key that holds a link.
fn is_link(lower: &str) -> bool {
    lower == "url" || lower.ends_with("_url") || lower.ends_with("url") || lower.ends_with("_href")
}

/// A link to a picture: an avatar, an icon, a thumbnail.
fn media_link(lower: &str) -> bool {
    [
        "avatar", "gravatar", "icon", "thumb", "emoji", "badge", "logo", "image", "img", "banner", "cover",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// A link into an API: what the page links to for the same thing is shown instead, and the model
/// has the API's name for it anyway. URL templates (`…/repos{/name}`) are never useful.
fn api_link(url: &str) -> bool {
    if url.contains('{') {
        return true;
    }
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    host.starts_with("api.")
        || path.starts_with("api/")
        || path.starts_with("repos/")
        || path.starts_with("rest/")
}

/// `body_html` or `bodyHtml` next to `body`: the same text, marked up.
fn html_copy(key: &str, siblings: &Map<String, Value>) -> bool {
    let plain = key
        .strip_suffix("_html")
        .or_else(|| key.strip_suffix("Html"))
        .or_else(|| key.strip_suffix("HTML"));
    plain.is_some_and(|plain| !plain.is_empty() && siblings.contains_key(plain))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    fn user(login: &str) -> Value {
        json!({
            "login": login,
            "id": 583231,
            "node_id": "MDQ6VXNlcjU4MzIzMQ==",
            "avatar_url": format!("https://avatars.githubusercontent.com/u/583231?v=4&login={login}"),
            "gravatar_id": "",
            "url": format!("https://api.github.com/users/{login}"),
            "html_url": format!("https://github.com/{login}"),
            "followers_url": format!("https://api.github.com/users/{login}/followers"),
            "following_url": format!("https://api.github.com/users/{login}/following{{/other_user}}"),
            "repos_url": format!("https://api.github.com/users/{login}/repos"),
            "type": "User",
            "site_admin": false
        })
    }

    fn github_issues() -> String {
        let issues: Vec<Value> = (1..=12)
            .map(|n| {
                json!({
                    "url": format!("https://api.github.com/repos/octo/hello/issues/{n}"),
                    "repository_url": "https://api.github.com/repos/octo/hello",
                    "comments_url": format!("https://api.github.com/repos/octo/hello/issues/{n}/comments"),
                    "html_url": format!("https://github.com/octo/hello/issues/{n}"),
                    "id": 1000 + n,
                    "node_id": format!("I_kwDOA{n}"),
                    "number": n,
                    "title": format!("Crash number {n} when the config is empty"),
                    "user": user("octocat"),
                    "labels": [{
                        "id": 208045946, "node_id": "MDU6TGFiZWwyMDgwNDU5NDY=", "name": "bug",
                        "color": "d73a4a", "default": true, "description": "Something isn't working",
                        "url": "https://api.github.com/repos/octo/hello/labels/bug"
                    }],
                    "state": "open",
                    "locked": false,
                    "assignee": null,
                    "assignees": [],
                    "milestone": null,
                    "comments": 3,
                    "body": format!("Steps to reproduce {n}: run it with no config."),
                    "body_html": format!("<p>Steps to reproduce {n}: run it with no config.</p>"),
                    "reactions": { "url": "https://api.github.com/repos/octo/hello/issues/1/reactions", "total_count": 0, "+1": 0 },
                    "performed_via_github_app": null
                })
            })
            .collect();
        serde_json::to_string_pretty(&issues).unwrap()
    }

    #[test]
    fn github_issues_lose_the_noise_and_keep_what_matters() {
        let text = github_issues();
        let slim = slim_text(&text).unwrap().text;
        assert!(slim.len() * 2 < text.len(), "{} of {}", slim.len(), text.len());
        let value: Value = serde_json::from_str(&slim).unwrap();
        let first = &value[0];
        assert_eq!(first["number"], 1);
        assert_eq!(first["title"], "Crash number 1 when the config is empty");
        assert_eq!(first["body"], "Steps to reproduce 1: run it with no config.");
        assert_eq!(first["html_url"], "https://github.com/octo/hello/issues/1");
        assert_eq!(first["state"], "open");
        assert_eq!(first["user"]["login"], "octocat");
        assert_eq!(first["labels"][0]["name"], "bug");
        // False and zero are information; nulls and empty lists are not.
        assert_eq!(first["locked"], false);
        assert_eq!(first["comments"], 3);
        assert_eq!(first["reactions"]["total_count"], 0);
        for gone in [
            "node_id",
            "body_html",
            "assignee",
            "assignees",
            "milestone",
            "repository_url",
        ] {
            assert!(first.get(gone).is_none(), "{gone} stays");
        }
        assert!(first["user"].get("avatar_url").is_none());
        assert!(first["labels"][0].get("color").is_none());
        assert!(first["reactions"].get("url").is_none());
    }

    #[test]
    fn linear_style_issues_lose_avatars_colours_and_empty_fields() {
        let nodes: Vec<Value> = (1..=20)
            .map(|n| {
                json!({
                    "id": format!("7f3a-{n}"),
                    "identifier": format!("ENG-{n}"),
                    "title": format!("Improve onboarding step {n}"),
                    "description": "Users drop off at the second screen.",
                    "descriptionHtml": "<p>Users drop off at the second screen.</p>",
                    "url": format!("https://linear.app/acme/issue/ENG-{n}"),
                    "priority": 2,
                    "estimate": null,
                    "dueDate": null,
                    "state": { "name": "In Progress", "color": "#f2c94c", "type": "started" },
                    "assignee": {
                        "name": "Ada", "email": "ada@acme.dev",
                        "avatarUrl": "https://public.linear.app/avatars/ada.png", "displayName": "ada"
                    },
                    "labels": { "nodes": [{ "name": "growth", "color": "#26b5ce" }] },
                    "children": { "nodes": [] },
                    "attachments": { "nodes": [] }
                })
            })
            .collect();
        let text =
            serde_json::to_string_pretty(&json!({ "data": { "issues": { "nodes": nodes } } })).unwrap();
        let slim = slim_text(&text).unwrap().text;
        let value: Value = serde_json::from_str(&slim).unwrap();
        let node = &value["data"]["issues"]["nodes"][0];
        assert_eq!(node["identifier"], "ENG-1");
        assert_eq!(node["url"], "https://linear.app/acme/issue/ENG-1");
        assert_eq!(node["state"]["name"], "In Progress");
        assert_eq!(node["assignee"]["email"], "ada@acme.dev");
        assert_eq!(node["labels"]["nodes"][0]["name"], "growth");
        for gone in [
            "descriptionHtml",
            "estimate",
            "dueDate",
            "children",
            "attachments",
        ] {
            assert!(node.get(gone).is_none(), "{gone} stays");
        }
        assert!(node["assignee"].get("avatarUrl").is_none());
        assert!(node["state"].get("color").is_none());
        assert!(slim.len() * 2 < text.len());
    }

    #[test]
    fn html_is_kept_when_there_is_no_plain_copy() {
        let mut value = json!({ "body_html": "<p>only copy</p>", "title": "t" });
        slim(&mut value);
        assert_eq!(value["body_html"], "<p>only copy</p>");
    }

    #[test]
    fn urls_that_are_not_api_noise_stay() {
        let mut value = json!({
            "url": "https://example.com/post/1",
            "download_url": "https://example.com/file.zip",
            "homepage": "https://example.com"
        });
        slim(&mut value);
        assert_eq!(value["url"], "https://example.com/post/1");
        assert_eq!(value["download_url"], "https://example.com/file.zip");
        assert_eq!(value["homepage"], "https://example.com");
    }

    #[test]
    fn colours_and_images_stay_outside_the_known_noisy_sources() {
        let themes: Vec<Value> = (0..40)
            .map(|n| {
                json!({
                    "name": format!("theme {n}"),
                    "primary_color": "#aabbcc",
                    "background": { "color": "#101010", "image_url": format!("https://cdn.example.com/bg/{n}.png") },
                    "logo_url": "https://example.com/logo.svg",
                    "api": "https://api.example.com/v1/themes"
                })
            })
            .collect();
        let text = serde_json::to_string_pretty(&themes).unwrap();
        let slimmed = slim_text(&text).unwrap();
        assert_eq!(slimmed.dropped, 0);
        let value: Value = serde_json::from_str(&slimmed.text).unwrap();
        assert_eq!(value[0]["primary_color"], "#aabbcc");
        assert_eq!(value[0]["background"]["color"], "#101010");
        assert_eq!(
            value[3]["background"]["image_url"],
            "https://cdn.example.com/bg/3.png"
        );
        assert_eq!(value[0]["logo_url"], "https://example.com/logo.svg");
        // Empty values are noise anywhere, and are counted.
        let mut value = json!({ "color": "red", "empty": [], "none": null, "n": 1 });
        assert_eq!(slim(&mut value), 2);
        assert_eq!(value, json!({ "color": "red", "n": 1 }));
        // An object that carries `node_id` is GitHub-shaped: its colours go too.
        let mut value = json!({ "node_id": "x", "color": "red", "n": 1 });
        assert_eq!(slim(&mut value), 2);
        assert_eq!(value, json!({ "n": 1 }));
    }

    #[test]
    fn the_count_of_dropped_fields_is_reported() {
        let slimmed = slim_text(&github_issues()).unwrap();
        assert!(slimmed.dropped > 12 * 5, "{}", slimmed.dropped);
    }

    #[test]
    fn small_and_non_json_text_is_left_alone() {
        assert_eq!(slim_text(r#"{"avatar_url":"x","a":1}"#), None);
        let prose = "word ".repeat(2_000);
        assert_eq!(slim_text(&prose), None);
        let broken = format!("{{\"a\": {}", "1,".repeat(2_000));
        assert_eq!(slim_text(&broken), None);
    }

    #[test]
    fn json_with_nothing_to_drop_and_no_whitespace_is_left_alone() {
        let items: Vec<Value> = (0..200)
            .map(|n| json!({ "name": format!("item {n}"), "count": n + 1 }))
            .collect();
        let text = serde_json::to_string(&items).unwrap();
        assert!(text.len() > MIN_CHARS);
        assert_eq!(slim_text(&text), None);
    }

    #[test]
    fn slimming_is_deterministic_and_idempotent() {
        let text = github_issues();
        let once = slim_text(&text).unwrap();
        assert_eq!(slim_text(&text).unwrap(), once);
        assert_eq!(slim_text(&once.text), None);
    }

    #[test]
    fn pretty_puts_one_field_on_a_line() {
        let text = pretty(r#"{"a":1,"b":[2,3]}"#).unwrap();
        assert!(text.lines().count() > 4);
        assert_eq!(pretty("not json"), None);
    }
}
