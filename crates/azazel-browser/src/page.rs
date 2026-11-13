//! What the browser tool works out by itself: the accessibility snapshot, key names, element
//! geometry and the host of a URL.

use std::collections::HashMap;

use serde_json::Value;

/// Lines a snapshot may have; the rest of the page is summarised in one more line.
pub const MAX_SNAPSHOT_LINES: usize = 400;

/// Characters of an element's name or value shown in a snapshot.
const MAX_NAME: usize = 120;

#[derive(Debug, Default, PartialEq)]
pub struct Snapshot {
    pub text: String,
    /// Ref number (`e<n>`) to the DOM node behind it (`backendDOMNodeId`).
    pub refs: HashMap<u32, i64>,
}

fn interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "checkbox"
            | "radio"
            | "combobox"
            | "listbox"
            | "option"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "switch"
            | "slider"
            | "spinbutton"
            | "treeitem"
    )
}

/// One line of whitespace-collapsed text, cut at [`MAX_NAME`] characters.
fn label(value: Option<&Value>) -> String {
    let text = match value.and_then(|v| v.get("value")) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let mut out = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > MAX_NAME {
        out = out.chars().take(MAX_NAME).collect::<String>() + "…";
    }
    out.replace('"', "'")
}

/// Renders the nodes of `Accessibility.getFullAXTree`: one line per node that has a name or can be
/// interacted with, `- <role> "<name>" [ref=e<n>]`, indented under the listed node that contains it.
pub fn snapshot(nodes: &[Value]) -> Snapshot {
    let id_of = |node: &Value| node.get("nodeId").and_then(Value::as_str).map(str::to_string);
    let by_id: HashMap<String, &Value> = nodes
        .iter()
        .filter_map(|node| Some((id_of(node)?, node)))
        .collect();
    let Some(root) = nodes
        .iter()
        .find(|node| node.get("parentId").is_none())
        .or(nodes.first())
        .and_then(id_of)
    else {
        return Snapshot::default();
    };

    let mut out = Snapshot::default();
    let (mut lines, mut omitted) = (0usize, 0usize);
    // (node id, listed ancestors, name of the nearest listed ancestor)
    let mut stack = vec![(root, 0usize, String::new())];
    while let Some((id, depth, parent_name)) = stack.pop() {
        let Some(node) = by_id.get(&id) else {
            continue;
        };
        let role = node
            .get("role")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = label(node.get("name"));
        let ignored = node.get("ignored").and_then(Value::as_bool).unwrap_or(false);
        let listed = !ignored
            && !matches!(role, "none" | "presentation" | "InlineTextBox" | "LineBreak")
            && (interactive(role) || !name.is_empty())
            // The text of a button or link repeats its name.
            && !(role == "StaticText" && name == parent_name);

        let (child_depth, child_parent) = if listed {
            if lines < MAX_SNAPSHOT_LINES {
                let mut line = format!("{}- {role} \"{name}\"", "  ".repeat(depth));
                let value = label(node.get("value"));
                if !value.is_empty() && value != name {
                    line.push_str(&format!(" value=\"{value}\""));
                }
                if let Some(dom) = node.get("backendDOMNodeId").and_then(Value::as_i64) {
                    let number = out.refs.len() as u32 + 1;
                    out.refs.insert(number, dom);
                    line.push_str(&format!(" [ref=e{number}]"));
                }
                out.text.push_str(&line);
                out.text.push('\n');
                lines += 1;
            } else {
                omitted += 1;
            }
            (depth + 1, name)
        } else {
            (depth, parent_name)
        };
        if let Some(children) = node.get("childIds").and_then(Value::as_array) {
            for child in children.iter().rev().filter_map(Value::as_str) {
                stack.push((child.to_string(), child_depth, child_parent.clone()));
            }
        }
    }
    if omitted > 0 {
        out.text.push_str(&format!("… {omitted} more nodes not shown\n"));
    }
    out
}

/// What the `wait` action waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// JavaScript that is true in the page once the condition holds.
    Script(String),
    /// No network activity.
    Idle,
    /// A node with this role in the accessibility tree, and this name when one is given.
    Role { role: String, name: Option<String> },
}

/// Reads the `for` of a `wait`: `load`, `idle`, `text:<string>`, `role:<role> "<name>"`,
/// `url:contains:<s>`, `title:contains:<s>`, `gone:<selector>`, or a CSS selector.
pub fn condition(wanted: &str) -> Result<Condition, String> {
    let quoted = |text: &str| Value::String(text.to_string()).to_string();
    let script = |script: String| Ok(Condition::Script(script));
    if wanted == "load" {
        script("document.readyState === 'complete'".to_string())
    } else if wanted == "idle" {
        Ok(Condition::Idle)
    } else if let Some(text) = wanted.strip_prefix("text:") {
        script(format!(
            "!!document.body && document.body.innerText.includes({})",
            quoted(text)
        ))
    } else if let Some(part) = wanted.strip_prefix("url:contains:") {
        script(format!("location.href.includes({})", quoted(part)))
    } else if let Some(part) = wanted.strip_prefix("title:contains:") {
        script(format!("document.title.includes({})", quoted(part)))
    } else if let Some(selector) = wanted.strip_prefix("gone:") {
        script(format!(
            "document.querySelector({}) === null",
            quoted(selector.trim())
        ))
    } else if let Some(rest) = wanted.strip_prefix("role:") {
        let rest = rest.trim();
        let (role, name) = match rest.split_once(char::is_whitespace) {
            Some((role, name)) => (role, name.trim()),
            None => (rest, ""),
        };
        if role.is_empty() {
            return Err(format!(
                "`{wanted}` names no role; write it as in a snapshot line: role:button \"Save\""
            ));
        }
        let name = name
            .strip_prefix('"')
            .and_then(|name| name.strip_suffix('"'))
            .unwrap_or(name);
        Ok(Condition::Role {
            role: role.to_string(),
            name: (!name.is_empty()).then(|| name.to_string()),
        })
    } else {
        script(format!("document.querySelector({}) !== null", quoted(wanted)))
    }
}

/// Whether the nodes of `Accessibility.getFullAXTree` hold one with `role` and, when given, the
/// `name` a snapshot shows for it.
pub fn has_role(nodes: &[Value], role: &str, name: Option<&str>) -> bool {
    nodes.iter().any(|node| {
        let ignored = node.get("ignored").and_then(Value::as_bool).unwrap_or(false);
        let node_role = node
            .get("role")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        !ignored && node_role == role && name.is_none_or(|name| label(node.get("name")) == name)
    })
}

/// The number in a ref such as `e12`.
pub fn ref_number(reference: &str) -> Option<u32> {
    reference.trim().strip_prefix('e')?.parse().ok()
}

pub const ALT: u8 = 1;
pub const CONTROL: u8 = 2;
pub const META: u8 = 4;
pub const SHIFT: u8 = 8;

/// One key press as `Input.dispatchKeyEvent` wants it.
#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub modifiers: u8,
    pub key: String,
    pub code: String,
    /// Windows virtual key code; 0 when the key has none we know of.
    pub vk: u32,
    /// What the key types, when it types something and no command modifier is held.
    pub text: Option<String>,
}

/// `(key, code, virtual key code, text)` of the keys that have a name.
fn named(name: &str) -> Option<(&'static str, &'static str, u32, Option<&'static str>)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => ("Enter", "Enter", 13, Some("\r")),
        "tab" => ("Tab", "Tab", 9, None),
        "escape" | "esc" => ("Escape", "Escape", 27, None),
        "backspace" => ("Backspace", "Backspace", 8, None),
        "delete" | "del" => ("Delete", "Delete", 46, None),
        "insert" => ("Insert", "Insert", 45, None),
        "space" => (" ", "Space", 32, Some(" ")),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38, None),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40, None),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37, None),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39, None),
        "home" => ("Home", "Home", 36, None),
        "end" => ("End", "End", 35, None),
        "pageup" => ("PageUp", "PageUp", 33, None),
        "pagedown" => ("PageDown", "PageDown", 34, None),
        _ => return None,
    })
}

/// Parses `Enter`, `a`, `Control+A`, `Shift+ArrowDown`, `F5`: modifiers joined to one key by `+`.
pub fn parse_keys(keys: &str) -> Result<Key, String> {
    let keys = keys.trim();
    // `Control++` presses the plus key.
    let (modifier_part, name) = match keys.strip_suffix("++") {
        Some(rest) => (rest, "+"),
        None if keys == "+" => ("", "+"),
        None => keys.rsplit_once('+').unwrap_or(("", keys)),
    };
    if name.is_empty() {
        return Err(format!("`{keys}` names no key"));
    }
    let mut modifiers = 0u8;
    for modifier in modifier_part.split('+').filter(|m| !m.is_empty()) {
        modifiers |= match modifier.to_ascii_lowercase().as_str() {
            "alt" | "option" => ALT,
            "control" | "ctrl" => CONTROL,
            "meta" | "cmd" | "command" | "super" | "win" => META,
            "shift" => SHIFT,
            _ => return Err(format!("unknown modifier `{modifier}` in `{keys}`")),
        };
    }
    let types = modifiers & (ALT | CONTROL | META) == 0;

    if let Some((key, code, vk, text)) = named(name) {
        return Ok(Key {
            modifiers,
            key: key.to_string(),
            code: code.to_string(),
            vk,
            text: text.filter(|_| types).map(str::to_string),
        });
    }
    let lower = name.to_ascii_lowercase();
    if let Some(number) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=12).contains(&number)
    {
        return Ok(Key {
            modifiers,
            key: format!("F{number}"),
            code: format!("F{number}"),
            vk: 111 + number,
            text: None,
        });
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(format!("unknown key `{name}` in `{keys}`"));
    };
    let (key, code, vk) = if c.is_ascii_alphabetic() {
        let upper = c.to_ascii_uppercase();
        let typed = if modifiers & SHIFT != 0 { upper } else { c };
        (typed.to_string(), format!("Key{upper}"), upper as u32)
    } else if c.is_ascii_digit() {
        (c.to_string(), format!("Digit{c}"), c as u32)
    } else {
        (c.to_string(), String::new(), 0)
    };
    Ok(Key {
        modifiers,
        text: types.then(|| key.clone()),
        key,
        code,
        vk,
    })
}

/// The centre of a quad given as `[x1, y1, x2, y2, x3, y3, x4, y4]`.
pub fn centre(quad: &[f64]) -> Option<(f64, f64)> {
    let (x, y, width, height) = bounds(quad)?;
    Some((x + width / 2.0, y + height / 2.0))
}

/// `(x, y, width, height)` of the box around a quad; `None` when it is not eight numbers or has no
/// area.
pub fn bounds(quad: &[f64]) -> Option<(f64, f64, f64, f64)> {
    if quad.len() != 8 {
        return None;
    }
    let xs = [quad[0], quad[2], quad[4], quad[6]];
    let ys = [quad[1], quad[3], quad[5], quad[7]];
    let (left, right) = (
        xs.iter().copied().fold(f64::MAX, f64::min),
        xs.iter().copied().fold(f64::MIN, f64::max),
    );
    let (top, bottom) = (
        ys.iter().copied().fold(f64::MAX, f64::min),
        ys.iter().copied().fold(f64::MIN, f64::max),
    );
    (right - left >= 1.0 && bottom - top >= 1.0).then_some((left, top, right - left, bottom - top))
}

/// The host of an `http` or `https` URL, lower-cased, without user, port or brackets.
pub fn host_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => host.split(':').next()?,
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    fn node(id: &str, parent: Option<&str>, role: &str, name: &str, children: &[&str], dom: i64) -> Value {
        let mut node = json!({
            "nodeId": id,
            "ignored": false,
            "role": { "type": "role", "value": role },
            "name": { "type": "computedString", "value": name },
            "childIds": children,
            "backendDOMNodeId": dom,
        });
        if let Some(parent) = parent {
            node["parentId"] = json!(parent);
        }
        node
    }

    #[test]
    fn a_snapshot_lists_named_and_interactive_nodes_under_their_listed_ancestors() {
        let mut ignored = node("3", Some("1"), "generic", "", &["4", "5", "8"], 30);
        ignored["ignored"] = json!(true);
        let mut field = node("8", Some("3"), "textbox", "", &[], 80);
        field["value"] = json!({ "type": "string", "value": "typed  so\nfar" });
        let nodes = [
            node("1", None, "RootWebArea", "Shop", &["2", "3"], 10),
            node("2", Some("1"), "heading", "Our \"best\" offers", &["9"], 20),
            node("9", Some("2"), "StaticText", "Our \"best\" offers", &[], 90),
            ignored,
            node("4", Some("3"), "button", "Buy", &["6"], 40),
            node("6", Some("4"), "StaticText", "Buy", &["7"], 60),
            node("7", Some("6"), "InlineTextBox", "Buy", &[], 70),
            node("5", Some("3"), "paragraph", "", &["10"], 50),
            node("10", Some("5"), "StaticText", "Free delivery", &[], 100),
            field,
        ];
        let shot = snapshot(&nodes);
        assert_eq!(
            shot.text,
            "- RootWebArea \"Shop\" [ref=e1]\n\
             \x20 - heading \"Our 'best' offers\" [ref=e2]\n\
             \x20 - button \"Buy\" [ref=e3]\n\
             \x20 - StaticText \"Free delivery\" [ref=e4]\n\
             \x20 - textbox \"\" value=\"typed so far\" [ref=e5]\n"
        );
        assert_eq!(shot.refs[&3], 40);
        assert_eq!(shot.refs[&5], 80);
        assert_eq!(shot.refs.len(), 5);
    }

    #[test]
    fn a_long_page_is_cut_at_the_line_limit_and_says_how_much_is_missing() {
        let ids: Vec<String> = (1..=MAX_SNAPSHOT_LINES + 25).map(|i| format!("b{i}")).collect();
        let children: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut nodes = vec![node("root", None, "RootWebArea", "Long", &children, 1)];
        for (index, id) in ids.iter().enumerate() {
            nodes.push(node(
                id,
                Some("root"),
                "button",
                &format!("b{index}"),
                &[],
                index as i64 + 2,
            ));
        }
        let shot = snapshot(&nodes);
        assert_eq!(shot.text.lines().count(), MAX_SNAPSHOT_LINES + 1);
        assert_eq!(shot.text.lines().last(), Some("… 26 more nodes not shown"));
        // Only listed nodes get a ref.
        assert_eq!(shot.refs.len(), MAX_SNAPSHOT_LINES);
        assert_eq!(snapshot(&[]), Snapshot::default());
    }

    #[test]
    fn wait_conditions_are_read_from_their_prefix_and_a_selector_is_the_rest() {
        let script = |wanted: &str| match condition(wanted).unwrap() {
            Condition::Script(script) => script,
            other => panic!("{wanted}: {other:?}"),
        };
        assert_eq!(condition("idle"), Ok(Condition::Idle));
        assert_eq!(script("load"), "document.readyState === 'complete'");
        assert_eq!(script("#go"), "document.querySelector(\"#go\") !== null");
        assert_eq!(
            script("gone: .spinner"),
            "document.querySelector(\".spinner\") === null"
        );
        // What the model writes is data, never code.
        assert_eq!(
            script("url:contains:/done?x=\"1\""),
            "location.href.includes(\"/done?x=\\\"1\\\"\")"
        );
        assert_eq!(
            script("title:contains:Saved"),
            "document.title.includes(\"Saved\")"
        );
        assert_eq!(
            script("text:it's \"here\""),
            "!!document.body && document.body.innerText.includes(\"it's \\\"here\\\"\")"
        );
        // A tag with a pseudo-class is still a selector.
        assert_eq!(
            script("title:hover"),
            "document.querySelector(\"title:hover\") !== null"
        );

        let role = |role: &str, name: Option<&str>| Condition::Role {
            role: role.into(),
            name: name.map(str::to_string),
        };
        assert_eq!(condition("role:button"), Ok(role("button", None)));
        assert_eq!(
            condition("role:button \"Save changes\""),
            Ok(role("button", Some("Save changes")))
        );
        assert_eq!(
            condition("role: link  Docs home "),
            Ok(role("link", Some("Docs home")))
        );
        assert!(condition("role:").unwrap_err().contains("names no role"));
    }

    #[test]
    fn a_role_is_found_by_the_name_the_snapshot_shows() {
        let mut hidden = node("4", Some("1"), "button", "Hidden", &[], 13);
        hidden["ignored"] = json!(true);
        let nodes = [
            node("1", None, "RootWebArea", "Page", &["2", "3", "4"], 10),
            node("2", Some("1"), "button", "Save  the \"draft\"", &[], 11),
            node("3", Some("1"), "link", "Docs", &[], 12),
            hidden,
        ];
        assert!(has_role(&nodes, "button", None));
        assert!(has_role(&nodes, "button", Some("Save the 'draft'")));
        assert!(has_role(&nodes, "link", Some("Docs")));
        assert!(!has_role(&nodes, "button", Some("Save")));
        assert!(!has_role(&nodes, "button", Some("Hidden")));
        assert!(!has_role(&nodes, "checkbox", None));
        // The names in a snapshot are the names a wait takes.
        let shown = snapshot(&nodes).text;
        assert!(shown.contains("- button \"Save the 'draft'\""), "{shown}");
    }

    #[test]
    fn refs_parse() {
        assert_eq!(ref_number("e12"), Some(12));
        assert_eq!(ref_number(" e3 "), Some(3));
        assert_eq!(ref_number("12"), None);
        assert_eq!(ref_number("ex"), None);
    }

    #[test]
    fn keys_parse_with_modifiers() {
        let enter = parse_keys("Enter").unwrap();
        assert_eq!(
            (enter.key.as_str(), enter.vk, enter.text.as_deref()),
            ("Enter", 13, Some("\r"))
        );

        let select_all = parse_keys("Control+A").unwrap();
        assert_eq!(select_all.modifiers, CONTROL);
        assert_eq!(
            (select_all.key.as_str(), select_all.code.as_str(), select_all.vk),
            ("A", "KeyA", 65)
        );
        // A command chord types nothing.
        assert_eq!(select_all.text, None);

        let lower = parse_keys("w").unwrap();
        assert_eq!(
            (lower.key.as_str(), lower.code.as_str(), lower.text.as_deref()),
            ("w", "KeyW", Some("w"))
        );
        let upper = parse_keys("Shift+w").unwrap();
        assert_eq!(
            (upper.modifiers, upper.key.as_str(), upper.text.as_deref()),
            (SHIFT, "W", Some("W"))
        );

        let chord = parse_keys("ctrl+shift+ArrowDown").unwrap();
        assert_eq!(
            (chord.modifiers, chord.key.as_str(), chord.vk),
            (CONTROL | SHIFT, "ArrowDown", 40)
        );
        assert_eq!(parse_keys("F5").unwrap().vk, 116);
        assert_eq!(parse_keys("7").unwrap().code, "Digit7");

        let plus = parse_keys("Control++").unwrap();
        assert_eq!((plus.modifiers, plus.key.as_str()), (CONTROL, "+"));
        assert_eq!(parse_keys("+").unwrap().text.as_deref(), Some("+"));

        assert!(
            parse_keys("Hyper+A")
                .unwrap_err()
                .contains("unknown modifier `Hyper`")
        );
        assert!(
            parse_keys("Control+Blorp")
                .unwrap_err()
                .contains("unknown key `Blorp`")
        );
        assert!(parse_keys("F13").is_err());
        assert!(parse_keys("").is_err());
    }

    #[test]
    fn geometry_of_quads() {
        let quad = [10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0];
        assert_eq!(bounds(&quad), Some((10.0, 20.0, 100.0, 40.0)));
        assert_eq!(centre(&quad), Some((60.0, 40.0)));
        // Nothing to click on.
        assert_eq!(centre(&[5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0]), None);
        assert_eq!(centre(&[1.0, 2.0]), None);
    }

    #[test]
    fn hosts_of_urls() {
        assert_eq!(
            host_of("https://Example.com/a?b#c").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("http://user:pw@localhost:8080/x").as_deref(),
            Some("localhost")
        );
        assert_eq!(host_of("http://[::1]:3000/").as_deref(), Some("::1"));
        assert_eq!(host_of("https://example.com").as_deref(), Some("example.com"));
        assert_eq!(host_of("file:///etc/passwd"), None);
        assert_eq!(host_of("example.com"), None);
        assert_eq!(host_of("https:///path"), None);
    }
}
