//! omp (oh-my-pi): `~/.omp/agent` (`config.yml`, `models.yml`, the credentials in `agent.db`,
//! `.env`, skills, commands, agents, rules, `mcp.json`, sessions, the mnemopi memory banks).

use std::path::{Path, PathBuf};

use azazel_core::frontmatter;
use serde_json::{Map, Value, json};

use super::convert::{
    Env, Providers, add_agents, add_commands, add_context_file, add_skills, env_lines, markdown_files,
    pi_providers, provider_of_variable, read_text, read_yaml, selectors, toml_of, with_frontmatter,
};
use super::model::{
    Body, Chat, ChatRef, FileItem, Found, InPlace, Key, Kind, Role, Scope, Secret, Setting, Source,
};
use super::pi;

/// Where omp keeps its data: `PI_CODING_AGENT_DIR` when that is an omp directory, else the
/// `agent` directory of `~/.omp` (`PI_CONFIG_DIR` renames it) or of the selected profile.
pub fn root(env: &Env) -> PathBuf {
    if let Some(dir) = env.var("PI_CODING_AGENT_DIR").map(PathBuf::from)
        && pi::looks_like_omp(&dir)
    {
        return dir;
    }
    let config = env.home.join(env.var("PI_CONFIG_DIR").unwrap_or(".omp"));
    let profile = env.var("OMP_PROFILE").or_else(|| env.var("PI_PROFILE"));
    match profile.map(|name| config.join("profiles").join(name).join("agent")) {
        Some(dir) if dir.is_dir() => dir,
        _ => config.join("agent"),
    }
}

/// The roles of `config.yml`: `modelRoles`, each followed by its `retry.fallbackChains`.
fn roles(found: &mut Found, config: &Map<String, Value>, providers: &Providers) {
    let fallbacks = config
        .get("retry")
        .and_then(|retry| retry.get("fallbackChains"))
        .and_then(Value::as_object);
    let roles = config.get("modelRoles").and_then(Value::as_object);
    for (name, value) in roles.into_iter().flatten() {
        let mut wanted = selectors(value);
        if let Some(more) = fallbacks.and_then(|chains| chains.get(name)) {
            wanted.extend(selectors(more));
        }
        let chain = providers.chain(found, &format!("role {name}"), &wanted);
        if chain.is_empty() {
            found.lossy(
                Kind::Models,
                format!("role {name}: none of its models can be used"),
            );
            continue;
        }
        found.roles.push(Role {
            name: name.clone(),
            chain,
        });
    }
    for (key, _) in fallbacks.into_iter().flatten() {
        if !roles.is_some_and(|roles| roles.contains_key(key)) {
            found.lossy(
                Kind::Models,
                format!("retry.fallbackChains.{key}: azazel chains fallbacks per role, not per model"),
            );
        }
    }
}

fn settings(found: &mut Found, config: &Map<String, Value>) {
    let at = |path: &[&str]| {
        let mut value = config.get(path[0])?;
        for key in &path[1..] {
            value = value.get(*key)?;
        }
        Some(value.clone())
    };
    let text = |path: &[&str]| at(path).and_then(|value| value.as_str().map(str::to_string));
    let word = |name: &str| Some(toml::Value::String(name.to_string()));

    if let Some(preset) = text(&["symbolPreset"]) {
        let mapped = matches!(preset.as_str(), "nerd" | "unicode" | "ascii")
            .then(|| word(&preset))
            .flatten();
        pi::setting(found, "symbolPreset", &json!(preset), &["tui", "glyphs"], mapped);
    }
    if let Some(theme) = text(&["theme", "dark"]).or_else(|| text(&["theme"])) {
        pi::theme_setting(found, "theme.dark", &theme);
    }
    if let Some(preset) = text(&["statusLine", "preset"]) {
        let mapped = matches!(preset.as_str(), "default" | "minimal" | "full")
            .then(|| word(&preset))
            .flatten();
        pi::setting(
            found,
            "statusLine.preset",
            &json!(preset),
            &["statusline", "preset"],
            mapped,
        );
    }
    if let Some(separator) = text(&["statusLine", "separator"]) {
        let mapped = match separator.as_str() {
            "powerline" | "powerline-thin" => word("powerline"),
            "pipe" => word("bar"),
            "none" => word("space"),
            _ => None,
        };
        pi::setting(
            found,
            "statusLine.separator",
            &json!(separator),
            &["statusline", "separator"],
            mapped,
        );
    }
    if let Some(accent) = at(&["statusLine", "sessionAccent"]).filter(Value::is_boolean) {
        pi::setting(
            found,
            "statusLine.sessionAccent",
            &accent,
            &["tui", "session_accent"],
            toml_of(&accent),
        );
    }
    if let Some(mode) = text(&["tools", "approvalMode"]) {
        // `yolo` asks nothing; so does `auto`, which keeps azazel's guards against destroying
        // work. Lifting those too is a choice to make by hand.
        let mapped = match mode.as_str() {
            "yolo" => word("auto"),
            "always-ask" | "always-ask-cached" | "write" => word("manual"),
            _ => None,
        };
        pi::setting(
            found,
            "tools.approvalMode",
            &json!(mode),
            &["permissions", "mode"],
            mapped,
        );
        if mode == "yolo" {
            found.lossy(
                Kind::Settings,
                "tools.approvalMode: yolo becomes `auto`, which still asks before a destructive command; \
                 `permissions.mode = \"bypass\"` lifts that too",
            );
        }
    }
    if let Some(parallel) = at(&["task", "maxConcurrency"]).filter(|n| n.as_u64().is_some_and(|n| n > 0)) {
        pi::setting(
            found,
            "task.maxConcurrency",
            &parallel,
            &["team", "max_parallel"],
            toml_of(&parallel),
        );
    }
    pi::shared_settings(found, config);

    let handled = [
        "symbolPreset",
        "theme",
        "statusLine",
        "tools",
        "task",
        "compaction",
        "steeringMode",
        "hideThinkingBlock",
        "shellPath",
        "modelRoles",
        "retry",
        "cycleOrder",
        // What omp keeps for itself.
        "setupVersion",
        "dev",
        "startup",
        "autoResume",
    ];
    let unmapped: Vec<&str> = config
        .keys()
        .map(String::as_str)
        .filter(|key| !handled.contains(key))
        .collect();
    if !unmapped.is_empty() {
        found.lossy(
            Kind::Settings,
            format!("without an equivalent in azazel: {}", unmapped.join(", ")),
        );
    }
}

/// A rule file of omp as azazel reads one: its trigger is called `trigger` here.
fn rule_text(text: &str, notes: &mut Vec<String>) -> String {
    let Some((yaml, body)) = frontmatter::split(text) else {
        return text.to_string();
    };
    let Ok(meta) = frontmatter::parse(yaml) else {
        notes.push("its frontmatter could not be read and is left as written".to_string());
        return text.to_string();
    };
    let mut kept = Map::new();
    let mut dropped = Vec::new();
    for (key, value) in &meta {
        match key.as_str() {
            "description" | "globs" | "paths" | "alwaysApply" | "always" | "agents" => {
                kept.insert(key.clone(), value.clone());
            }
            "condition" | "ttsr_trigger" | "ttsrTrigger" => {
                let patterns = selectors(value);
                let trigger = match patterns.len() {
                    0 => continue,
                    1 => patterns[0].clone(),
                    _ => patterns
                        .iter()
                        .map(|pattern| format!("(?:{pattern})"))
                        .collect::<Vec<_>>()
                        .join("|"),
                };
                kept.insert("trigger".to_string(), Value::String(trigger));
            }
            other => dropped.push(other.to_string()),
        }
    }
    if !dropped.is_empty() {
        notes.push(format!("frontmatter dropped: {}", dropped.join(", ")));
    }
    with_frontmatter(&kept, body)
}

fn rules(found: &mut Found, dir: &Path, scope: &Scope) {
    for file in markdown_files(dir, true) {
        let Some(text) = read_text(found, &file) else {
            continue;
        };
        let Ok(relative) = file.strip_prefix(dir) else {
            continue;
        };
        let mut notes = Vec::new();
        let text = rule_text(&text, &mut notes);
        found.files.push(FileItem {
            kind: Kind::Rules,
            scope: scope.clone(),
            to: Path::new("rules").join("omp").join(relative).with_extension("md"),
            from: file,
            body: Body::Text(text),
            notes,
        });
    }
}

/// Opens a SQLite file of another program without changing it. A database another process
/// left mid-write cannot be read in place; a copy can.
#[cfg(feature = "memory")]
fn open_database(path: &Path) -> Result<(rusqlite::Connection, Option<tempfile::TempDir>), String> {
    use rusqlite::{Connection, OpenFlags};
    let readable = |conn: &Connection| conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()));
    if let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        && readable(&conn).is_ok()
    {
        return Ok((conn, None));
    }
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .ok_or("not a file")?
        .to_string_lossy()
        .into_owned();
    for suffix in ["", "-wal", "-shm"] {
        let from = path.with_file_name(format!("{name}{suffix}"));
        if from.is_file() {
            std::fs::copy(&from, dir.path().join(format!("{name}{suffix}"))).map_err(|e| e.to_string())?;
        }
    }
    let conn = Connection::open(dir.path().join(&name)).map_err(|e| e.to_string())?;
    readable(&conn).map_err(|e| e.to_string())?;
    Ok((conn, Some(dir)))
}

/// The credentials in `agent.db`: API keys to copy, sign-ins to repeat.
#[cfg(feature = "memory")]
fn credentials(found: &mut Found, database: &Path) {
    use super::convert::provider_id;
    use super::model::Login;
    if !database.is_file() {
        return;
    }
    let rows = open_database(database).and_then(|(conn, _copy)| {
        let mut statement = conn
            .prepare(
                "SELECT provider, credential_type, data FROM auth_credentials
                 WHERE disabled_cause IS NULL ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    });
    let rows = match rows {
        Ok(rows) => rows,
        Err(why) => return found.problem(database, why),
    };
    for (provider, kind, data) in rows {
        let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let field = |key: &str| data.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
        let provider = provider_id(&provider);
        match kind.as_str() {
            "api_key" => {
                if let Some(key) = field("key") {
                    found.keys.push(Key {
                        provider,
                        key: Secret::new(key),
                        origin: "agent.db".to_string(),
                    });
                }
            }
            _ => found.logins.push(Login {
                provider,
                who: field("email").map(str::to_string),
            }),
        }
    }
}

#[cfg(not(feature = "memory"))]
fn credentials(found: &mut Found, database: &Path) {
    if database.is_file() {
        found.lossy(
            Kind::Keys,
            "the credentials in agent.db are not read: this build of azazel cannot open SQLite files",
        );
    }
}

/// What omp's memory holds that is a memory in azazel's sense: what was retained on purpose.
/// The transcript episodes it also keeps are the conversations, which are brought over as such.
#[cfg(feature = "memory")]
fn memories(found: &mut Found, dir: &Path) {
    use super::model::MemoryItem;
    let mut databases = vec![dir.join("mnemopi.db")];
    let banks = std::fs::read_dir(dir.join("banks"))
        .into_iter()
        .flatten()
        .flatten();
    databases.extend(banks.map(|bank| bank.path().join("mnemopi.db")));
    databases.retain(|database| database.is_file());
    databases.sort();
    let mut episodes = 0u64;
    for database in databases {
        let rows = open_database(&database).and_then(|(conn, _copy)| {
            let mut statement = conn
                .prepare(
                    "SELECT content, memory_type, metadata_json FROM working_memory
                     WHERE superseded_by IS NULL ORDER BY timestamp, rowid",
                )
                .map_err(|e| e.to_string())?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(why) => {
                found.problem(&database, why);
                continue;
            }
        };
        for (content, kind, metadata) in rows {
            if kind.as_deref() == Some("episode") {
                episodes += 1;
                continue;
            }
            let cwd = metadata
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|metadata| metadata.get("cwd").and_then(Value::as_str).map(PathBuf::from));
            found.memories.push(MemoryItem {
                scope: cwd.map_or(Scope::Home, Scope::Project),
                kind: match kind.as_deref() {
                    Some("preference") => "preference",
                    Some("decision") => "decision",
                    _ => "fact",
                },
                text: content,
            });
        }
    }
    if episodes > 0 {
        found.lossy(
            Kind::Memory,
            format!(
                "{episodes} transcript episodes of mnemopi are not memories in azazel's sense; the conversations \
                 they were cut from are imported as chats"
            ),
        );
    }
}

#[cfg(not(feature = "memory"))]
fn memories(found: &mut Found, dir: &Path) {
    if dir.join("banks").is_dir() || dir.join("mnemopi.db").is_file() {
        found.lossy(
            Kind::Memory,
            "this build of azazel has no memory to bring them into",
        );
    }
}

/// Everything omp holds for the user and for `project`.
pub fn scan(env: &Env, project: &Path) -> Option<Found> {
    let root = root(env);
    if !root.is_dir() {
        return None;
    }
    let mut found = Found::new(Source::Omp, &root);
    let home = Scope::Home;
    let here = Scope::Project(project.to_path_buf());
    let ours = project.join(".omp");

    let models = ["models.yml", "models.yaml"]
        .iter()
        .map(|name| root.join(name))
        .find(|file| file.is_file());
    let providers = match models.and_then(|file| Some((read_yaml(&mut found, &file)?, file))) {
        Some((models, file)) => pi_providers(&mut found, &models, &file, true),
        None => Providers::default(),
    };

    let config = ["config.yml", "config.yaml"]
        .iter()
        .map(|name| root.join(name))
        .find(|file| file.is_file());
    if let Some(Value::Object(config)) = config.and_then(|file| read_yaml(&mut found, &file)) {
        roles(&mut found, &config, &providers);
        settings(&mut found, &config);
        // The roles Ctrl+P steps through.
        let cycle: Vec<Value> = config
            .get("cycleOrder")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|role| found.roles.iter().any(|known| known.name == *role))
            .map(|role| Value::String(format!("@{role}")))
            .collect();
        if !cycle.is_empty() {
            let value = Value::Array(cycle);
            found.settings.push(Setting {
                key: vec!["tui".to_string(), "favourites".to_string()],
                value: toml_of(&value).unwrap_or(toml::Value::Array(Vec::new())),
                from: "cycleOrder".to_string(),
            });
        }
        if found.roles.iter().any(|role| role.name == "advisor") {
            found.settings.push(Setting {
                key: vec!["advisor".to_string(), "model".to_string()],
                value: toml::Value::Array(vec![toml::Value::String("@advisor".to_string())]),
                from: "modelRoles.advisor".to_string(),
            });
        }
    }

    credentials(&mut found, &root.join("agent.db"));
    if let Some(text) = read_text(&mut found, &root.join(".env")) {
        for (variable, value) in env_lines(&text) {
            if let Some(provider) = provider_of_variable(&variable) {
                found.keys.push(Key {
                    provider,
                    key: Secret::new(value),
                    origin: format!(".env ({variable})"),
                });
            }
        }
    }

    add_skills(&mut found, &root.join("skills"), &home);
    add_skills(&mut found, &ours.join("skills"), &here);
    add_commands(&mut found, &root.join("commands"), &home, true, &providers);
    add_commands(&mut found, &ours.join("commands"), &here, true, &providers);
    add_agents(&mut found, &root.join("agents"), &home, &providers);
    add_agents(&mut found, &ours.join("agents"), &here, &providers);

    add_context_file(&mut found, &root.join("RULES.md"), &home, "omp-rules.md", env);
    add_context_file(&mut found, &root.join("AGENTS.md"), &home, "omp-agents.md", env);
    add_context_file(&mut found, &ours.join("RULES.md"), &here, "omp-rules.md", env);
    add_context_file(&mut found, &ours.join("AGENTS.md"), &here, "omp-agents.md", env);
    rules(&mut found, &root.join("rules"), &home);
    rules(&mut found, &ours.join("rules"), &here);
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = project.join(name);
        if path.is_file() {
            found.in_place.push(InPlace {
                kind: Kind::Rules,
                path,
            });
        }
    }

    pi::mcp_servers(&mut found, &root.join("mcp.json"), &home, true);
    pi::mcp_servers(&mut found, &ours.join("mcp.json"), &here, true);

    pi::session_files(&mut found, &root.join("sessions"));
    memories(&mut found, &root.join("memories").join("mnemopi"));
    Some(found)
}

/// A session with its sub-agents' conversations, its images read back from omp's blob store.
pub fn read_chat(root: &Path, reference: &ChatRef) -> Result<Chat, String> {
    pi::read_chat(reference, Some(&root.join("blobs")))
}
