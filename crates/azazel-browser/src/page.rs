//! What the browser tool works out by itself: the accessibility snapshot, key names, element
//! geometry and the host of a URL.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

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
    snapshot_with(nodes, None, 0)
}

/// [`snapshot`], where an element that `previous` (the refs of the last snapshot of the page)
/// already numbered keeps its number and new ones are numbered after the highest ever given (at
/// least `floor`), so that what an earlier snapshot showed the model still names the same element.
pub fn snapshot_with(nodes: &[Value], previous: Option<&HashMap<u32, i64>>, floor: u32) -> Snapshot {
    let known: HashMap<i64, u32> = previous
        .map(|refs| refs.iter().map(|(number, dom)| (*dom, *number)).collect())
        .unwrap_or_default();
    // Numbers are never handed out twice in a tab, so a ref in an older snapshot is either the
    // same element or none.
    let mut next = floor.max(previous.and_then(|refs| refs.keys().max().copied()).unwrap_or(0));
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
                    let number = match known.get(&dom) {
                        Some(number) => *number,
                        None if previous.is_some() => {
                            next += 1;
                            next
                        }
                        None => out.refs.len() as u32 + 1,
                    };
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

/// How a snapshot differs from the one before it, for the model that still has that one.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// Nothing changed.
    Same,
    /// What changed, ready to show.
    Diff(String),
    /// Too much changed to be worth it: show the whole snapshot.
    Whole,
}

/// The ref a snapshot line carries, `e12` of `[ref=e12]`.
fn line_ref(line: &str) -> Option<&str> {
    let start = line.rfind("[ref=")? + "[ref=".len();
    let end = line[start..].find(']')?;
    Some(&line[start..start + end])
}

/// Compares the lines of two snapshots of one page (refs are stable between them, see
/// [`snapshot_with`]). `+` lines are new, `-` lines gone, `~` lines are the same element (same
/// ref) whose line differs now. [`Change::Whole`] when more than half the lines moved or the
/// difference would not be shorter than the snapshot.
pub fn diff(old: &str, new: &str) -> Change {
    if old == new {
        return Change::Same;
    }
    let (before, after): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let mut left: HashMap<&str, usize> = HashMap::new();
    for line in &before {
        *left.entry(line).or_default() += 1;
    }
    let mut added: Vec<&str> = Vec::new();
    let mut kept = 0usize;
    for line in &after {
        match left.get_mut(line) {
            Some(count) if *count > 0 => {
                *count -= 1;
                kept += 1;
            }
            _ => added.push(line),
        }
    }
    let mut gone: Vec<&str> = Vec::new();
    let mut remaining = left;
    for line in &before {
        if let Some(count) = remaining.get_mut(line)
            && *count > 0
        {
            *count -= 1;
            gone.push(line);
        }
    }
    let total = before.len().max(after.len());
    if (added.len() + gone.len()) * 2 > total {
        return Change::Whole;
    }
    // A removed line and an added one with the same ref are one element that changed.
    let mut changed: Vec<&str> = Vec::new();
    let mut plus: Vec<&str> = Vec::new();
    for line in added {
        match line_ref(line).filter(|r| gone.iter().any(|g| line_ref(g) == Some(*r))) {
            Some(reference) => {
                gone.retain(|g| line_ref(g) != Some(reference));
                changed.push(line);
            }
            None => plus.push(line),
        }
    }
    let mut text = format!(
        "(the page changed since your last snapshot of this tab: {kept} lines unchanged, refs keep their numbers; \
         `+` is new, `-` is gone, `~` changed)\n"
    );
    for (mark, lines) in [('-', &gone), ('~', &changed), ('+', &plus)] {
        for line in lines {
            text.push(mark);
            text.push(' ');
            text.push_str(line);
            text.push('\n');
        }
    }
    if text.len() >= new.len() {
        return Change::Whole;
    }
    Change::Diff(text)
}

/// The longest chain of changes sent one after another before the whole snapshot is sent again.
const MAX_CHAIN: usize = 8;

/// What one agent was last shown of one tab.
struct Shown {
    url: String,
    body: String,
    /// The calls whose results the model needs to have in its context to rebuild `body`: the
    /// full snapshot and the changes sent after it.
    needs: Vec<String>,
}

/// The snapshots agents were shown, to send only what changed the next time. Decided once, when a
/// result is produced: it needs the earlier results to be in the context still, and says so when
/// they are not by sending the whole snapshot.
#[derive(Default)]
pub struct Baselines(Mutex<HashMap<(String, String), Shown>>);

impl Baselines {
    /// The text to give `agent` for a snapshot of `tab` at `url`: `header` then `body`, or then
    /// only what changed since the snapshot the agent was shown last. `live` says whether the
    /// result of a call is still in the agent's context.
    pub fn revise(
        &self,
        (agent, tab, url): (&str, &str, &str),
        (header, body): (&str, &str),
        call_id: &str,
        live: &dyn Fn(&str) -> bool,
    ) -> String {
        let mut shown = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (agent.to_string(), tab.to_string());
        let change = shown
            .get(&key)
            .filter(|before| {
                before.url == url && before.needs.len() < MAX_CHAIN && before.needs.iter().all(|id| live(id))
            })
            .map(|before| diff(&before.body, body));
        let whole = |shown: &mut HashMap<(String, String), Shown>| {
            shown.insert(
                key.clone(),
                Shown {
                    url: url.to_string(),
                    body: body.to_string(),
                    needs: vec![call_id.to_string()],
                },
            );
            format!("{header}{body}")
        };
        match change {
            Some(Change::Same) => {
                format!(
                    "{header}(unchanged since your last snapshot of this tab; its refs are still valid)\n"
                )
            }
            Some(Change::Diff(changes)) => {
                if let Some(before) = shown.get_mut(&key) {
                    before.body = body.to_string();
                    before.needs.push(call_id.to_string());
                }
                format!("{header}{changes}")
            }
            Some(Change::Whole) | None => whole(&mut shown),
        }
    }
}

/// The shortest long side a scaled screenshot keeps: smaller than this and text on a page stops
/// being legible.
pub const MIN_SHOT_SIDE: u32 = 512;

/// How a screenshot given to the model relates to the page it shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShotFrame {
    /// Size of the area shown, in page (CSS) pixels: what `click` and `hover` coordinates are
    /// measured in, from the top left of the area.
    pub page: (f64, f64),
    /// Size of the image the model gets, in pixels.
    pub shown: (u32, u32),
}

impl ShotFrame {
    /// The frame of an `image`-sized screenshot of a `page`-sized area, shown at `scale` (from
    /// 0.1 to 1) but never with a long side below [`MIN_SHOT_SIDE`] nor above `max_side`, and
    /// never larger than the image was.
    pub fn new(page: (f64, f64), image: (u32, u32), scale: f64, max_side: u32) -> Self {
        let long = image.0.max(image.1);
        let scale = if scale.is_finite() {
            scale.clamp(0.1, 1.0)
        } else {
            1.0
        };
        let wanted = ((f64::from(long) * scale).round() as u32).max(MIN_SHOT_SIDE.min(long));
        let side = wanted.min(long).min(max_side).max(1);
        let shown = if long <= side {
            image
        } else {
            let k = f64::from(side) / f64::from(long);
            (
                ((f64::from(image.0) * k).round() as u32).max(1),
                ((f64::from(image.1) * k).round() as u32).max(1),
            )
        };
        ShotFrame { page, shown }
    }

    /// The long side the image is scaled to (what to pass to `image_block`).
    pub fn side(&self) -> u32 {
        self.shown.0.max(self.shown.1)
    }

    /// The point of the page area that the point (`x`, `y`) of the shown image is.
    pub fn to_page(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x * self.page.0 / f64::from(self.shown.0.max(1)),
            y * self.page.1 / f64::from(self.shown.1.max(1)),
        )
    }

    /// Whether image pixels are page pixels, so that nothing needs saying.
    pub fn is_identity(&self) -> bool {
        (f64::from(self.shown.0) - self.page.0).abs() < 0.5
            && (f64::from(self.shown.1) - self.page.1).abs() < 0.5
    }

    /// The line of the result that says which pixels `x` and `y` are measured in.
    pub fn note(&self) -> String {
        format!(
            "the image is {}x{}, the window {}x{} page pixels: until the next screenshot of this tab, give `x` \
             and `y` of click, hover and scroll in pixels of this image; they are scaled to the page",
            self.shown.0,
            self.shown.1,
            self.page.0.round(),
            self.page.1.round(),
        )
    }
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

    /// A list page of `items` rows, each a link and a button, the shape of a search result page.
    fn list_page(items: &[(i64, &str)]) -> Vec<Value> {
        let ids: Vec<String> = items.iter().map(|(dom, _)| format!("r{dom}")).collect();
        let children: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut nodes = vec![node("1", None, "RootWebArea", "Results", &children, 1)];
        for (dom, name) in items {
            let (row, link, button) = (format!("r{dom}"), format!("l{dom}"), format!("b{dom}"));
            nodes.push(node(&row, Some("1"), "listitem", "", &[&link, &button], dom * 10));
            nodes.push(node(&link, Some(&row), "link", name, &[], dom * 10 + 1));
            nodes.push(node(
                &button,
                Some(&row),
                "button",
                "Add to cart",
                &[],
                dom * 10 + 2,
            ));
        }
        nodes
    }

    fn rows(n: i64) -> Vec<(i64, String)> {
        (1..=n).map(|i| (i, format!("Product number {i}"))).collect()
    }

    fn page_of(rows: &[(i64, String)]) -> Vec<Value> {
        let items: Vec<(i64, &str)> = rows.iter().map(|(d, n)| (*d, n.as_str())).collect();
        list_page(&items)
    }

    #[test]
    fn refs_keep_their_numbers_and_new_elements_get_numbers_never_given_before() {
        let first = snapshot_with(&page_of(&rows(3)), None, 0);
        let button_2 = first
            .refs
            .iter()
            .find(|(_, dom)| **dom == 22)
            .map(|(n, _)| *n)
            .unwrap();
        // Row 1 goes, row 4 comes.
        let mut later = rows(4);
        later.remove(0);
        let second = snapshot_with(&page_of(&later), Some(&first.refs), 7);
        assert_eq!(second.refs[&button_2], 22, "the same element keeps its ref");
        let highest_before = *first.refs.keys().max().unwrap();
        for (number, dom) in &second.refs {
            if !first.refs.values().any(|d| d == dom) {
                assert!(*number > highest_before.max(7), "e{number} was given before");
            }
        }
        // Refs of the gone row are not handed to anything else.
        for (number, dom) in &first.refs {
            if let Some(now) = second.refs.get(number) {
                assert_eq!(now, dom);
            }
        }
    }

    #[test]
    fn a_diff_of_the_same_page_is_same() {
        let shot = snapshot(&page_of(&rows(10))).text;
        assert_eq!(diff(&shot, &shot), Change::Same);
    }

    #[test]
    fn a_diff_shows_an_added_node_with_its_ref() {
        let before = snapshot_with(&page_of(&rows(10)), None, 0);
        let after = snapshot_with(&page_of(&rows(11)), Some(&before.refs), 0);
        let Change::Diff(text) = diff(&before.text, &after.text) else {
            panic!("{:?}", diff(&before.text, &after.text));
        };
        assert!(
            text.contains("+ ") && text.contains("Product number 11"),
            "{text}"
        );
        assert!(!text.contains("Product number 3\""), "{text}");
        assert!(!text.contains("\n- "), "{text}");
        assert!(text.len() < after.text.len() / 3);
    }

    #[test]
    fn a_diff_shows_a_removed_node_and_a_changed_one() {
        let before = snapshot_with(&page_of(&rows(10)), None, 0);
        let mut later = rows(10);
        later.remove(4);
        later[0].1 = "Product number 1 (sold out)".into();
        let after = snapshot_with(&page_of(&later), Some(&before.refs), 0);
        let Change::Diff(text) = diff(&before.text, &after.text) else {
            panic!("no diff");
        };
        assert!(
            text.lines()
                .any(|l| l.starts_with("- ") && l.contains("Product number 5")),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|l| l.starts_with("~ ") && l.contains("Product number 1 (sold out)")),
            "{text}"
        );
    }

    #[test]
    fn a_page_that_changed_by_more_than_half_is_sent_whole() {
        let before = snapshot(&page_of(&rows(4))).text;
        let other: Vec<(i64, String)> = (50..58).map(|i| (i, format!("Other {i}"))).collect();
        let after = snapshot(&page_of(&other)).text;
        assert_eq!(diff(&before, &after), Change::Whole);
    }

    #[test]
    fn baselines_send_changes_only_while_the_last_snapshot_is_in_context() {
        let baselines = Baselines::default();
        let before = snapshot_with(&page_of(&rows(10)), None, 0);
        let after = snapshot_with(&page_of(&rows(11)), Some(&before.refs), 0);
        let page = ("lead", "T1", "http://shop/list");
        let all = |_: &str| true;
        let first = baselines.revise(page, ("tab-1 · x\n", &before.text), "c1", &all);
        assert_eq!(first, format!("tab-1 · x\n{}", before.text));
        let same = baselines.revise(page, ("", &before.text), "c2", &all);
        assert!(same.contains("unchanged"), "{same}");
        let changed = baselines.revise(page, ("", &after.text), "c3", &all);
        assert!(changed.starts_with("(the page changed") && changed.len() < after.text.len() / 3);

        // The full snapshot c1 was elided or compacted away: the whole snapshot again.
        let lost = |id: &str| id != "c1";
        let whole = baselines.revise(page, ("", &after.text), "c4", &lost);
        assert_eq!(whole, after.text);
        // From c4 on, changes are against c4.
        let only_c4 = |id: &str| id == "c4";
        assert!(
            baselines
                .revise(page, ("", &after.text), "c5", &only_c4)
                .contains("unchanged")
        );

        // Another URL, another agent and another tab start from a whole snapshot.
        assert_eq!(
            baselines.revise(("lead", "T1", "http://shop/other"), ("", &after.text), "c6", &all),
            after.text
        );
        assert_eq!(
            baselines.revise(
                ("scout", "T1", "http://shop/other"),
                ("", &after.text),
                "c7",
                &all
            ),
            after.text
        );
        assert_eq!(
            baselines.revise(("lead", "T2", "http://shop/other"), ("", &after.text), "c8", &all),
            after.text
        );
    }

    #[test]
    fn a_long_chain_of_changes_ends_in_a_whole_snapshot() {
        let baselines = Baselines::default();
        let page = ("lead", "T1", "u");
        let all = |_: &str| true;
        let mut previous = snapshot_with(&page_of(&rows(20)), None, 0);
        baselines.revise(page, ("", &previous.text), "c0", &all);
        let mut wholes = 0;
        for n in 21..=(21 + MAX_CHAIN as i64) {
            let next = snapshot_with(&page_of(&rows(n)), Some(&previous.refs), 0);
            let sent = baselines.revise(page, ("", &next.text), &format!("c{n}"), &all);
            if sent == next.text {
                wholes += 1;
            }
            previous = next;
        }
        assert_eq!(wholes, 1);
    }

    #[test]
    fn a_scaled_screenshot_maps_its_positions_back_to_page_pixels() {
        // A 1280x800 window on a screen with two device pixels to a page pixel.
        let frame = ShotFrame::new((1280.0, 800.0), (2560, 1600), 0.5, 1568);
        assert_eq!(frame.shown, (1280, 800));
        assert!(frame.is_identity());
        let frame = ShotFrame::new((1280.0, 800.0), (1280, 800), 0.5, 1568);
        assert_eq!(frame.shown, (640, 400));
        assert_eq!(frame.to_page(320.0, 200.0), (640.0, 400.0));
        assert_eq!(frame.to_page(0.0, 0.0), (0.0, 0.0));
        assert_eq!(frame.to_page(640.0, 400.0), (1280.0, 800.0));
        assert!(frame.note().contains("640x400"));
        // Never below the minimum long side, nor above what the image was.
        let small = ShotFrame::new((800.0, 600.0), (800, 600), 0.25, 1568);
        assert_eq!(small.side(), MIN_SHOT_SIDE);
        assert_eq!(small.shown, (512, 384));
        let (x, y) = small.to_page(256.0, 192.0);
        assert!((x - 400.0).abs() < 1e-9 && (y - 300.0).abs() < 1e-9);
        assert_eq!(
            ShotFrame::new((300.0, 200.0), (300, 200), 0.1, 1568).shown,
            (300, 200)
        );
        // Scale 1 keeps the image as it was, up to the model's largest side.
        assert_eq!(
            ShotFrame::new((1280.0, 800.0), (1280, 800), 1.0, 1568).shown,
            (1280, 800)
        );
        assert_eq!(
            ShotFrame::new((4000.0, 1000.0), (4000, 1000), 1.0, 1568).shown,
            (1568, 392)
        );
    }

    /// Estimated tokens: characters / 4 for text, ⌈w/28⌉×⌈h/28⌉ for an image.
    fn tokens_text(text: &str) -> usize {
        text.len().div_ceil(4)
    }

    fn tokens_image((w, h): (u32, u32)) -> u32 {
        w.div_ceil(28) * h.div_ceil(28)
    }

    /// Prints what the two browser settings save on a fixture page (`--nocapture` shows it).
    #[test]
    fn measure_what_scaling_and_diffs_save() {
        let full = ShotFrame::new((1280.0, 800.0), (1280, 800), 1.0, 1568);
        let half = ShotFrame::new((1280.0, 800.0), (1280, 800), 0.5, 1568);
        let (big, small) = (tokens_image(full.shown), tokens_image(half.shown));
        println!(
            "screenshot 1280x800: scale 1.0 {:?} ~{big} tokens; scale 0.5 {:?} ~{small} tokens",
            full.shown, half.shown
        );
        assert!(small * 3 < big);

        let baselines = Baselines::default();
        let all = |_: &str| true;
        let first = snapshot_with(&page_of(&rows(40)), None, 0);
        baselines.revise(("lead", "T", "u"), ("", &first.text), "c1", &all);
        // After a click: one product sold out, one more loaded.
        let mut later = rows(41);
        later[2].1 = "Product number 3 (in cart)".into();
        let second = snapshot_with(&page_of(&later), Some(&first.refs), 0);
        let sent = baselines.revise(("lead", "T", "u"), ("", &second.text), "c2", &all);
        println!(
            "second snapshot of a 40-row page: full {} bytes ~{} tokens; diff {} bytes ~{} tokens",
            second.text.len(),
            tokens_text(&second.text),
            sent.len(),
            tokens_text(&sent)
        );
        assert!(sent.len() * 5 < second.text.len());
    }
}
