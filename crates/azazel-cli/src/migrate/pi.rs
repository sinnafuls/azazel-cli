//! pi: `~/.pi/agent` (`models.json`, `auth.json`, `settings.json`, skills, prompt templates,
//! `mcp.json`, sessions). Its session files are also what omp, its fork, writes; the reader of
//! both is here.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use azazel_ai::{Block, Usage, WireApi};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::convert::{
    Env, Providers, add_commands, add_context_file, add_skills, pi_providers, provider_id, read_json,
    time_of, title_of, toml_of, wire_api,
};
use super::model::{
    Chat, ChatRef, Found, InPlace, Key, Kind, Login, Role, Scope, Secret, Server, Setting, Source, Turn,
    TurnKind,
};

/// Where pi keeps its data: `PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
pub fn root(env: &Env) -> PathBuf {
    match env.var("PI_CODING_AGENT_DIR").map(PathBuf::from) {
        // omp reads the same variable; a directory with its files is not pi's.
        Some(dir) if !looks_like_omp(&dir) => dir,
        _ => env.home.join(".pi").join("agent"),
    }
}

pub fn looks_like_omp(dir: &Path) -> bool {
    ["config.yml", "config.yaml", "agent.db"]
        .iter()
        .any(|name| dir.join(name).is_file())
}

/// One key of a settings file as a key of `config.toml`, when the value passes `check`.
pub fn setting(found: &mut Found, from: &str, value: &Value, key: &[&str], mapped: Option<toml::Value>) {
    let shown = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    match mapped {
        Some(mapped) => found.settings.push(Setting {
            key: key.iter().map(|part| part.to_string()).collect(),
            value: mapped,
            from: format!("{from}: {shown}"),
        }),
        None => found.lossy(
            Kind::Settings,
            format!("{from}: {shown} has no equivalent in azazel"),
        ),
    }
}

/// The settings pi and omp name alike.
pub fn shared_settings(found: &mut Found, settings: &Map<String, Value>) {
    if let Some(compaction) = settings.get("compaction").and_then(Value::as_object) {
        for (theirs, ours) in [
            ("enabled", "enabled"),
            ("reserveTokens", "reserve_tokens"),
            ("keepRecentTokens", "keep_recent_tokens"),
        ] {
            let usable = compaction
                .get(theirs)
                .filter(|value| value.is_boolean() || value.as_u64().is_some());
            if let Some(value) = usable {
                setting(
                    found,
                    &format!("compaction.{theirs}"),
                    value,
                    &["compaction", ours],
                    toml_of(value),
                );
            }
        }
    }
    if let Some(mode) = settings.get("steeringMode").and_then(Value::as_str) {
        let mapped = match mode {
            "one-at-a-time" => Some("one"),
            "all" => Some("all"),
            _ => None,
        };
        let value = Value::String(mode.to_string());
        setting(
            found,
            "steeringMode",
            &value,
            &["tui", "queue_mode"],
            mapped.map(|mode| toml::Value::String(mode.to_string())),
        );
    }
    // Azazel hides thinking until asked; only the other choice is a setting.
    if settings.get("hideThinkingBlock").and_then(Value::as_bool) == Some(false) {
        setting(
            found,
            "hideThinkingBlock",
            &Value::Bool(false),
            &["tui", "show_thinking"],
            Some(toml::Value::Boolean(true)),
        );
    }
    if let Some(shell) = settings
        .get("shellPath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let value = Value::String(shell.to_string());
        setting(found, "shellPath", &value, &["tools", "shell"], toml_of(&value));
    }
}

/// A theme of a source, when azazel has one of that name.
pub fn theme_setting(found: &mut Found, from: &str, name: &str) {
    let value = Value::String(name.to_string());
    let known = azazel_tui::Theme::is_builtin(name);
    setting(
        found,
        from,
        &value,
        &["tui", "theme"],
        known.then(|| toml::Value::String(name.to_string())),
    );
}

/// The servers of an `mcp.json` (`mcpServers`, or `servers` as jcode also writes it).
/// `millis` says the file counts its timeouts in milliseconds.
pub fn mcp_servers(found: &mut Found, file: &Path, scope: &Scope, millis: bool) {
    let Some(root) = read_json(found, file) else {
        return;
    };
    let servers = root
        .get("mcpServers")
        .or_else(|| root.get("servers"))
        .and_then(Value::as_object);
    let disabled: Vec<&str> = root
        .get("disabledServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for (name, entry) in servers.into_iter().flatten() {
        let mut entry = entry.clone();
        let timeout = ["timeout", "timeout_secs"]
            .iter()
            .find_map(|key| entry.get(*key).and_then(Value::as_u64))
            .filter(|timeout| *timeout > 0)
            .map(|timeout| match millis {
                true => timeout.div_ceil(1000),
                false => timeout,
            });
        if disabled.contains(&name.as_str())
            && let Some(fields) = entry.as_object_mut()
        {
            fields.insert("enabled".to_string(), Value::Bool(false));
        }
        for dropped in ["auth", "oauth", "exposure", "toolExposure"] {
            if entry.get(dropped).is_some() {
                found.lossy(
                    Kind::Mcp,
                    format!("server {name}: `{dropped}` is not carried over; sign in with `azazel mcp login {name}`"),
                );
            }
        }
        found.servers.push(Server {
            scope: scope.clone(),
            name: name.clone(),
            entry,
            timeout,
        });
    }
}

/// Everything pi holds for the user and for `project`.
pub fn scan(env: &Env, project: &Path) -> Option<Found> {
    let root = root(env);
    if !root.is_dir() {
        return None;
    }
    let mut found = Found::new(Source::Pi, &root);
    let home = Scope::Home;
    let here = Scope::Project(project.to_path_buf());

    let providers = match read_json(&mut found, &root.join("models.json")) {
        Some(models) => pi_providers(&mut found, &models, &root.join("models.json"), false),
        None => Providers::default(),
    };

    if let Some(Value::Object(settings)) = read_json(&mut found, &root.join("settings.json")) {
        let text = |key: &str| {
            settings
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        if let (Some(provider), Some(model)) = (text("defaultProvider"), text("defaultModel")) {
            let selector = match text("defaultThinkingLevel") {
                Some(level) => format!("{provider}/{model}:{level}"),
                None => format!("{provider}/{model}"),
            };
            let chain = providers.chain(&mut found, "the default model", &[selector]);
            if !chain.is_empty() {
                found.roles.push(Role {
                    name: "default".to_string(),
                    chain,
                });
            }
        }
        // The models Ctrl+P steps through.
        let cycle: Vec<String> = settings
            .get("enabledModels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|selector| providers.spec(selector).ok())
            .collect();
        if !cycle.is_empty() {
            let value = Value::Array(cycle.iter().cloned().map(Value::String).collect());
            setting(
                &mut found,
                "enabledModels",
                &json!(format!("{} models", cycle.len())),
                &["tui", "favourites"],
                toml_of(&value),
            );
        }
        if let Some(theme) = text("theme") {
            theme_setting(&mut found, "theme", theme);
        }
        if let Some(retries) = settings
            .get("retry")
            .and_then(|retry| retry.get("maxRetries"))
            .and_then(Value::as_u64)
        {
            let value = Value::from(retries.max(1));
            setting(
                &mut found,
                "retry.maxRetries",
                &value,
                &["retry", "max_attempts"],
                toml_of(&value),
            );
        }
        shared_settings(&mut found, &settings);
        let unmapped: Vec<&str> = [
            "extensions",
            "skills",
            "prompts",
            "themes",
            "packages",
            "thinkingBudgets",
        ]
        .into_iter()
        .filter(|key| settings.contains_key(*key))
        .collect();
        if !unmapped.is_empty() {
            found.lossy(
                Kind::Settings,
                format!("{}: azazel has no equivalent", unmapped.join(", ")),
            );
        }
    }

    if let Some(Value::Object(auth)) = read_json(&mut found, &root.join("auth.json")) {
        for (provider, credential) in &auth {
            let provider = provider_id(provider);
            match credential.get("type").and_then(Value::as_str) {
                Some("oauth") => found.logins.push(Login {
                    provider,
                    who: credential
                        .get("email")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }),
                _ => {
                    if let Some(key) = credential
                        .get("key")
                        .and_then(Value::as_str)
                        .filter(|k| !k.is_empty())
                    {
                        found.keys.push(Key {
                            provider,
                            key: Secret::new(key),
                            origin: "auth.json".to_string(),
                        });
                    }
                }
            }
        }
    }

    add_skills(&mut found, &root.join("skills"), &home);
    add_skills(&mut found, &project.join(".pi").join("skills"), &here);
    add_commands(&mut found, &root.join("prompts"), &home, false, &providers);
    add_commands(
        &mut found,
        &project.join(".pi").join("prompts"),
        &here,
        false,
        &providers,
    );
    add_context_file(&mut found, &root.join("AGENTS.md"), &home, "pi-agents.md", env);
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = project.join(name);
        if path.is_file() {
            found.in_place.push(InPlace {
                kind: Kind::Rules,
                path,
            });
        }
    }
    // The skills pi shares with other agents are ones azazel reads too.
    for shared in [
        env.home.join(".agents").join("skills"),
        project.join(".agents").join("skills"),
    ] {
        if shared.is_dir() {
            found.in_place.push(InPlace {
                kind: Kind::Skills,
                path: shared,
            });
        }
    }
    mcp_servers(&mut found, &root.join("mcp.json"), &home, false);
    mcp_servers(&mut found, &project.join(".pi").join("mcp.json"), &here, false);

    let sessions = env
        .var("PI_CODING_AGENT_SESSION_DIR")
        .map_or_else(|| root.join("sessions"), PathBuf::from);
    session_files(&mut found, &sessions);
    Some(found)
}

// ---------------------------------------------------------------- sessions

/// The head of a session file: the title omp keeps on its first line, and the header.
fn head_of(file: &Path) -> Option<(Option<String>, Value)> {
    let mut head = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(256 * 1024)
        .read_to_end(&mut head)
        .ok()?;
    let head = String::from_utf8_lossy(&head);
    let mut title = None;
    for line in head.lines().take(2) {
        let entry: Value = serde_json::from_str(line).ok()?;
        match entry.get("type").and_then(Value::as_str) {
            Some("title") => {
                title = entry.get("title").and_then(Value::as_str).and_then(title_of);
            }
            Some("session") => return Some((title, entry)),
            _ => return None,
        }
    }
    None
}

/// The sessions below `dir`: one directory per project, the session files in it.
pub fn session_files(found: &mut Found, dir: &Path) {
    let mut out = Vec::new();
    let mut headless = 0usize;
    let Ok(projects) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for project in projects.flatten().map(|entry| entry.path()) {
        if project.is_dir() {
            let sessions = std::fs::read_dir(&project).into_iter().flatten().flatten();
            files.extend(sessions.map(|entry| entry.path()));
        } else {
            files.push(project);
        }
    }
    files.retain(|file| file.is_file() && file.extension().is_some_and(|ext| ext == "jsonl"));
    files.sort();
    for file in files {
        let Some((slot_title, header)) = head_of(&file) else {
            headless += 1;
            continue;
        };
        let text = |key: &str| header.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
        let Some(id) = text("id") else {
            headless += 1;
            continue;
        };
        out.push(ChatRef {
            id: id.to_string(),
            cwd: text("cwd").map(PathBuf::from),
            title: slot_title.or_else(|| text("title").and_then(title_of)),
            file,
        });
    }
    if headless > 0 {
        found.lossy(
            Kind::Chats,
            format!("{headless} session files do not start with a session header and are left out"),
        );
    }
    found.chats = out;
}

/// What of an entry says where it hangs in the tree; the rest of the line is not read.
#[derive(Deserialize)]
struct Light {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "parentId", default)]
    parent: Option<String>,
    /// omp: the leaf the session was on when the entry was written. Absent, or there and null.
    #[serde(rename = "activeLeafId", default, deserialize_with = "present")]
    leaf: Option<Option<String>>,
}

fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

/// The text and images of a message's content: a string, or blocks. An image omp moved out of
/// the session (`blob:sha256:<hash>`) is read back from `blobs`.
fn content_blocks(content: Option<&Value>, blobs: Option<&Path>, lost: &mut usize) -> Vec<Block> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => vec![Block::text(text.clone())],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str)? {
                "text" => block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(Block::text),
                "image" => {
                    let data = block.get("data").and_then(Value::as_str)?;
                    let mime = block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png")
                        .to_string();
                    let data = match data.strip_prefix("blob:sha256:") {
                        Some(hash) => match blobs.and_then(|dir| std::fs::read(dir.join(hash)).ok()) {
                            Some(bytes) => base64::engine::general_purpose::STANDARD.encode(bytes),
                            None => {
                                *lost += 1;
                                return Some(Block::text("[image: not found among the source's files]"));
                            }
                        },
                        None => data.to_string(),
                    };
                    Some(Block::Image { mime, data })
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn usage_of(usage: Option<&Value>) -> Usage {
    let count = |key: &str| {
        usage
            .and_then(|usage| usage.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input: count("input"),
        output: count("output"),
        cache_read: count("cacheRead"),
        cache_write: count("cacheWrite"),
        reasoning: count("reasoning"),
        cost_usd: usage
            .and_then(|usage| usage.get("cost"))
            .and_then(|cost| cost.get("total"))
            .and_then(Value::as_f64),
    }
}

/// One message of a pi session as a turn; `None` for what is no part of the conversation.
fn message_turn(message: &Value, blobs: Option<&Path>, lost: &mut usize) -> Option<TurnKind> {
    let text = |key: &str| message.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    Some(match text("role")? {
        "user" => TurnKind::User(content_blocks(message.get("content"), blobs, lost)),
        "assistant" => {
            let api = text("api").and_then(wire_api);
            let signed = api == Some(WireApi::AnthropicMessages);
            let blocks = message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|block| {
                    let field = |key: &str| block.get(key).and_then(Value::as_str);
                    match field("type")? {
                        "text" => field("text").map(Block::text),
                        "thinking" => Some(Block::Thinking {
                            text: field("thinking").unwrap_or_default().to_string(),
                            // Only Anthropic's signature is one azazel can send back.
                            signature: field("thinkingSignature")
                                .filter(|signature| signed && !signature.is_empty())
                                .map(str::to_string),
                            redacted: block.get("redacted").and_then(Value::as_bool) == Some(true),
                        }),
                        "toolCall" => Some(Block::ToolCall {
                            id: field("id")?.to_string(),
                            name: field("name")?.to_string(),
                            input: block.get("arguments").cloned().unwrap_or_else(|| json!({})),
                            signature: None,
                        }),
                        _ => None,
                    }
                })
                .collect();
            TurnKind::Assistant {
                blocks,
                model: text("provider")
                    .zip(text("model"))
                    .map(|(provider, model)| (provider_id(provider), model.to_string())),
                api,
                usage: usage_of(message.get("usage")),
                failed: matches!(text("stopReason"), Some("error" | "aborted")),
            }
        }
        "toolResult" => TurnKind::ToolResult {
            call_id: text("toolCallId")?.to_string(),
            content: content_blocks(message.get("content"), blobs, lost),
            is_error: message.get("isError").and_then(Value::as_bool) == Some(true),
        },
        // A command the user ran with `!`: the agent was told, as azazel tells its own.
        "bashExecution" => {
            if message.get("excludeFromContext").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let status = match message.get("exitCode").and_then(Value::as_i64) {
                Some(code) => format!("exit code {code}"),
                None => "it did not finish".to_string(),
            };
            TurnKind::User(vec![Block::text(format!(
                "I ran `{}` myself:\n{}\n{status}",
                text("command").unwrap_or_default(),
                text("output").unwrap_or_default(),
            ))])
        }
        role => TurnKind::Note {
            name: role.to_string(),
            data: json!({ "content": message.get("content"), "summary": message.get("summary") }),
        },
    })
}

/// Reads a session of pi or omp: the branch it was left on, turn by turn. `blobs` is where omp
/// keeps the images it moved out of its session files.
pub fn read_session(file: &Path, blobs: Option<&Path>) -> Result<Chat, String> {
    let open = || {
        std::fs::File::open(file)
            .map(BufReader::new)
            .map_err(|e| e.to_string())
    };

    // First the shape of the tree, without keeping what the entries hold.
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    let mut last: Option<Light> = None;
    let mut messages = 0usize;
    for line in open()?.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let Ok(light) = serde_json::from_str::<Light>(&line) else {
            continue;
        };
        let Some(id) = light.id.clone() else { continue };
        if matches!(light.kind.as_str(), "session" | "title") {
            continue;
        }
        messages += usize::from(light.kind == "message");
        parents.insert(id, light.parent.clone());
        last = Some(light);
    }
    let leaf = last.and_then(|last| match last.leaf {
        Some(Some(leaf)) if parents.contains_key(&leaf) => Some(leaf),
        _ => last.id,
    });
    let mut branch: HashSet<String> = HashSet::new();
    let mut cursor = leaf;
    while let Some(id) = cursor {
        cursor = parents.get(&id).cloned().flatten();
        if !branch.insert(id) {
            break; // a parent cycle in a damaged file
        }
    }

    // Then the entries on that branch, in the order they were written.
    let mut chat = Chat::default();
    let mut renamed: Option<String> = None;
    let mut lost = 0usize;
    let mut kept_messages = 0usize;
    // The turn each entry of the branch starts at, for compactions that name the first kept.
    let mut turn_of: HashMap<String, usize> = HashMap::new();
    for line in open()?.lines() {
        let line = line.map_err(|e| e.to_string())?;
        // The header and the title slot have no place in the tree; every other line is read
        // in full only when it is on the branch.
        let Ok(light) = serde_json::from_str::<Light>(&line) else {
            continue;
        };
        let on_branch = light.id.as_ref().is_some_and(|id| branch.contains(id));
        if !on_branch && !matches!(light.kind.as_str(), "session" | "title") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let text = |key: &str| entry.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
        let at = time_of(entry.get("timestamp"));
        if let Some(id) = &light.id {
            turn_of.insert(id.clone(), chat.turns.len());
        }
        let kind = match light.kind.as_str() {
            "title" => {
                chat.title = text("title").and_then(title_of);
                continue;
            }
            "session" => {
                chat.id = text("id").unwrap_or_default().to_string();
                chat.cwd = text("cwd").map(PathBuf::from);
                chat.created = at;
                if chat.title.is_none() {
                    chat.title = text("title").and_then(title_of);
                }
                continue;
            }
            "message" => {
                kept_messages += 1;
                let Some(message) = entry.get("message") else {
                    continue;
                };
                match message_turn(message, blobs, &mut lost) {
                    Some(kind) => kind,
                    None => continue,
                }
            }
            "compaction" => {
                let first = text("firstKeptEntryId").and_then(|id| turn_of.get(id)).copied();
                TurnKind::Compaction {
                    summary: text("summary").unwrap_or_default().to_string(),
                    tokens_before: entry.get("tokensBefore").and_then(Value::as_u64).unwrap_or(0),
                    kept: first.map_or(0, |first| chat.turns.len() - first),
                }
            }
            "branch_summary" => TurnKind::Note {
                name: "branch_summary".to_string(),
                data: json!({ "summary": text("summary") }),
            },
            "custom_message" => TurnKind::Note {
                name: text("customType").unwrap_or("custom_message").to_string(),
                data: json!({ "content": entry.get("content") }),
            },
            // pi names a session with an entry; omp keeps a trail of its titles.
            "session_info" => {
                renamed = text("name").and_then(title_of).or(renamed);
                continue;
            }
            "title_change" => {
                renamed = text("title").and_then(title_of).or(renamed);
                continue;
            }
            _ => continue,
        };
        chat.turns.push(Turn { at, kind });
    }
    if chat.id.is_empty() {
        return Err("not a session file: it has no header".to_string());
    }
    chat.title = chat.title.or(renamed).or_else(|| {
        chat.turns.iter().find_map(|turn| match &turn.kind {
            TurnKind::User(blocks) => blocks.iter().find_map(|block| match block {
                Block::Text { text } => title_of(text),
                _ => None,
            }),
            _ => None,
        })
    });
    if messages > kept_messages {
        chat.dropped.push(format!(
            "{} messages on branches that were left are not imported",
            messages - kept_messages
        ));
    }
    if lost > 0 {
        chat.dropped
            .push(format!("{lost} images were no longer among the source's files"));
    }
    Ok(chat)
}

/// A session with the conversations of the sub-agents it started, which omp keeps in a folder
/// named like the session file.
pub fn read_chat(reference: &ChatRef, blobs: Option<&Path>) -> Result<Chat, String> {
    let mut chat = read_session(&reference.file, blobs)?;
    let folder = reference.file.with_extension("");
    // A sub-agent's own sub-agents are in a folder named like its file: `Scout/Helper.jsonl`
    // is the agent `Scout.Helper`.
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut pending = vec![(String::new(), folder, 0usize)];
    while let Some((prefix, dir, depth)) = pending.pop() {
        for path in std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
        {
            let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()) else {
                continue;
            };
            let name = match prefix.is_empty() {
                true => stem,
                false => format!("{prefix}.{stem}"),
            };
            if path.is_dir() && depth < 4 {
                pending.push((name, path, depth + 1));
            } else if path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push((name, path));
            }
        }
    }
    files.sort();
    for (name, file) in files {
        match read_session(&file, blobs) {
            Ok(agent) if !agent.turns.is_empty() => chat.agents.push((name, agent)),
            Ok(_) => {}
            Err(why) => chat.dropped.push(format!(
                "a sub-agent's transcript ({}) could not be read: {why}",
                file.display()
            )),
        }
    }
    Ok(chat)
}
