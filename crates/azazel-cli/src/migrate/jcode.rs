//! jcode: `~/.jcode` (`config.toml`, skills, `mcp.json`, a JSON snapshot per session with a
//! journal beside it, memory graphs) and the key files in its application config directory.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use azazel_ai::{AuthStyle, Block, ModelOverride, ProviderSettings, Usage, WireApi};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::convert::{
    Env, Providers, add_context_file, add_skills, env_lines, json_of, provider_id, provider_of_variable,
    read_json, read_text, read_toml, time_of, title_of, wire_api,
};
use super::model::{
    Chat, ChatRef, Found, InPlace, Key, Kind, Login, MemoryItem, Provider, Role, Scope, Secret, Source, Turn,
    TurnKind,
};
use super::pi;

/// Where jcode keeps its data: `JCODE_HOME`, else `~/.jcode`.
pub fn root(env: &Env) -> PathBuf {
    env.dir("JCODE_HOME", ".jcode")
}

/// Where jcode keeps the key files of its providers.
fn key_dir(env: &Env) -> PathBuf {
    match env.var("JCODE_HOME") {
        Some(home) => Path::new(home).join("config").join("jcode"),
        None => env.config.join("jcode"),
    }
}

/// A model as jcode names one (`provider:model`, or a bare model of the default provider) as a
/// selector of `provider/model`.
fn selector(model: &str, default_provider: Option<&str>, chatgpt: bool) -> Option<String> {
    let (provider, model) = match model.split_once(':') {
        // A colon inside a model id (`gpt-x:free`) is no provider prefix.
        Some((provider, model)) if !provider.contains(['/', '.']) && !model.is_empty() => (provider, model),
        _ => (default_provider?, model),
    };
    let provider = match provider {
        // jcode's `openai` is the ChatGPT sign-in when there is one, and the API otherwise.
        "openai" if chatgpt => "openai-codex",
        other => other,
    };
    Some(format!("{provider}/{model}"))
}

fn providers(found: &mut Found, config: &toml::Table, keys: &Path) -> Providers {
    let mut known = Providers::default();
    let tables = config.get("providers").and_then(toml::Value::as_table);
    for (name, entry) in tables.into_iter().flatten() {
        let Some(entry) = entry.as_table() else { continue };
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(toml::Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let what = format!("provider {name}");
        let api = text("type").or(text("provider_type")).and_then(wire_api);
        let (Some(api), Some(base_url)) = (api, text("base_url")) else {
            found.lossy(
                Kind::Models,
                format!("{what}: it needs a `type` azazel speaks and a `base_url`; it is left out"),
            );
            continue;
        };
        let mut settings = ProviderSettings {
            api: Some(api),
            base_url: Some(base_url.to_string()),
            api_key_env: text("api_key_env").map(str::to_string),
            ..ProviderSettings::default()
        };
        for (header, value) in entry
            .get("headers")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
        {
            if let Some(value) = value.as_str() {
                settings.headers.insert(header.clone(), value.to_string());
            }
        }
        if let Some(Value::Object(extra)) = entry.get("extra_body").map(json_of) {
            settings.extra_body = extra;
        }
        match (text("auth").map(str::to_ascii_lowercase).as_deref(), text("auth_header")) {
            (Some("header"), Some(header)) if header.eq_ignore_ascii_case("api-key") => {
                settings.auth = Some(AuthStyle::ApiKey);
            }
            (Some("header"), header) => found.lossy(
                Kind::Models,
                format!(
                    "{what}: its key goes in the header `{}`, which azazel cannot be told; set `headers` by hand",
                    header.unwrap_or("?")
                ),
            ),
            (Some("bearer"), _) if api != WireApi::OpenAiChat => settings.auth = Some(AuthStyle::Bearer),
            _ => {}
        }
        // The key itself: in the table, or in the provider's environment file.
        let from_file = text("env_file")
            .zip(text("api_key_env"))
            .and_then(|(file, variable)| {
                let lines = env_lines(&std::fs::read_to_string(keys.join(file)).ok()?);
                lines
                    .into_iter()
                    .find(|(name, _)| name == variable)
                    .map(|(_, value)| value)
            });
        if let Some(key) = text("api_key").map(str::to_string).or(from_file) {
            found.keys.push(Key {
                provider: name.clone(),
                key: Secret::new(key),
                origin: format!("config.toml ({what})"),
            });
        }
        let models = entry.get("models").and_then(toml::Value::as_array);
        let models = models
            .into_iter()
            .flatten()
            .filter_map(|model| {
                let model = model.as_table()?;
                let id = model.get("id")?.as_str()?.to_string();
                let window = ["context_window", "context_limit", "context_length"]
                    .iter()
                    .find_map(|key| model.get(*key).and_then(toml::Value::as_integer))
                    .and_then(|window| u64::try_from(window).ok());
                let inputs = model.get("input").and_then(toml::Value::as_array);
                Some((
                    id,
                    ModelOverride {
                        context_window: window,
                        reasoning: model.get("reasoning").and_then(toml::Value::as_bool),
                        vision: inputs
                            .map(|inputs| inputs.iter().any(|input| input.as_str() == Some("image"))),
                        ..ModelOverride::default()
                    },
                ))
            })
            .collect();
        known.add_custom(name);
        found.providers.push(Provider {
            id: name.clone(),
            settings,
            models,
        });
    }
    known
}

/// The project a memory file named after a hash belongs to: jcode names it with the hash of the
/// project's path, which only a path that is known from elsewhere can be matched against.
fn project_of_hash(hash: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|dir| {
            let mut as_path = std::collections::hash_map::DefaultHasher::new();
            dir.hash(&mut as_path);
            let mut as_text = std::collections::hash_map::DefaultHasher::new();
            dir.to_string_lossy().hash(&mut as_text);
            [as_path.finish(), as_text.finish()]
                .iter()
                .any(|sum| format!("{sum:016x}") == hash)
        })
        .cloned()
}

fn memories(found: &mut Found, file: &Path, scope: &Scope) {
    let Some(graph) = read_json(found, file) else {
        return;
    };
    let entries: Vec<&Value> = match (graph.get("memories"), graph.get("entries")) {
        (Some(Value::Object(by_id)), _) => by_id.values().collect(),
        (_, Some(Value::Array(entries))) => entries.iter().collect(),
        _ => Vec::new(),
    };
    for entry in entries {
        let inactive = entry.get("active").and_then(Value::as_bool) == Some(false)
            || entry.get("superseded_by").is_some_and(|by| !by.is_null());
        let Some(text) = entry.get("content").and_then(Value::as_str).filter(|_| !inactive) else {
            continue;
        };
        let category = entry
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        found.memories.push(MemoryItem {
            scope: scope.clone(),
            kind: match category.as_str() {
                "preference" => "preference",
                "correction" => "convention",
                _ => "fact",
            },
            text: text.to_string(),
        });
    }
}

/// Everything jcode holds for the user and for `project`.
pub fn scan(env: &Env, project: &Path) -> Option<Found> {
    let root = root(env);
    if !root.is_dir() {
        return None;
    }
    let mut found = Found::new(Source::Jcode, &root);
    let home = Scope::Home;
    let here = Scope::Project(project.to_path_buf());
    let keys = key_dir(env);

    // Sign-ins, which are named and never copied.
    let mut chatgpt = false;
    for (file, list, provider) in [
        ("auth.json", "anthropic_accounts", "anthropic"),
        ("openai-auth.json", "openai_accounts", "openai-codex"),
    ] {
        let Some(auth) = read_json(&mut found, &root.join(file)) else {
            continue;
        };
        let accounts = auth
            .get(list)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        chatgpt |= provider == "openai-codex" && !accounts.is_empty();
        for account in &accounts {
            found.logins.push(Login {
                provider: provider.to_string(),
                who: account.get("email").and_then(Value::as_str).map(str::to_string),
            });
        }
        if accounts.is_empty() && auth.get("anthropic").is_some_and(Value::is_object) {
            found.logins.push(Login {
                provider: provider.to_string(),
                who: None,
            });
        }
    }
    if root.join("gemini_oauth.json").is_file() {
        found.logins.push(Login {
            provider: "google-gemini-cli".to_string(),
            who: None,
        });
    }

    let config = read_toml(&mut found, &root.join("config.toml")).unwrap_or_default();
    let known = providers(&mut found, &config, &keys);
    let table = |name: &str| config.get(name).and_then(toml::Value::as_table);
    if let Some(provider) = table("provider") {
        let text = |key: &str| {
            provider
                .get(key)
                .and_then(toml::Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let default_provider = text("default_provider");
        if let Some(model) = text("default_model") {
            match selector(model, default_provider, chatgpt) {
                Some(selector) => {
                    let chain = known.chain(&mut found, "the default model", &[selector]);
                    if !chain.is_empty() {
                        found.roles.push(Role {
                            name: "default".to_string(),
                            chain,
                        });
                    }
                }
                None => found.lossy(
                    Kind::Models,
                    format!("the default model `{model}` names no provider, and no default provider is set"),
                ),
            }
        }
    }
    if let Some(theme) = table("display")
        .and_then(|display| display.get("theme"))
        .and_then(toml::Value::as_str)
        && theme != "auto"
    {
        pi::theme_setting(&mut found, "display.theme", theme);
    }
    if let Some(memory) = table("features")
        .and_then(|features| features.get("memory"))
        .and_then(toml::Value::as_bool)
    {
        pi::setting(
            &mut found,
            "features.memory",
            &Value::Bool(memory),
            &["memory", "enabled"],
            Some(toml::Value::Boolean(memory)),
        );
    }
    let unmapped: Vec<&str> = [
        "hooks",
        "keybindings",
        "notifications",
        "agents",
        "safety",
        "compaction",
    ]
    .into_iter()
    .filter(|name| table(name).is_some_and(|table| !table.is_empty()))
    .collect();
    if !unmapped.is_empty() {
        found.lossy(
            Kind::Settings,
            format!("without an equivalent in azazel: {}", unmapped.join(", ")),
        );
    }

    // API keys: one environment file per provider.
    let mut files: Vec<PathBuf> = std::fs::read_dir(&keys)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "env"))
        .collect();
    files.sort();
    for file in files {
        let Some(text) = read_text(&mut found, &file) else {
            continue;
        };
        for (variable, value) in env_lines(&text) {
            if let Some(provider) = provider_of_variable(&variable)
                && !found.keys.iter().any(|key| key.key.expose() == value)
            {
                found.keys.push(Key {
                    provider,
                    key: Secret::new(value),
                    origin: format!("{} ({variable})", super::convert::file_name(&file)),
                });
            }
        }
    }

    add_skills(&mut found, &root.join("skills"), &home);
    add_skills(&mut found, &project.join(".jcode").join("skills"), &here);
    // jcode reads an AGENTS.md in the home directory itself.
    add_context_file(
        &mut found,
        &env.home.join("AGENTS.md"),
        &home,
        "jcode-agents.md",
        env,
    );
    add_context_file(
        &mut found,
        &root.join("prompt-overlay.md"),
        &home,
        "jcode-prompt-overlay.md",
        env,
    );
    add_context_file(
        &mut found,
        &project.join(".jcode").join("prompt-overlay.md"),
        &here,
        "jcode-prompt-overlay.md",
        env,
    );
    let agents = project.join("AGENTS.md");
    if agents.is_file() {
        found.in_place.push(InPlace {
            kind: Kind::Rules,
            path: agents,
        });
    }
    pi::mcp_servers(&mut found, &root.join("mcp.json"), &home, false);
    pi::mcp_servers(&mut found, &project.join(".jcode").join("mcp.json"), &here, false);

    found.chats = session_files(&root.join("sessions"));

    let memory = root.join("memory");
    memories(&mut found, &memory.join("global.json"), &home);
    let mut dirs: Vec<PathBuf> = found.chats.iter().filter_map(|chat| chat.cwd.clone()).collect();
    dirs.push(project.to_path_buf());
    let mut graphs: Vec<PathBuf> = std::fs::read_dir(memory.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    graphs.sort();
    for graph in graphs {
        let hash = graph.file_stem().map(|stem| stem.to_string_lossy().into_owned());
        match hash.as_deref().and_then(|hash| project_of_hash(hash, &dirs)) {
            Some(dir) => memories(&mut found, &graph, &Scope::Project(dir)),
            None => {
                let before = found.memories.len();
                memories(&mut found, &graph, &home);
                if found.memories.len() > before {
                    found.lossy(
                        Kind::Memory,
                        format!(
                            "{} memories of a project jcode names only by a hash ({}) become memories of every \
                             project",
                            found.memories.len() - before,
                            hash.unwrap_or_default()
                        ),
                    );
                }
            }
        }
    }
    Some(found)
}

// ---------------------------------------------------------------- sessions

/// What of a snapshot says which conversation it is.
#[derive(Deserialize)]
struct Head {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
}

fn session_files(dir: &Path) -> Vec<ChatRef> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            let reader = std::io::BufReader::new(std::fs::File::open(&file).ok()?);
            let head: Head = serde_json::from_reader(reader).ok()?;
            Some(ChatRef {
                id: head.id,
                cwd: head.working_dir.filter(|dir| !dir.is_empty()).map(PathBuf::from),
                title: head.custom_title.or(head.title).as_deref().and_then(title_of),
                file,
            })
        })
        .collect()
}

fn usage_of(usage: Option<&Value>) -> Usage {
    let count = |key: &str| {
        usage
            .and_then(|usage| usage.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input: count("input_tokens"),
        output: count("output_tokens"),
        cache_read: count("cache_read_input_tokens"),
        cache_write: count("cache_creation_input_tokens"),
        ..Usage::default()
    }
}

/// The turns of one stored message. A user message carries the results of the calls before it
/// as well as what the user wrote.
fn message_turns(message: &Value, model: &Option<(String, String)>, turns: &mut Vec<Turn>) {
    let at = time_of(message.get("timestamp"));
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let field = |block: &Value, key: &str| block.get(key).and_then(Value::as_str).map(str::to_string);
    let mut push = |kind: TurnKind| turns.push(Turn { at, kind });
    if message.get("role").and_then(Value::as_str) == Some("assistant") {
        let signed = model
            .as_ref()
            .is_some_and(|(provider, _)| provider == "anthropic");
        let blocks = blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str)? {
                "text" => field(block, "text").map(Block::text),
                "reasoning" | "reasoning_trace" => Some(Block::Thinking {
                    text: field(block, "text")?,
                    signature: None,
                    redacted: false,
                }),
                "anthropic_thinking" => Some(Block::Thinking {
                    text: field(block, "thinking").unwrap_or_default(),
                    signature: field(block, "signature").filter(|signature| signed && !signature.is_empty()),
                    redacted: false,
                }),
                "open_ai_reasoning" => {
                    let summary: Vec<&str> = block
                        .get("summary")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    Some(Block::Thinking {
                        text: summary.join("\n\n"),
                        signature: None,
                        redacted: false,
                    })
                }
                "tool_use" => Some(Block::ToolCall {
                    id: field(block, "id")?,
                    name: field(block, "name")?,
                    input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    signature: None,
                }),
                _ => None,
            })
            .collect();
        return push(TurnKind::Assistant {
            blocks,
            model: model.clone(),
            api: None,
            usage: usage_of(message.get("token_usage")),
            failed: false,
        });
    }
    let mut said = Vec::new();
    for block in &blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => said.extend(
                field(block, "text")
                    .filter(|text| !text.is_empty())
                    .map(Block::text),
            ),
            Some("image") => {
                if let (Some(mime), Some(data)) = (field(block, "media_type"), field(block, "data")) {
                    said.push(Block::Image { mime, data });
                }
            }
            Some("tool_result") => {
                if let Some(call_id) = field(block, "tool_use_id") {
                    push(TurnKind::ToolResult {
                        call_id,
                        content: vec![Block::text(field(block, "content").unwrap_or_default())],
                        is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
                    });
                }
            }
            _ => {}
        }
    }
    if said.is_empty() {
        return;
    }
    // What jcode itself put into the conversation is kept, but is no prompt of the user's.
    match message.get("display_role").and_then(Value::as_str) {
        Some(role) => push(TurnKind::Note {
            name: role.to_string(),
            data: json!({ "content": message.get("content") }),
        }),
        None => push(TurnKind::User(said)),
    }
}

/// Reads a session: the snapshot, then what its journal added since the snapshot was written.
pub fn read_chat(reference: &ChatRef) -> Result<Chat, String> {
    let text = std::fs::read_to_string(&reference.file).map_err(|e| e.to_string())?;
    let mut session: Map<String, Value> =
        serde_json::from_str(&text).map_err(|e| format!("not a session: {e}"))?;
    let mut messages = match session.remove("messages") {
        Some(Value::Array(messages)) => messages,
        _ => Vec::new(),
    };
    let journal = reference.file.with_extension("journal.jsonl");
    for line in std::fs::read_to_string(journal).unwrap_or_default().lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // The journal's newest word on the session replaces the snapshot's.
        if let Some(meta) = entry.get("meta").and_then(Value::as_object) {
            for (key, value) in meta {
                if !value.is_null() {
                    session.insert(key.clone(), value.clone());
                }
            }
        }
        for message in entry
            .get("append_messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = message.get("id");
            if id.is_none() || !messages.iter().any(|known| known.get("id") == id) {
                messages.push(message.clone());
            }
        }
    }

    let text = |key: &str| session.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let model = text("model").map(|model| {
        let provider = match text("provider_key") {
            Some("openai") if text("route_api_method").is_some_and(|route| !route.contains("api")) => {
                "openai-codex".to_string()
            }
            Some(provider) => provider_id(provider),
            None => Source::Jcode.name().to_string(),
        };
        // The model may carry its provider as a prefix.
        let model = model.split_once(':').map_or(model, |(_, model)| model);
        (provider, model.to_string())
    });
    let mut chat = Chat {
        id: text("id").unwrap_or(&reference.id).to_string(),
        title: text("custom_title").or(text("title")).and_then(title_of),
        cwd: text("working_dir")
            .map(PathBuf::from)
            .or_else(|| reference.cwd.clone()),
        created: time_of(session.get("created_at")),
        ..Chat::default()
    };
    // A summary of what was compacted away stands before the messages that are left.
    let summary = session
        .get("compaction")
        .and_then(|compaction| compaction.get("summary_text"))
        .and_then(Value::as_str)
        .filter(|summary| !summary.trim().is_empty());
    if let Some(summary) = summary {
        chat.turns.push(Turn {
            at: chat.created,
            kind: TurnKind::Compaction {
                summary: summary.to_string(),
                tokens_before: 0,
                kept: 0,
            },
        });
    }
    for message in &messages {
        message_turns(message, &model, &mut chat.turns);
    }
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
