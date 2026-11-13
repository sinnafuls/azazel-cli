//! Codex CLI: `~/.codex` (`config.toml`, `auth.json`, prompts, skills, `AGENTS.md`, the
//! rollout files of its sessions).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use azazel_ai::{Block, ProviderSettings, ThinkingLevel, Usage, WireApi};
use serde_json::{Map, Value, json};

use super::convert::{
    Env, Providers, add_commands, add_context_file, add_skills, json_of, read_json, read_toml, time_of,
    title_of, wire_api,
};
use super::model::{
    Chat, ChatRef, Found, InPlace, Key, Kind, Login, Provider, Role, Scope, Secret, Server, Source, Turn,
    TurnKind,
};
use super::pi;

/// Where Codex keeps its data: `CODEX_HOME`, else `~/.codex`.
pub fn root(env: &Env) -> PathBuf {
    env.dir("CODEX_HOME", ".codex")
}

fn providers(found: &mut Found, config: &toml::Table) -> Providers {
    let mut known = Providers::default();
    let tables = config.get("model_providers").and_then(toml::Value::as_table);
    for (id, entry) in tables.into_iter().flatten() {
        let Some(entry) = entry.as_table() else { continue };
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(toml::Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let what = format!("provider {id}");
        let api = wire_api(text("wire_api").unwrap_or("responses"));
        let (Some(api), Some(base_url)) = (api, text("base_url")) else {
            found.lossy(
                Kind::Models,
                format!("{what}: it needs a `base_url` and a `wire_api` azazel speaks; it is left out"),
            );
            continue;
        };
        let mut settings = ProviderSettings {
            name: text("name").map(str::to_string),
            api: Some(api),
            base_url: Some(base_url.to_string()),
            api_key_env: text("env_key").map(str::to_string),
            ..ProviderSettings::default()
        };
        let pairs = |key: &str| {
            entry
                .get(key)
                .and_then(toml::Value::as_table)
                .into_iter()
                .flatten()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
        };
        settings.headers.extend(pairs("http_headers"));
        // A header filled from a variable.
        settings
            .headers
            .extend(pairs("env_http_headers").map(|(header, variable)| (header, format!("${{{variable}}}"))));
        if let Some(token) = text("experimental_bearer_token") {
            found.keys.push(Key {
                provider: id.clone(),
                key: Secret::new(token),
                origin: format!("config.toml ({what})"),
            });
        }
        for dropped in ["query_params", "auth"] {
            if entry.contains_key(dropped) {
                found.lossy(Kind::Models, format!("{what}: `{dropped}` has no equivalent"));
            }
        }
        known.add_custom(id);
        found.providers.push(Provider {
            id: id.clone(),
            settings,
            models: Vec::new(),
        });
    }
    known
}

/// An `[mcp_servers.<name>]` table in the shape of an `mcpServers` entry.
fn server(found: &mut Found, name: &str, table: &toml::Table) -> Server {
    let mut entry = Map::new();
    for key in ["command", "args", "env", "cwd", "url", "enabled"] {
        if let Some(value) = table.get(key) {
            entry.insert(key.to_string(), json_of(value));
        }
    }
    let mut headers: Map<String, Value> = match table.get("http_headers").map(json_of) {
        Some(Value::Object(headers)) => headers,
        _ => Map::new(),
    };
    for (header, variable) in table
        .get("env_http_headers")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
    {
        if let Some(variable) = variable.as_str() {
            headers.insert(header.clone(), json!(format!("${{{variable}}}")));
        }
    }
    if let Some(variable) = table.get("bearer_token_env_var").and_then(toml::Value::as_str) {
        headers.insert(
            "Authorization".to_string(),
            json!(format!("Bearer ${{{variable}}}")),
        );
    }
    if !headers.is_empty() {
        entry.insert("headers".to_string(), Value::Object(headers));
    }
    // Variables of the user's environment the server is to see.
    let forwarded = table.get("env_vars").and_then(toml::Value::as_array);
    for variable in forwarded.into_iter().flatten().filter_map(toml::Value::as_str) {
        let env = entry.entry("env").or_insert_with(|| json!({}));
        if let Some(env) = env.as_object_mut() {
            env.entry(variable.to_string())
                .or_insert_with(|| json!(format!("${{{variable}}}")));
        }
    }
    let names = |key: &str| -> Vec<String> {
        table
            .get(key)
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|name| name.as_str().map(str::to_string))
            .collect()
    };
    let (include, exclude) = (names("enabled_tools"), names("disabled_tools"));
    if !include.is_empty() {
        entry.insert("includeTools".to_string(), json!(include));
    }
    if !exclude.is_empty() {
        entry.insert("excludeTools".to_string(), json!(exclude));
    }
    if table.contains_key("oauth") || table.contains_key("auth") {
        found.lossy(
            Kind::Mcp,
            format!("server {name}: its sign-in is not carried over; sign in with `azazel mcp login {name}`"),
        );
    }
    let seconds = |key: &str| {
        let value = table.get(key)?;
        value
            .as_integer()
            .map(|n| n as f64)
            .or_else(|| value.as_float())
            .filter(|n| *n > 0.0)
            .map(|n| n.ceil() as u64)
    };
    Server {
        scope: Scope::Home,
        name: name.to_string(),
        entry: Value::Object(entry),
        timeout: seconds("tool_timeout_sec"),
    }
}

/// Everything Codex holds for the user and for `project`.
pub fn scan(env: &Env, project: &Path) -> Option<Found> {
    let root = root(env);
    if !root.is_dir() {
        return None;
    }
    let mut found = Found::new(Source::Codex, &root);
    let home = Scope::Home;
    let here = Scope::Project(project.to_path_buf());

    // The key and the sign-in.
    let mut chatgpt = false;
    if let Some(auth) = read_json(&mut found, &root.join("auth.json")) {
        let key = auth
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty());
        if let Some(key) = key {
            found.keys.push(Key {
                provider: "openai".to_string(),
                key: Secret::new(key),
                origin: "auth.json (OPENAI_API_KEY)".to_string(),
            });
        }
        let signed_in = auth.get("tokens").is_some_and(|tokens| {
            tokens
                .get("refresh_token")
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty())
        });
        if signed_in {
            found.logins.push(Login {
                provider: "openai-codex".to_string(),
                who: None,
            });
        }
        chatgpt =
            signed_in && (key.is_none() || auth.get("auth_mode").and_then(Value::as_str) == Some("chatgpt"));
    }

    let config = read_toml(&mut found, &root.join("config.toml")).unwrap_or_default();
    let known = providers(&mut found, &config);
    let text = |key: &str| {
        config
            .get(key)
            .and_then(toml::Value::as_str)
            .filter(|s| !s.is_empty())
    };
    if let Some(model) = text("model") {
        let provider = match text("model_provider").unwrap_or("openai") {
            // The same models, through the subscription or through the API.
            "openai" if chatgpt => "openai-codex",
            other => other,
        };
        let effort = text("model_reasoning_effort").filter(|effort| effort.parse::<ThinkingLevel>().is_ok());
        let selector = match effort {
            Some(effort) => format!("{provider}/{model}:{effort}"),
            None => format!("{provider}/{model}"),
        };
        let chain = known.chain(&mut found, "the default model", &[selector]);
        if !chain.is_empty() {
            found.roles.push(Role {
                name: "default".to_string(),
                chain,
            });
        }
    }
    // How much Codex asks, from its approval policy and its sandbox.
    let approval = config.get("approval_policy");
    let sandbox = text("sandbox_mode");
    if approval.is_some() || sandbox.is_some() {
        let policy = approval.and_then(toml::Value::as_str);
        let mode = match (policy, sandbox) {
            (_, Some("read-only")) => Some("plan"),
            (Some("never"), _) => Some("auto"),
            (Some("on-request" | "untrusted" | "on-failure"), _) => Some("manual"),
            (None, Some(_)) if approval.is_none() => Some("manual"),
            _ => None,
        };
        let shown = format!(
            "{} / {}",
            policy.unwrap_or(if approval.is_some() { "granular" } else { "default" }),
            sandbox.unwrap_or("default")
        );
        pi::setting(
            &mut found,
            "approval_policy / sandbox_mode",
            &json!(shown),
            &["permissions", "mode"],
            mode.map(|mode| toml::Value::String(mode.to_string())),
        );
        if policy == Some("never") && sandbox == Some("danger-full-access") {
            found.lossy(
                Kind::Settings,
                "approval_policy never with sandbox_mode danger-full-access becomes `auto`, which still asks before \
                 a destructive command; `permissions.mode = \"bypass\"` lifts that too",
            );
        }
    }
    let unmapped: Vec<&str> = [
        "profiles",
        "sandbox_workspace_write",
        "notify",
        "features",
        "project_doc_fallback_filenames",
    ]
    .into_iter()
    .filter(|key| config.contains_key(*key))
    .collect();
    if !unmapped.is_empty() {
        found.lossy(
            Kind::Settings,
            format!("without an equivalent in azazel: {}", unmapped.join(", ")),
        );
    }

    let servers = config.get("mcp_servers").and_then(toml::Value::as_table);
    for (name, table) in servers.into_iter().flatten() {
        if let Some(table) = table.as_table() {
            let server = server(&mut found, name, table);
            found.servers.push(server);
        }
    }
    if project.join(".codex").join("config.toml").is_file() {
        found.lossy(
            Kind::Settings,
            ".codex/config.toml of this project is not converted: copy what you need into .azazel/config.toml",
        );
    }

    add_commands(&mut found, &root.join("prompts"), &home, false, &known);
    add_skills(&mut found, &root.join("skills"), &home);
    add_skills(&mut found, &project.join(".codex").join("skills"), &here);
    add_context_file(&mut found, &root.join("AGENTS.md"), &home, "codex-agents.md", env);
    let agents = project.join("AGENTS.md");
    if agents.is_file() {
        found.in_place.push(InPlace {
            kind: Kind::Rules,
            path: agents,
        });
    }
    for memories in ["memories", "memories_v2"] {
        let kept = std::fs::read_dir(root.join(memories)).is_ok_and(|mut entries| entries.next().is_some());
        if kept {
            found.lossy(
                Kind::Memory,
                format!(
                    "{memories}: Codex does not document the format of its memory files; they are not read"
                ),
            );
        }
    }

    found.chats = rollouts(&root);
    Some(found)
}

// ---------------------------------------------------------------- rollouts

/// The names Codex gave its threads, by session id.
fn thread_names(root: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(text) = std::fs::read_to_string(root.join("session_index.jsonl")) else {
        return names;
    };
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let (Some(id), Some(name)) = (
            entry["id"].as_str(),
            entry["thread_name"].as_str().and_then(title_of),
        ) {
            names.insert(id.to_string(), name);
        }
    }
    names
}

fn rollouts(root: &Path) -> Vec<ChatRef> {
    let names = thread_names(root);
    let mut files = Vec::new();
    let mut pending = vec![(root.join("sessions"), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        for path in std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
        {
            if path.is_dir() && depth < 4 {
                pending.push((path, depth + 1));
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            let mut first = String::new();
            BufReader::new(std::fs::File::open(&file).ok()?)
                .read_line(&mut first)
                .ok()?;
            let line: Value = serde_json::from_str(&first).ok()?;
            let meta = (line["type"] == "session_meta").then(|| &line["payload"])?;
            let id = meta["id"].as_str()?.to_string();
            Some(ChatRef {
                cwd: meta["cwd"].as_str().map(PathBuf::from),
                title: names.get(&id).cloned(),
                id,
                file,
            })
        })
        .collect()
}

/// Whether a user message is context Codex put into the conversation itself: the environment,
/// the instruction files.
fn injected(text: &str) -> bool {
    let text = text.trim();
    if text.starts_with("# AGENTS.md") {
        return true;
    }
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let tag: String = rest
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || matches!(c, '_' | '-'))
        .collect();
    !tag.is_empty() && rest[tag.len()..].starts_with('>') && text.ends_with(&format!("</{tag}>"))
}

/// What a tool returned: text, a `{content|output, success}` object, or content items.
fn output_of(output: &Value) -> (Vec<Block>, bool) {
    let text_blocks = |text: &str| vec![Block::text(text.to_string())];
    match output {
        Value::String(text) => {
            // Older rollouts keep the object as a JSON string.
            match serde_json::from_str::<Value>(text) {
                Ok(Value::Object(inner)) if inner.contains_key("output") => {
                    let failed = inner
                        .get("metadata")
                        .and_then(|metadata| metadata.get("exit_code"))
                        .and_then(Value::as_i64)
                        .is_some_and(|code| code != 0);
                    (text_blocks(inner["output"].as_str().unwrap_or_default()), failed)
                }
                _ => (text_blocks(text), false),
            }
        }
        Value::Object(object) => {
            let failed = object.get("success").and_then(Value::as_bool) == Some(false);
            let body = object.get("content").or_else(|| object.get("output"));
            let (blocks, _) = body.map(output_of).unwrap_or_default();
            (blocks, failed)
        }
        Value::Array(items) => (items.iter().filter_map(content_item).collect(), false),
        _ => (Vec::new(), false),
    }
}

/// One content item of a message: text, or an image given as a `data:` address.
fn content_item(item: &Value) -> Option<Block> {
    match item["type"].as_str()? {
        "input_text" | "output_text" | "text" => item["text"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(Block::text),
        "input_image" => {
            let url = item["image_url"].as_str()?;
            match url
                .strip_prefix("data:")
                .and_then(|rest| rest.split_once(";base64,"))
            {
                Some((mime, data)) => Some(Block::Image {
                    mime: mime.to_string(),
                    data: data.to_string(),
                }),
                None => Some(Block::text(format!("[image: {url}]"))),
            }
        }
        _ => None,
    }
}

fn usage_of(usage: &Value) -> Usage {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let cached = count("cached_input_tokens");
    Usage {
        input: count("input_tokens").saturating_sub(cached),
        output: count("output_tokens"),
        cache_read: cached,
        reasoning: count("reasoning_output_tokens"),
        ..Usage::default()
    }
}

/// Reads a rollout: its response items are the conversation; the events beside them repeat it.
pub fn read_chat(reference: &ChatRef) -> Result<Chat, String> {
    let reader = std::fs::File::open(&reference.file)
        .map(BufReader::new)
        .map_err(|e| e.to_string())?;
    let mut chat = Chat {
        id: reference.id.clone(),
        cwd: reference.cwd.clone(),
        title: reference.title.clone(),
        ..Chat::default()
    };
    let mut provider = "openai".to_string();
    let mut model: Option<String> = None;
    // The blocks of the reply being read: its reasoning, its text and its calls are items of
    // their own, and what follows them (a result, a prompt) ends it.
    let mut reply: Vec<Block> = Vec::new();
    let mut reply_at = None;
    let mut reply_usage = Usage::default();

    fn close(
        chat: &mut Chat,
        reply: &mut Vec<Block>,
        at: &mut Option<jiff::Timestamp>,
        usage: &mut Usage,
        model: &Option<String>,
        provider: &str,
    ) {
        if reply.is_empty() {
            return;
        }
        chat.turns.push(Turn {
            at: at.take(),
            kind: TurnKind::Assistant {
                blocks: std::mem::take(reply),
                model: model.clone().map(|model| (provider.to_string(), model)),
                api: Some(WireApi::OpenAiResponses),
                usage: std::mem::take(usage),
                failed: false,
            },
        });
    }

    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let at = time_of(entry.get("timestamp"));
        let payload = &entry["payload"];
        match entry["type"].as_str() {
            Some("session_meta") => {
                chat.created = time_of(payload.get("timestamp")).or(at);
                if let Some(cwd) = payload["cwd"].as_str() {
                    chat.cwd = Some(PathBuf::from(cwd));
                }
                if let Some(named) = payload["model_provider"].as_str().filter(|name| !name.is_empty()) {
                    provider = named.to_string();
                }
            }
            Some("turn_context") => {
                // A new model: the reply before was another's.
                let named = payload["model"].as_str().map(str::to_string);
                if named.is_some() && named != model {
                    close(
                        &mut chat,
                        &mut reply,
                        &mut reply_at,
                        &mut reply_usage,
                        &model,
                        &provider,
                    );
                    model = named;
                }
            }
            Some("compacted") => {
                close(
                    &mut chat,
                    &mut reply,
                    &mut reply_at,
                    &mut reply_usage,
                    &model,
                    &provider,
                );
                chat.turns.push(Turn {
                    at,
                    kind: TurnKind::Compaction {
                        summary: payload["message"].as_str().unwrap_or_default().to_string(),
                        tokens_before: 0,
                        kept: 0,
                    },
                });
            }
            Some("event_msg") => match payload["type"].as_str() {
                Some("thread_name_updated") => {
                    chat.title = payload["thread_name"].as_str().and_then(title_of).or(chat.title);
                }
                Some("token_count") => {
                    let last = &payload["info"]["last_token_usage"];
                    if last.is_object() {
                        let usage = usage_of(last);
                        match chat.turns.last_mut() {
                            _ if !reply.is_empty() => reply_usage = usage,
                            Some(Turn {
                                kind: TurnKind::Assistant { usage: counted, .. },
                                ..
                            }) => *counted = usage,
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            Some("response_item") => {
                let field = |key: &str| payload[key].as_str().map(str::to_string);
                let mut say = |block: Block| {
                    reply_at = reply_at.or(at);
                    reply.push(block);
                };
                match payload["type"].as_str() {
                    Some("message") => {
                        let blocks: Vec<Block> = payload["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(content_item)
                            .collect();
                        match payload["role"].as_str() {
                            Some("assistant") => blocks.into_iter().for_each(&mut say),
                            Some("user") => {
                                let own: Vec<Block> = blocks
                                    .into_iter()
                                    .filter(|block| !matches!(block, Block::Text { text } if injected(text)))
                                    .collect();
                                if !own.is_empty() {
                                    close(
                                        &mut chat,
                                        &mut reply,
                                        &mut reply_at,
                                        &mut reply_usage,
                                        &model,
                                        &provider,
                                    );
                                    chat.turns.push(Turn {
                                        at,
                                        kind: TurnKind::User(own),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("reasoning") => {
                        let parts = |key: &str| -> Vec<String> {
                            payload[key]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|part| part["text"].as_str().map(str::to_string))
                                .collect()
                        };
                        let mut text = parts("summary");
                        text.extend(parts("content"));
                        if !text.is_empty() {
                            say(Block::Thinking {
                                text: text.join("\n\n"),
                                signature: None,
                                redacted: false,
                            });
                        }
                    }
                    Some("function_call") => {
                        if let (Some(id), Some(name)) = (field("call_id"), field("name")) {
                            let arguments = payload["arguments"].as_str().unwrap_or("{}");
                            let input = serde_json::from_str::<Value>(arguments)
                                .ok()
                                .filter(Value::is_object)
                                .unwrap_or_else(|| json!({ "arguments": arguments }));
                            say(Block::ToolCall {
                                id,
                                name,
                                input,
                                signature: None,
                            });
                        }
                    }
                    Some("custom_tool_call") => {
                        if let (Some(id), Some(name)) = (field("call_id"), field("name")) {
                            say(Block::ToolCall {
                                id,
                                name,
                                input: json!({ "input": payload["input"] }),
                                signature: None,
                            });
                        }
                    }
                    Some("local_shell_call") => {
                        if let Some(id) = field("call_id") {
                            say(Block::ToolCall {
                                id,
                                name: "shell".to_string(),
                                input: payload["action"].clone(),
                                signature: None,
                            });
                        }
                    }
                    Some("function_call_output" | "custom_tool_call_output" | "local_shell_call_output") => {
                        if let Some(call_id) = field("call_id") {
                            close(
                                &mut chat,
                                &mut reply,
                                &mut reply_at,
                                &mut reply_usage,
                                &model,
                                &provider,
                            );
                            let (content, is_error) = output_of(&payload["output"]);
                            chat.turns.push(Turn {
                                at,
                                kind: TurnKind::ToolResult {
                                    call_id,
                                    content,
                                    is_error,
                                },
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    close(
        &mut chat,
        &mut reply,
        &mut reply_at,
        &mut reply_usage,
        &model,
        &provider,
    );
    if chat.title.is_none() {
        chat.title = chat.turns.iter().find_map(|turn| match &turn.kind {
            TurnKind::User(blocks) => blocks.iter().find_map(|block| match block {
                Block::Text { text } => title_of(text),
                _ => None,
            }),
            _ => None,
        });
    }
    Ok(chat)
}
