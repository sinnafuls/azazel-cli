//! Claude Code: `~/.claude` (`settings.json`, commands, agents, rules, `CLAUDE.md`, the
//! transcripts and memory files under `projects/`) and `~/.claude.json` (the API key, the
//! account, MCP servers).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use azazel_ai::{Block, Usage, WireApi};
use azazel_core::{Rule, frontmatter};
use serde_json::{Value, json};

use super::convert::{
    Env, Providers, add_agents, add_commands, add_context_file, add_rules, markdown_files, read_json,
    read_text, time_of, title_of, toml_of,
};
use super::model::{
    Chat, ChatRef, Found, InPlace, Key, Kind, Login, MemoryItem, Role, Scope, Secret, Server, Setting,
    Source, Turn, TurnKind,
};
use super::pi;

/// Where Claude Code keeps its data: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn root(env: &Env) -> PathBuf {
    env.dir("CLAUDE_CONFIG_DIR", ".claude")
}

/// The file with the machine's state: beside the home's `.claude`, or inside the directory
/// `CLAUDE_CONFIG_DIR` names.
fn state_file(env: &Env) -> PathBuf {
    match env.var("CLAUDE_CONFIG_DIR") {
        Some(dir) => Path::new(dir).join(".claude.json"),
        None => env.home.join(".claude.json"),
    }
}

/// The name Claude Code gives a project's directory under `projects/`.
fn sanitized(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// A permission rule of Claude Code as azazel's rules: `Bash(npm test:*)` is
/// `shell(npm test:*)`, and a rule for `Edit` covers `write` too, as it does there.
fn rules_of(rule: &str) -> Option<Vec<String>> {
    let rule = rule.trim();
    let (tool, pattern) = match rule.split_once('(') {
        Some((tool, rest)) => (tool.trim(), Some(rest.strip_suffix(')')?.trim())),
        None => (rule, None),
    };
    let path = |pattern: &str| {
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        match pattern.strip_prefix("//") {
            // `//x` is an absolute path there, `/x` one from the project root.
            Some(absolute) => format!("/{absolute}"),
            None => pattern.strip_prefix('/').unwrap_or(pattern).to_string(),
        }
    };
    let tools: &[&str] = match tool {
        "Bash" => &["shell"],
        "Read" => &["read"],
        "Edit" | "Write" | "MultiEdit" => &["edit", "write"],
        "Glob" => &["glob"],
        "Grep" => &["grep"],
        "WebFetch" => &["web_fetch"],
        "WebSearch" => &["web_search"],
        "Task" | "Agent" => &["task"],
        "TodoWrite" => &["todo"],
        _ if tool.starts_with("mcp__") => {
            // A server alone stands for every tool of it.
            let name = match tool.matches("__").count() {
                1 => format!("{tool}__*"),
                _ => tool.to_string(),
            };
            return name.parse::<Rule>().ok().map(|_| vec![name]);
        }
        _ => return None,
    };
    let rules: Vec<String> = tools
        .iter()
        .map(|name| match pattern {
            None => name.to_string(),
            Some(pattern) if matches!(tool, "Bash" | "WebFetch" | "WebSearch" | "Task" | "Agent") => {
                format!("{name}({pattern})")
            }
            Some(pattern) => format!("{name}({})", path(pattern)),
        })
        .collect();
    rules
        .iter()
        .all(|rule| rule.parse::<Rule>().is_ok())
        .then_some(rules)
}

fn settings(found: &mut Found, settings: &Value) {
    let text = |key: &str| {
        settings
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    if let Some(model) = text("model") {
        // `[1m]` and its like select a variant of the model, not another model.
        let id = model.split('[').next().unwrap_or(model);
        if id.starts_with("claude-") {
            found.roles.push(Role {
                name: "default".to_string(),
                chain: vec![format!("anthropic/{id}")],
            });
        } else {
            found.lossy(
                Kind::Models,
                format!("model: `{model}` is an alias of Claude Code's; choose the model with /model"),
            );
        }
    }
    if let Some(permissions) = settings.get("permissions") {
        for list in ["allow", "ask", "deny"] {
            let mut rules: Vec<String> = Vec::new();
            for rule in permissions
                .get(list)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(rule) = rule.as_str() else { continue };
                match rules_of(rule) {
                    Some(mapped) => {
                        for rule in mapped {
                            if !rules.contains(&rule) {
                                rules.push(rule);
                            }
                        }
                    }
                    None => found.lossy(
                        Kind::Settings,
                        format!("permissions.{list}: `{rule}` names a tool azazel does not have"),
                    ),
                }
            }
            if !rules.is_empty() {
                found.settings.push(Setting {
                    key: vec!["permissions".to_string(), list.to_string()],
                    value: toml::Value::Array(rules.iter().cloned().map(toml::Value::String).collect()),
                    from: format!("permissions.{list}: {} rules", rules.len()),
                });
            }
        }
        if let Some(mode) = permissions.get("defaultMode").and_then(Value::as_str) {
            let mapped = match mode {
                "default" => Some("manual"),
                "acceptEdits" => Some("accept-edits"),
                "plan" => Some("plan"),
                "auto" | "bypassPermissions" => Some("auto"),
                _ => None,
            };
            pi::setting(
                found,
                "permissions.defaultMode",
                &json!(mode),
                &["permissions", "mode"],
                mapped.map(|mode| toml::Value::String(mode.to_string())),
            );
            if mode == "bypassPermissions" {
                found.lossy(
                    Kind::Settings,
                    "permissions.defaultMode: bypassPermissions becomes `auto`, which still asks before a \
                     destructive command; `permissions.mode = \"bypass\"` lifts that too",
                );
            }
        }
        if let Some(dirs) = permissions
            .get("additionalDirectories")
            .filter(|dirs| dirs.is_array())
        {
            pi::setting(
                found,
                "permissions.additionalDirectories",
                dirs,
                &["permissions", "additional_dirs"],
                toml_of(dirs),
            );
        }
    }
    if let Some(days) = settings
        .get("cleanupPeriodDays")
        .filter(|days| days.as_u64().is_some())
    {
        pi::setting(
            found,
            "cleanupPeriodDays",
            days,
            &["session", "retention_days"],
            toml_of(days),
        );
    }
    if let Some(command) = settings
        .get("statusLine")
        .and_then(|line| line.get("command"))
        .and_then(Value::as_str)
    {
        let value = json!(command);
        pi::setting(
            found,
            "statusLine.command",
            &value,
            &["statusline", "command"],
            toml_of(&value),
        );
    }
    if let Some(helper) = text("apiKeyHelper") {
        let value = json!(helper);
        pi::setting(
            found,
            "apiKeyHelper",
            &value,
            &["providers", "anthropic", "api_key_command"],
            toml_of(&value),
        );
    }
    if settings
        .get("hooks")
        .is_some_and(|hooks| hooks.as_object().is_some_and(|hooks| !hooks.is_empty()))
    {
        found.lossy(
            Kind::Settings,
            "hooks: not converted; their commands read Claude Code's tool names and inputs, which differ in a \
             `[[hooks.<event>]]` table (docs/hooks.md)",
        );
    }
    let unmapped: Vec<&str> = [
        "env",
        "outputStyle",
        "sandbox",
        "enabledPlugins",
        "alwaysThinkingEnabled",
    ]
    .into_iter()
    .filter(|key| settings.get(*key).is_some())
    .collect();
    if !unmapped.is_empty() {
        found.lossy(
            Kind::Settings,
            format!("without an equivalent in azazel: {}", unmapped.join(", ")),
        );
    }
}

/// The project a directory under `projects/` belongs to: what its transcripts say, else the
/// project of the machine's state file whose name comes out the same.
fn project_of(dir: &Path, registered: &[String]) -> Option<PathBuf> {
    let from_transcripts = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .find_map(|file| head_of(&file).1);
    from_transcripts.or_else(|| {
        let name = dir.file_name()?.to_string_lossy().into_owned();
        registered
            .iter()
            .find(|path| sanitized(path) == name)
            .map(PathBuf::from)
    })
}

/// A memory file's kind in azazel's terms, from the `type` Claude Code gives it.
fn memory_kind(kind: Option<&str>) -> &'static str {
    match kind {
        Some("user") => "preference",
        Some("feedback") => "convention",
        _ => "fact",
    }
}

/// The memory files of one project: each topic file is a memory; a lone `MEMORY.md` is read
/// item by item.
fn memories(found: &mut Found, dir: &Path, scope: &Scope) {
    let files = markdown_files(dir, false);
    let topics: Vec<&PathBuf> = files
        .iter()
        .filter(|file| file.file_name().is_some_and(|name| name != "MEMORY.md"))
        .collect();
    for file in &topics {
        let Some(text) = read_text(found, file) else {
            continue;
        };
        let (meta, body) = match frontmatter::split(&text) {
            Some((yaml, body)) => (frontmatter::parse(yaml).unwrap_or_default(), body),
            None => (serde_json::Map::new(), text.as_str()),
        };
        if body.trim().is_empty() {
            continue;
        }
        found.memories.push(MemoryItem {
            scope: scope.clone(),
            kind: memory_kind(meta.get("type").and_then(Value::as_str)),
            text: body.trim().to_string(),
        });
    }
    if !topics.is_empty() {
        return;
    }
    let Some(index) = read_text(found, &dir.join("MEMORY.md")) else {
        return;
    };
    for line in index.lines() {
        let item = line
            .trim()
            .strip_prefix("- ")
            .or_else(|| line.trim().strip_prefix("* "));
        if let Some(item) = item.map(str::trim).filter(|item| !item.is_empty()) {
            found.memories.push(MemoryItem {
                scope: scope.clone(),
                kind: "fact",
                text: item.to_string(),
            });
        }
    }
}

/// Everything Claude Code holds for the user and for `project`.
pub fn scan(env: &Env, project: &Path) -> Option<Found> {
    let root = root(env);
    let state_file = state_file(env);
    if !root.is_dir() && !state_file.is_file() {
        return None;
    }
    let mut found = Found::new(Source::Claude, &root);
    let home = Scope::Home;
    let here = Scope::Project(project.to_path_buf());
    let providers = Providers::default();

    if let Some(user) = read_json(&mut found, &root.join("settings.json")) {
        settings(&mut found, &user);
    }
    for name in ["settings.json", "settings.local.json"] {
        if project.join(".claude").join(name).is_file() {
            found.lossy(
                Kind::Settings,
                format!(".claude/{name} of this project is not converted: copy what you need into .azazel/config.toml"),
            );
        }
    }

    let state = read_json(&mut found, &state_file).unwrap_or(Value::Null);
    if let Some(theme) = state.get("theme").and_then(Value::as_str) {
        pi::theme_setting(&mut found, "theme", theme);
    }
    if let Some(key) = state
        .get("primaryApiKey")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
    {
        found.keys.push(Key {
            provider: "anthropic".to_string(),
            key: Secret::new(key),
            origin: ".claude.json (primaryApiKey)".to_string(),
        });
    }
    let account = state
        .get("oauthAccount")
        .and_then(|account| account.get("emailAddress"))
        .and_then(Value::as_str);
    let subscribed = read_json(&mut found, &root.join(".credentials.json"))
        .is_some_and(|credentials| credentials.get("claudeAiOauth").is_some());
    // With an API key, the account is the Console's that made the key, not a subscription.
    if subscribed || (account.is_some() && found.keys.is_empty()) {
        found.logins.push(Login {
            provider: "anthropic".to_string(),
            who: account.map(str::to_string),
        });
    }

    // Servers of the user, and those kept for this project in the user's own file.
    let servers = |found: &mut Found, map: Option<&Value>, scope: &Scope| {
        for (name, entry) in map.and_then(Value::as_object).into_iter().flatten() {
            found.servers.push(Server {
                scope: scope.clone(),
                name: name.clone(),
                entry: entry.clone(),
                timeout: None,
            });
        }
    };
    servers(&mut found, state.get("mcpServers"), &home);
    let registered: Vec<String> = state
        .get("projects")
        .and_then(Value::as_object)
        .map(|projects| projects.keys().cloned().collect())
        .unwrap_or_default();
    let same = |written: &str| {
        let written = written.replace('\\', "/");
        let wanted = project.to_string_lossy().replace('\\', "/");
        let (written, wanted) = (written.trim_end_matches('/'), wanted.trim_end_matches('/'));
        written == wanted || (cfg!(windows) && written.eq_ignore_ascii_case(wanted))
    };
    for path in registered.iter().filter(|path| same(path)) {
        let map = state["projects"][path.as_str()].get("mcpServers").cloned();
        servers(&mut found, map.as_ref(), &here);
    }
    for (kind, path) in [
        (Kind::Mcp, project.join(".mcp.json")),
        (Kind::Skills, root.join("skills")),
        (Kind::Skills, project.join(".claude").join("skills")),
        (Kind::Commands, project.join(".claude").join("commands")),
        (Kind::Agents, project.join(".claude").join("agents")),
        (Kind::Rules, project.join(".claude").join("rules")),
    ] {
        if path.exists() {
            found.in_place.push(InPlace { kind, path });
        }
    }

    add_commands(&mut found, &root.join("commands"), &home, true, &providers);
    add_agents(&mut found, &root.join("agents"), &home, &providers);
    add_context_file(&mut found, &root.join("CLAUDE.md"), &home, "claude.md", env);
    add_rules(&mut found, &root.join("rules"), &home, "claude");
    // A project's CLAUDE.md is read where it is unless an AGENTS.md beside it takes its place.
    let ours = project.join("CLAUDE.md");
    if ours.is_file() {
        let shadowed = project.join("AGENTS.md").is_file();
        let only_points = std::fs::read_to_string(&ours).is_ok_and(|text| text.trim() == "@AGENTS.md");
        match shadowed && !only_points {
            true => add_context_file(&mut found, &ours, &here, "claude.md", env),
            false => found.in_place.push(InPlace {
                kind: Kind::Rules,
                path: ours,
            }),
        }
    }
    add_context_file(
        &mut found,
        &project.join(".claude").join("CLAUDE.md"),
        &here,
        "claude-project.md",
        env,
    );
    add_context_file(
        &mut found,
        &project.join("CLAUDE.local.md"),
        &here,
        "claude-local.md",
        env,
    );

    let projects: Vec<PathBuf> = std::fs::read_dir(root.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    for dir in &projects {
        let memory = dir.join("memory");
        if memory.is_dir() {
            let scope = match project_of(dir, &registered) {
                Some(cwd) => Scope::Project(cwd),
                None => {
                    found.lossy(
                        Kind::Memory,
                        format!(
                            "the memories under {} belong to a project that can no longer be told; they become \
                             memories of every project",
                            dir.display()
                        ),
                    );
                    Scope::Home
                }
            };
            memories(&mut found, &memory, &scope);
        }
    }
    found.chats = transcripts(&projects);
    Some(found)
}

// ---------------------------------------------------------------- transcripts

/// The title and working directory the first lines of a transcript give away.
fn head_of(file: &Path) -> (Option<String>, Option<PathBuf>) {
    let Ok(file) = std::fs::File::open(file) else {
        return (None, None);
    };
    let (mut title, mut cwd) = (None, None);
    for line in BufReader::new(file).lines().map_while(Result::ok).take(40) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let text = |key: &str| entry.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
        title = text("customTitle")
            .or(text("aiTitle"))
            .or(text("summary"))
            .and_then(title_of)
            .or(title);
        cwd = cwd.or_else(|| text("cwd").map(PathBuf::from));
        if cwd.is_some() && title.is_some() {
            break;
        }
    }
    (title, cwd)
}

fn transcripts(projects: &[PathBuf]) -> Vec<ChatRef> {
    let mut files: Vec<PathBuf> = projects
        .iter()
        .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            let id = file.file_stem()?.to_string_lossy().into_owned();
            let (title, cwd) = head_of(&file);
            Some(ChatRef { file, id, cwd, title })
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

/// The text and images of a tool result or of what a user sent.
fn plain_blocks(content: Option<&Value>) -> Vec<Block> {
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
                    let source = block.get("source")?;
                    Some(Block::Image {
                        mime: source.get("media_type")?.as_str()?.to_string(),
                        data: source.get("data")?.as_str()?.to_string(),
                    })
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a user message is something Claude Code wrote into the conversation itself: the
/// record of a slash command, or the output of a command run with `!`.
fn written_by_the_ui(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<command-name>",
        "<command-message>",
        "<local-command-",
        "<bash-input>",
        "<bash-stdout>",
    ]
    .iter()
    .any(|mark| text.starts_with(mark))
}

/// Reads one transcript file: the branch that was written to last, back to its beginning and
/// through the compactions on the way. `sidechain` reads the transcript of a sub-agent.
fn read_transcript(file: &Path, sidechain: bool) -> Result<Chat, String> {
    let reader = std::fs::File::open(file)
        .map(BufReader::new)
        .map_err(|e| e.to_string())?;
    let mut lines: Vec<Value> = Vec::new();
    let mut by_uuid: HashMap<String, usize> = HashMap::new();
    let mut parents: HashSet<String> = HashSet::new();
    let (mut custom_title, mut ai_title, mut summary) = (None, None, None);
    let mut chat = Chat::default();
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let text = |key: &str| entry.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
        match text("type") {
            Some("custom-title") => custom_title = text("customTitle").and_then(title_of).or(custom_title),
            Some("ai-title") => ai_title = text("aiTitle").and_then(title_of).or(ai_title),
            Some("summary") => summary = text("summary").and_then(title_of).or(summary),
            Some("user" | "assistant" | "system" | "attachment") => {
                if (entry.get("isSidechain").and_then(Value::as_bool) == Some(true)) != sidechain {
                    continue;
                }
                let Some(uuid) = text("uuid") else { continue };
                if chat.cwd.is_none() {
                    chat.cwd = text("cwd").map(PathBuf::from);
                }
                if chat.id.is_empty() {
                    chat.id = text("sessionId").unwrap_or_default().to_string();
                }
                for key in ["parentUuid", "logicalParentUuid"] {
                    if let Some(parent) = text(key) {
                        parents.insert(parent.to_string());
                    }
                }
                by_uuid.insert(uuid.to_string(), lines.len());
                lines.push(entry);
            }
            _ => {}
        }
    }
    let uuid_of = |entry: &Value| {
        entry
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let spoken = |entry: &Value| matches!(entry["type"].as_str(), Some("user" | "assistant"));

    // The leaf is the message nothing follows; among several, the one written last.
    let leaf = lines
        .iter()
        .enumerate()
        .filter(|(_, entry)| spoken(entry) && !parents.contains(&uuid_of(entry)))
        .max_by_key(|(at, entry)| (time_of(entry.get("timestamp")), *at))
        .map(|(at, _)| at);
    let mut chain: Vec<usize> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut cursor = leaf;
    while let Some(at) = cursor {
        if !seen.insert(at) {
            break;
        }
        chain.push(at);
        let entry = &lines[at];
        // A compaction starts a new root; what it summarised hangs from its logical parent.
        let parent = ["parentUuid", "logicalParentUuid"]
            .iter()
            .find_map(|key| entry.get(*key).and_then(Value::as_str));
        cursor = parent.and_then(|parent| by_uuid.get(parent)).copied();
    }
    chain.reverse();

    let total = lines.iter().filter(|entry| spoken(entry)).count();
    let mut kept = 0usize;
    // The API message the last reply turn was cut from: its blocks come one per line.
    let mut open: Option<String> = None;
    for at in chain {
        let entry = &lines[at];
        let when = time_of(entry.get("timestamp"));
        if chat.created.is_none() {
            chat.created = when;
        }
        if !spoken(entry) {
            continue;
        }
        kept += 1;
        let message = &entry["message"];
        let flag = |key: &str| entry.get(key).and_then(Value::as_bool) == Some(true);
        if entry["type"] == "assistant" {
            if flag("isApiErrorMessage") || message["model"] == "<synthetic>" {
                continue;
            }
            let blocks: Vec<Block> = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|block| {
                    let field = |key: &str| block.get(key).and_then(Value::as_str);
                    match field("type")? {
                        "text" => field("text").map(Block::text),
                        "thinking" => Some(Block::Thinking {
                            text: field("thinking").unwrap_or_default().to_string(),
                            signature: field("signature").filter(|s| !s.is_empty()).map(str::to_string),
                            redacted: false,
                        }),
                        "redacted_thinking" => Some(Block::Thinking {
                            text: String::new(),
                            signature: Some(field("data")?.to_string()),
                            redacted: true,
                        }),
                        "tool_use" => Some(Block::ToolCall {
                            id: field("id")?.to_string(),
                            name: field("name")?.to_string(),
                            input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                            signature: None,
                        }),
                        _ => None,
                    }
                })
                .collect();
            let id = message["id"].as_str().map(str::to_string);
            let usage = usage_of(message.get("usage"));
            // The same message id: more blocks of the reply before.
            if id.is_some()
                && id == open
                && let Some(Turn {
                    kind:
                        TurnKind::Assistant {
                            blocks: so_far,
                            usage: counted,
                            ..
                        },
                    ..
                }) = chat.turns.last_mut()
            {
                so_far.extend(blocks);
                *counted = usage;
                continue;
            }
            open = id;
            chat.turns.push(Turn {
                at: when,
                kind: TurnKind::Assistant {
                    blocks,
                    model: message["model"]
                        .as_str()
                        .map(|model| ("anthropic".to_string(), model.to_string())),
                    api: Some(WireApi::AnthropicMessages),
                    usage,
                    failed: false,
                },
            });
            continue;
        }
        if flag("isCompactSummary") {
            open = None;
            let text: String = plain_blocks(message.get("content"))
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            chat.turns.push(Turn {
                at: when,
                kind: TurnKind::Compaction {
                    summary: text,
                    tokens_before: 0,
                    kept: 0,
                },
            });
            continue;
        }
        if flag("isMeta") {
            continue;
        }
        // The results of the reply before come first: nothing was said before them.
        let mut results = Vec::new();
        for block in message["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_result"
                && let Some(call_id) = block["tool_use_id"].as_str()
            {
                results.push(TurnKind::ToolResult {
                    call_id: call_id.to_string(),
                    content: plain_blocks(block.get("content")),
                    is_error: block["is_error"].as_bool() == Some(true),
                });
            }
        }
        let said: Vec<Block> = plain_blocks(message.get("content"))
            .into_iter()
            .filter(|block| !matches!(block, Block::Text { text } if written_by_the_ui(text)))
            .collect();
        if !results.is_empty() || !said.is_empty() {
            open = None;
        }
        chat.turns
            .extend(results.into_iter().map(|kind| Turn { at: when, kind }));
        if !said.is_empty() {
            chat.turns.push(Turn {
                at: when,
                kind: TurnKind::User(said),
            });
        }
    }
    chat.title = custom_title.or(ai_title).or(summary).or_else(|| {
        chat.turns.iter().find_map(|turn| match &turn.kind {
            TurnKind::User(blocks) => blocks.iter().find_map(|block| match block {
                Block::Text { text } => title_of(text),
                _ => None,
            }),
            _ => None,
        })
    });
    if total > kept {
        chat.dropped.push(format!(
            "{} messages on branches that were left are not imported",
            total - kept
        ));
    }
    Ok(chat)
}

/// A transcript with the conversations of the sub-agents it started.
pub fn read_chat(reference: &ChatRef) -> Result<Chat, String> {
    let mut chat = read_transcript(&reference.file, false)?;
    if chat.id.is_empty() {
        chat.id = reference.id.clone();
    }
    chat.cwd = chat.cwd.or_else(|| reference.cwd.clone());
    let folder = reference.file.with_extension("").join("subagents");
    for file in subagent_files(&folder) {
        let name = file.file_stem().map(|stem| stem.to_string_lossy().into_owned());
        match (name, read_transcript(&file, true)) {
            (Some(name), Ok(agent)) if !agent.turns.is_empty() => chat.agents.push((name, agent)),
            (_, Ok(_)) => {}
            (_, Err(why)) => chat.dropped.push(format!(
                "a sub-agent's transcript ({}) could not be read: {why}",
                file.display()
            )),
        }
    }
    Ok(chat)
}

/// The transcripts below a `subagents` folder, one level of directories deep.
fn subagent_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
    {
        if path.is_dir() {
            out.extend(
                std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.path()),
            );
        } else {
            out.push(path);
        }
    }
    out.retain(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl"));
    out.sort();
    out
}
