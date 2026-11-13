//! Bringing what a source holds into azazel's own files. Nothing that is there is replaced
//! unless the user asked for it; what was done, left out and lost is reported line by line.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use azazel_ai::providers::presets;
use azazel_ai::{Credential, Credentials, credentials::account_key};
use azazel_core::{Config, LoadOptions, Paths, SessionStore};

use super::chat::{imported_in, write_chat};
use super::convert::redact_email;
use super::model::{Body, ChatRef, FileItem, Found, Kind, Provider, Report, Scope, Setting, Source, Status};
use super::{Options, read_chat};
use crate::init::set_config_value;

/// The settings that are lists every layer adds to: what a source has joins what is there.
const ADDED_TO: [[&str; 2]; 4] = [
    ["permissions", "allow"],
    ["permissions", "ask"],
    ["permissions", "deny"],
    ["permissions", "additional_dirs"],
];

/// The file names of the configuration in a home directory.
const CONFIG: &str = "config.toml";
const MODELS: &str = azazel_ai::models_file::FILE_NAME;

/// One source being brought over.
pub struct Run<'a> {
    pub paths: &'a Paths,
    pub options: &'a Options,
    pub report: &'a mut Report,
    pub source: Source,
    /// The directory whose `config.toml` and `models.yml` are edited: the home, or for a dry
    /// run a copy of the two, so that what would be written is checked the same way.
    pub config_home: PathBuf,
}

impl Run<'_> {
    fn say(&mut self, kind: Kind, status: Status, text: impl Into<String>) {
        self.report.push(self.source, kind, status, text);
    }

    fn config_file(&self) -> PathBuf {
        self.config_home.join(CONFIG)
    }

    /// The configuration as the files in `config_home` give it, without any project's.
    fn config(&self) -> Result<Config, String> {
        let home = self.config_home.clone();
        let paths = Paths {
            project_root: home.clone(),
            cwd: home.clone(),
            project_data: home.join("projects").join("migrate"),
            sessions_override: None,
            home,
        };
        let options = LoadOptions {
            trusted: false,
            overrides: toml::Table::new(),
            env: Vec::new(),
        };
        Config::load(&paths, &options)
            .map(|loaded| loaded.config)
            .map_err(|e| e.to_string())
    }

    /// Makes a change to the configuration files and keeps it only when they still load.
    fn edit(&self, change: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let files = [self.config_file(), self.config_home.join(MODELS)];
        let before: Vec<Option<Vec<u8>>> = files.iter().map(|file| std::fs::read(file).ok()).collect();
        let outcome = change().and_then(|()| self.config().map(drop));
        if outcome.is_err() {
            for (file, bytes) in files.iter().zip(before) {
                // Best effort: the files were writable a moment ago.
                let _ = match bytes {
                    Some(bytes) => std::fs::write(file, bytes),
                    None => std::fs::remove_file(file),
                };
            }
        }
        outcome
    }

    /// The value at `key` in the home's `config.toml`.
    fn configured(&self, key: &[String]) -> Option<toml::Value> {
        let text = std::fs::read_to_string(self.config_file()).ok()?;
        let table: toml::Table = text.parse().ok()?;
        let mut value = table.get(&key[0])?;
        for part in &key[1..] {
            value = value.get(part)?;
        }
        Some(value.clone())
    }
}

/// A scalar or a list as an item of a TOML document.
fn item_of(value: &toml::Value) -> Option<toml_edit::Item> {
    if value.is_table() {
        return None;
    }
    value
        .to_string()
        .parse::<toml_edit::Value>()
        .ok()
        .map(toml_edit::Item::Value)
}

fn shown(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The `.azazel` directory a scope's files live in.
fn base(paths: &Paths, scope: &Scope) -> PathBuf {
    match scope {
        Scope::Home => paths.home.clone(),
        Scope::Project(dir) => dir.join(".azazel"),
    }
}

// ---------------------------------------------------------------- models and settings

/// `text` of a `models.yml` with `entries` added to its providers. Comments and layout of what
/// is there stay when the entries can be written after it; replacing an entry, or adding to a
/// file that is not laid out in blocks, writes the file anew.
fn models_text(text: &str, entries: &[(String, serde_yaml_ng::Value)]) -> Result<String, String> {
    use serde_yaml_ng::{Mapping, Value};
    let root: Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    let providers = match &root {
        Value::Null => Mapping::new(),
        Value::Mapping(root) => match root.get("providers") {
            None | Some(Value::Null) => Mapping::new(),
            Some(Value::Mapping(providers)) => providers.clone(),
            Some(_) => return Err("`providers` is not a mapping".to_string()),
        },
        _ => return Err("the file is not a mapping".to_string()),
    };
    let block = |indent: usize| -> Result<String, String> {
        let mut out = String::new();
        for (id, entry) in entries {
            let mut one = Mapping::new();
            one.insert(Value::String(id.clone()), entry.clone());
            let yaml = serde_yaml_ng::to_string(&one).map_err(|e| e.to_string())?;
            for line in yaml.lines() {
                out.push_str(&" ".repeat(indent));
                out.push_str(line);
                out.push('\n');
            }
        }
        Ok(out)
    };
    let replaces = entries.iter().any(|(id, _)| providers.contains_key(id.as_str()));
    // Where the providers of the file are indented to, when it lists them in a block.
    let lines: Vec<&str> = text.lines().collect();
    let heading = lines.iter().position(|line| {
        let rest = line.strip_prefix("providers:").map(str::trim);
        rest.is_some_and(|rest| rest.is_empty() || rest.starts_with('#'))
    });
    let indent = heading.and_then(|at| {
        lines[at + 1..]
            .iter()
            .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .map(|line| line.len() - line.trim_start().len())
    });
    let appended = match (replaces, providers.is_empty(), indent) {
        (true, _, _) => None,
        (false, true, _) if heading.is_none() && matches!(root, Value::Null) => {
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("providers:\n");
            out.push_str(&block(2)?);
            Some(out)
        }
        (false, false, Some(indent)) if indent > 0 => {
            let mut out = text.to_string();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&block(indent)?);
            Some(out)
        }
        _ => None,
    };
    let out = match appended {
        Some(out) => out,
        None => {
            let mut providers = providers;
            for (id, entry) in entries {
                providers.insert(Value::String(id.clone()), entry.clone());
            }
            let mut root = Mapping::new();
            root.insert(Value::String("providers".to_string()), Value::Mapping(providers));
            serde_yaml_ng::to_string(&root).map_err(|e| e.to_string())?
        }
    };
    azazel_ai::models_file::parse(&out)?;
    Ok(out)
}

/// A provider as an entry of `models.yml`.
fn provider_entry(provider: &Provider) -> Result<serde_yaml_ng::Value, String> {
    use serde_yaml_ng::Value;
    let mut entry = match serde_yaml_ng::to_value(&provider.settings).map_err(|e| e.to_string())? {
        Value::Mapping(entry) => entry,
        _ => serde_yaml_ng::Mapping::new(),
    };
    let mut models = Vec::new();
    for (id, model) in &provider.models {
        let mut row = serde_yaml_ng::Mapping::new();
        row.insert(Value::String("id".to_string()), Value::String(id.clone()));
        if let Value::Mapping(fields) = serde_yaml_ng::to_value(model).map_err(|e| e.to_string())? {
            row.extend(fields);
        }
        models.push(Value::Mapping(row));
    }
    if !models.is_empty() {
        entry.insert(Value::String("models".to_string()), Value::Sequence(models));
    }
    Ok(Value::Mapping(entry))
}

fn models(run: &mut Run<'_>, found: &Found) {
    let overwrite = run.options.overwrite;
    let models_file = run.config_home.join(MODELS);
    for provider in &found.providers {
        let what = match provider.models.len() {
            0 => format!("provider {}", provider.id),
            1 => format!("provider {} with 1 model", provider.id),
            n => format!("provider {} with {n} models", provider.id),
        };
        let existing = std::fs::read_to_string(&models_file).unwrap_or_default();
        let in_models = azazel_ai::models_file::parse(&existing).is_ok_and(|file| {
            let prefix = format!("{}/", provider.id);
            file.providers.contains_key(&provider.id)
                || file.models.keys().any(|key| key.starts_with(&prefix))
        });
        let in_config = run
            .configured(&["providers".to_string(), provider.id.clone()])
            .is_some();
        if (in_models || in_config) && !overwrite {
            let place = if in_models { MODELS } else { CONFIG };
            run.say(
                Kind::Models,
                Status::Skipped,
                format!(
                    "{what}: `{}` is already in {place} (--overwrite replaces it)",
                    provider.id
                ),
            );
            continue;
        }
        let written = provider_entry(provider).and_then(|entry| {
            run.edit(|| {
                let text = models_text(&existing, &[(provider.id.clone(), entry)])?;
                std::fs::create_dir_all(&run.config_home).map_err(|e| e.to_string())?;
                std::fs::write(&models_file, text).map_err(|e| e.to_string())
            })
        });
        match written {
            Ok(()) => run.say(Kind::Models, Status::Imported, format!("{what} -> {MODELS}")),
            Err(why) => run.say(Kind::Models, Status::Failed, format!("{what}: {why}")),
        }
    }
    for role in &found.roles {
        let value = toml::Value::Array(role.chain.iter().cloned().map(toml::Value::String).collect());
        let setting = Setting {
            key: vec!["roles".to_string(), role.name.clone()],
            value,
            from: String::new(),
        };
        let what = format!("role {} = {}", role.name, role.chain.join(", "));
        set(run, Kind::Models, &setting, &what);
    }
}

/// Writes one key of `config.toml`, unless it is set to something else already.
fn set(run: &mut Run<'_>, kind: Kind, setting: &Setting, what: &str) {
    let key: Vec<&str> = setting.key.iter().map(String::as_str).collect();
    let name = setting.key.join(".");
    let existing = run.configured(&setting.key);
    let joins = ADDED_TO.iter().any(|added| added[..] == key[..]);
    let value = match (&existing, &setting.value) {
        (Some(same), wanted) if same == wanted => {
            return run.say(kind, Status::Skipped, format!("{what}: already so"));
        }
        // A list every layer adds to takes what it does not have yet.
        (Some(toml::Value::Array(present)), toml::Value::Array(wanted)) if joins => {
            let mut all = present.clone();
            all.extend(wanted.iter().filter(|rule| !present.contains(rule)).cloned());
            if all.len() == present.len() {
                return run.say(kind, Status::Skipped, format!("{what}: already there"));
            }
            toml::Value::Array(all)
        }
        (Some(other), _) if !run.options.overwrite => {
            return run.say(
                kind,
                Status::Skipped,
                format!(
                    "{what}: {name} is already {} (--overwrite replaces it)",
                    shown(other)
                ),
            );
        }
        _ => setting.value.clone(),
    };
    let Some(item) = item_of(&value) else {
        return run.say(
            kind,
            Status::Failed,
            format!("{what}: not a value {CONFIG} can hold"),
        );
    };
    let config = run.config_file();
    match run.edit(|| set_config_value(&config, &key, Some(item)).map_err(|e| format!("{e:#}"))) {
        Ok(()) => run.say(kind, Status::Imported, what.to_string()),
        Err(why) => run.say(kind, Status::Failed, format!("{what}: {why}")),
    }
}

fn settings(run: &mut Run<'_>, found: &Found) {
    for setting in &found.settings {
        let what = format!(
            "{} -> {} = {}",
            setting.from,
            setting.key.join("."),
            shown(&setting.value)
        );
        set(run, Kind::Settings, setting, &what);
    }
}

// ---------------------------------------------------------------- keys

fn keys(run: &mut Run<'_>, found: &Found) {
    let credentials = Credentials::new(run.paths.auth_file());
    let mut stored = credentials.load();
    let custom = run.config().map(|config| config.providers).unwrap_or_default();
    // How many keys of each provider this source has given so far: the first is the default
    // account, the others are accounts beside it.
    let mut given: HashMap<&str, usize> = HashMap::new();
    for key in &found.keys {
        let provider = key.provider.as_str();
        if !presets::is_builtin(provider) && !custom.contains_key(provider) {
            run.say(
                Kind::Keys,
                Status::Lossy,
                format!(
                    "{provider}: its key ({}) is left out: azazel has no provider of that name",
                    key.origin
                ),
            );
            continue;
        }
        let same = |credential: &Credential| matches!(credential, Credential::ApiKey { key: known } if known == key.key.expose());
        if stored
            .iter()
            .any(|(slot, credential)| slot.split('#').next() == Some(provider) && same(credential))
        {
            run.say(
                Kind::Keys,
                Status::Skipped,
                format!("{provider}: the key from {} is already stored", key.origin),
            );
            continue;
        }
        let count = given.entry(provider).or_insert(0);
        *count += 1;
        let label = match *count {
            1 => azazel_ai::DEFAULT_ACCOUNT.to_string(),
            n => format!("{}-{n}", run.source.name()),
        };
        let slot = account_key(provider, &label);
        let account = match label.as_str() {
            azazel_ai::DEFAULT_ACCOUNT => String::new(),
            label => format!(" as account `{label}`"),
        };
        match stored.get(&slot) {
            Some(Credential::OAuth { .. }) => {
                run.say(
                    Kind::Keys,
                    Status::Skipped,
                    format!(
                        "{provider}: a sign-in is stored for it and is kept; `azazel login {provider} --key --as \
                         <label>` adds the key from {} beside it",
                        key.origin
                    ),
                );
                continue;
            }
            Some(_) if !run.options.overwrite => {
                run.say(
                    Kind::Keys,
                    Status::Skipped,
                    format!(
                        "{provider}: another key is stored for it (--overwrite replaces it with the one from {})",
                        key.origin
                    ),
                );
                continue;
            }
            _ => {}
        }
        let credential = Credential::ApiKey {
            key: key.key.expose().to_string(),
        };
        let written = match run.options.dry_run {
            true => Ok(()),
            false => credentials.store(&slot, credential.clone()),
        };
        match written {
            Ok(()) => {
                stored.insert(slot, credential);
                run.say(
                    Kind::Keys,
                    Status::Imported,
                    format!("{provider}: API key from {}{account} -> auth.json", key.origin),
                );
            }
            Err(e) => run.say(Kind::Keys, Status::Failed, format!("{provider}: {e}")),
        }
    }

    let signs_in: Vec<String> = presets::resolve(&BTreeMap::new())
        .unwrap_or_default()
        .into_iter()
        .filter(|provider| provider.oauth.is_some())
        .map(|provider| provider.id)
        .collect();
    let mut named: Vec<String> = Vec::new();
    for login in &found.logins {
        let who = match &login.who {
            Some(who) => format!(" ({})", redact_email(who)),
            None => String::new(),
        };
        let line = match signs_in.contains(&login.provider) {
            true => format!(
                "{}{who}: sign-ins are not copied; run `azazel login {}`",
                login.provider, login.provider
            ),
            false => format!(
                "{}{who}: azazel has no sign-in for this provider; use an API key",
                login.provider
            ),
        };
        if !named.contains(&line) {
            let status = match signs_in.contains(&login.provider) {
                true => Status::Login,
                false => Status::Lossy,
            };
            run.say(Kind::Keys, status, line.clone());
            named.push(line);
        }
    }
}

// ---------------------------------------------------------------- files

/// Copies a file, or a directory with everything below it. Links are not followed.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let kind = std::fs::symlink_metadata(from)?.file_type();
    if kind.is_symlink() {
        return Ok(());
    }
    if !kind.is_dir() {
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir)?;
        }
        return std::fs::copy(from, to).map(drop);
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

fn file(run: &mut Run<'_>, item: &FileItem) {
    let to = base(run.paths, &item.scope).join(&item.to);
    let what = format!("{} -> {}", item.from.display(), to.display());
    if to.exists() && !run.options.overwrite {
        return run.say(
            item.kind,
            Status::Skipped,
            format!("{what}: already there (--overwrite replaces it)"),
        );
    }
    let written = match (&item.body, run.options.dry_run) {
        (_, true) => Ok(()),
        (Body::Copy, false) => copy_tree(&item.from, &to),
        (Body::Text(text), false) => to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&to, text)),
    };
    match written {
        Ok(()) => {
            run.say(item.kind, Status::Imported, what);
            for note in &item.notes {
                run.say(item.kind, Status::Lossy, format!("{}: {note}", item.to.display()));
            }
        }
        Err(e) => run.say(item.kind, Status::Failed, format!("{what}: {e}")),
    }
}

// ---------------------------------------------------------------- MCP

#[cfg(feature = "mcp")]
fn servers(run: &mut Run<'_>, found: &Found) {
    use azazel_mcp::config::{add_server, defined};
    for server in &found.servers {
        let name = &server.name;
        let target = match &server.scope {
            Scope::Home => run.paths.home_mcp(),
            Scope::Project(dir) => dir.join(".azazel").join("mcp.json"),
        };
        let (mut entry, note) = match azazel_mcp::import::convert(&server.entry) {
            Ok(converted) => converted,
            Err(why) => {
                run.say(
                    Kind::Mcp,
                    Status::Lossy,
                    format!("server {name} is left out: {why}"),
                );
                continue;
            }
        };
        entry.timeout = server.timeout;
        let names = |key: &str| -> Vec<String> {
            server.entry[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|name| name.as_str().map(str::to_string))
                .collect()
        };
        entry.include_tools = names("includeTools");
        entry.exclude_tools = names("excludeTools");
        let what = format!("server {name} -> {}", target.display());
        if defined(&target).contains(name) && !run.options.overwrite {
            run.say(
                Kind::Mcp,
                Status::Skipped,
                format!("{what}: a server of that name is there (--overwrite replaces it)"),
            );
            continue;
        }
        let written = match run.options.dry_run {
            true => Ok(()),
            false => add_server(&target, name, &entry),
        };
        match written {
            Ok(()) => {
                run.say(Kind::Mcp, Status::Imported, what);
                if let Some(note) = note {
                    run.say(Kind::Mcp, Status::Lossy, format!("server {name}: {note}"));
                }
            }
            Err(e) => run.say(Kind::Mcp, Status::Failed, format!("{what}: {e}")),
        }
    }
}

#[cfg(not(feature = "mcp"))]
fn servers(run: &mut Run<'_>, found: &Found) {
    if !found.servers.is_empty() {
        run.say(
            Kind::Mcp,
            Status::Lossy,
            format!("{} servers: this build of azazel has no MCP", found.servers.len()),
        );
    }
}

// ---------------------------------------------------------------- chats

/// The sessions directory of the project a conversation was held in.
fn sessions_of(paths: &Paths, cwd: &Path) -> PathBuf {
    let home = paths.home.to_string_lossy();
    Paths::discover(cwd, Some(&home)).sessions_dir()
}

/// A conversation as the report names it before it has been read: by its title, or by the
/// source's id of it when the title is somewhere inside the file.
fn chat_label(reference: &ChatRef) -> String {
    let title = reference.title.as_deref().unwrap_or(&reference.id);
    match &reference.cwd {
        Some(cwd) => format!("{title} ({})", cwd.display()),
        None => title.to_string(),
    }
}

fn chats(run: &mut Run<'_>, found: &Found) {
    let source = run.source;
    let mut known: HashMap<PathBuf, std::collections::HashSet<(String, String)>> = HashMap::new();
    let total = found.chats.len();
    // When each imported conversation was last used, for the warning about old ones.
    let mut last_used: Vec<jiff::Timestamp> = Vec::new();
    for (index, reference) in found.chats.iter().enumerate() {
        let label = chat_label(reference);
        let Some(cwd) = &reference.cwd else {
            run.say(
                Kind::Chats,
                Status::Lossy,
                format!(
                    "{label}: left out, {} does not say which directory it was held in",
                    source.label()
                ),
            );
            continue;
        };
        let dir = sessions_of(run.paths, cwd);
        let imported = known.entry(dir.clone()).or_insert_with(|| imported_in(&dir));
        let mark = (source.name().to_string(), reference.id.clone());
        if imported.contains(&mark) {
            run.say(Kind::Chats, Status::Skipped, format!("{label}: already imported"));
            continue;
        }
        if run.options.dry_run {
            run.say(Kind::Chats, Status::Imported, label);
            continue;
        }
        (run.options.progress)(&format!(
            "{}: conversation {} of {total}: {label}",
            source.label(),
            index + 1
        ));
        let mut chat = match read_chat(found, reference) {
            Ok(chat) => chat,
            Err(why) => {
                run.say(Kind::Chats, Status::Failed, format!("{label}: {why}"));
                continue;
            }
        };
        // The id it is found again by is the one it was found by now, whatever the file says
        // of itself (a resumed Claude Code session carries lines of the one before).
        chat.id = reference.id.clone();
        let spoken = chat.turns.iter().any(|turn| {
            matches!(
                turn.kind,
                super::model::TurnKind::User(_) | super::model::TurnKind::Assistant { .. }
            )
        });
        if !spoken {
            run.say(
                Kind::Chats,
                Status::Skipped,
                format!("{label}: nothing was said in it"),
            );
            continue;
        }
        let label = match &chat.title {
            Some(title) => format!("{title} ({})", cwd.display()),
            None => label,
        };
        match write_chat(&dir, source, &chat, &reference.file) {
            Ok(written) => {
                imported.insert(mark);
                last_used.push(written.last);
                let agents = match written.agents {
                    0 => String::new(),
                    1 => ", 1 sub-agent".to_string(),
                    n => format!(", {n} sub-agents"),
                };
                run.say(
                    Kind::Chats,
                    Status::Imported,
                    format!("{label}: {} messages{agents}", written.messages),
                );
                if !written.notes.is_empty() {
                    run.say(
                        Kind::Chats,
                        Status::Lossy,
                        format!("{label}: {}", written.notes.join("; ")),
                    );
                }
            }
            Err(e) => run.say(Kind::Chats, Status::Failed, format!("{label}: {e}")),
        }
    }
    if last_used.is_empty() {
        return;
    }
    // The first listing of a project's sessions reads every one of them; it is done now, while
    // a wait is expected, and kept beside them.
    (run.options.progress)(&format!("{}: indexing the imported sessions", source.label()));
    for dir in known.keys() {
        SessionStore::list(dir);
    }
    // Sessions keep the times of the conversations they were, and old ones are what the
    // retention setting deletes.
    let days = run
        .config()
        .map(|config| config.session.retention_days)
        .unwrap_or(0);
    let cutoff = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * i64::from(days));
    let old = last_used.iter().filter(|used| **used < cutoff).count();
    if days > 0 && old > 0 {
        run.say(
            Kind::Chats,
            Status::Lossy,
            format!(
                "{old} of the imported conversations were last used more than {days} days ago: \
                 `session.retention_days = {days}` deletes them when azazel next starts; pin them, or raise or \
                 remove the setting"
            ),
        );
    }
}

// ---------------------------------------------------------------- memory

#[cfg(feature = "memory")]
async fn memories(run: &mut Run<'_>, found: &Found) {
    use azazel_memory::{MemoryExt, Scope as Store};
    // By the project each belongs to; the user's own first.
    let mut groups: Vec<(&Scope, Vec<(String, &str)>)> = Vec::new();
    for memory in &found.memories {
        let row = (memory.text.clone(), memory.kind);
        match groups.iter_mut().find(|(scope, _)| *scope == &memory.scope) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((&memory.scope, vec![row])),
        }
    }
    let tags = vec!["imported".to_string(), run.source.name().to_string()];
    for (scope, rows) in groups {
        let place = match scope {
            Scope::Home => "every project".to_string(),
            Scope::Project(dir) => dir.display().to_string(),
        };
        if run.options.dry_run {
            run.say(
                Kind::Memory,
                Status::Imported,
                format!(
                    "{} memories for {place} (those already remembered are left out)",
                    rows.len()
                ),
            );
            continue;
        }
        let home = run.paths.home.to_string_lossy().into_owned();
        let (paths, store_scope) = match scope {
            Scope::Home => (run.paths.clone(), Store::Global),
            Scope::Project(dir) => (Paths::discover(dir, Some(&home)), Store::Project),
        };
        let stored = match run.config() {
            Ok(config) => {
                let credentials = std::sync::Arc::new(Credentials::new(paths.auth_file()));
                match MemoryExt::open(&paths, &config, credentials) {
                    Ok(memory) => memory.store().import(&rows, &tags, store_scope).await,
                    Err(why) => Err(why),
                }
            }
            Err(why) => Err(why),
        };
        match stored {
            Ok((stored, present)) => {
                if stored > 0 {
                    run.say(
                        Kind::Memory,
                        Status::Imported,
                        format!("{stored} memories for {place}"),
                    );
                }
                if present > 0 {
                    run.say(
                        Kind::Memory,
                        Status::Skipped,
                        format!("{present} memories for {place}: already remembered"),
                    );
                }
            }
            Err(why) => run.say(
                Kind::Memory,
                Status::Failed,
                format!("memories for {place}: {why}"),
            ),
        }
    }
}

#[cfg(not(feature = "memory"))]
async fn memories(run: &mut Run<'_>, found: &Found) {
    if !found.memories.is_empty() {
        run.say(
            Kind::Memory,
            Status::Lossy,
            format!(
                "{} memories: this build of azazel has no memory",
                found.memories.len()
            ),
        );
    }
}

// ---------------------------------------------------------------- everything

/// Brings over the kinds of `found` the options name, reporting on each thing.
pub async fn apply(run: &mut Run<'_>, found: &Found) {
    let wanted = |kind: Kind| run.options.kinds.contains(&kind);
    let edits_config = wanted(Kind::Models) || wanted(Kind::Settings);
    let mut config_loads = true;
    if edits_config
        && (!found.roles.is_empty() || !found.providers.is_empty() || !found.settings.is_empty())
        && let Err(why) = run.config()
    {
        config_loads = false;
        let kind = if wanted(Kind::Models) {
            Kind::Models
        } else {
            Kind::Settings
        };
        run.say(
            kind,
            Status::Failed,
            format!("azazel's own configuration does not load, so nothing is written to it: {why}"),
        );
    }
    if wanted(Kind::Models) && config_loads {
        models(run, found);
    }
    if wanted(Kind::Settings) && config_loads {
        settings(run, found);
    }
    if wanted(Kind::Keys) {
        keys(run, found);
    }
    for kind in [Kind::Skills, Kind::Commands, Kind::Agents, Kind::Rules] {
        if wanted(kind) {
            for item in found.files.iter().filter(|item| item.kind == kind) {
                file(run, item);
            }
        }
    }
    if wanted(Kind::Mcp) {
        servers(run, found);
    }
    if wanted(Kind::Chats) {
        chats(run, found);
    }
    if wanted(Kind::Memory) {
        memories(run, found).await;
    }
    for place in &found.in_place {
        if wanted(place.kind) {
            run.say(
                place.kind,
                Status::InPlace,
                format!("{}: azazel reads it where it is", place.path.display()),
            );
        }
    }
    for note in &found.lossy {
        if wanted(note.kind) {
            run.say(note.kind, Status::Lossy, note.text.clone());
        }
    }
    for problem in &found.problems {
        // A file that could not be read is nobody's kind in particular.
        run.say(Kind::Settings, Status::Failed, problem.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_are_added_after_what_a_models_file_holds_keeping_its_comments() {
        let existing = "# my gateways\nproviders:\n    mine:   # four spaces\n        api: openai-chat\n        \
                        base_url: https://mine.example/v1\n";
        let entry: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            "api: anthropic-messages\nbase_url: https://theirs.example\nmodels:\n- id: big\n",
        )
        .unwrap();
        let text = models_text(existing, &[("theirs".to_string(), entry.clone())]).unwrap();
        assert!(text.starts_with(existing), "{text}");
        assert!(
            text.contains("\n    theirs:\n      api: anthropic-messages\n"),
            "{text}"
        );
        let file = azazel_ai::models_file::parse(&text).unwrap();
        assert_eq!(file.providers.len(), 2);
        assert!(file.models.contains_key("theirs/big"));

        // An empty file gets the heading; a file written in one line is written anew.
        let fresh = models_text("", &[("theirs".to_string(), entry.clone())]).unwrap();
        assert!(fresh.starts_with("providers:\n  theirs:\n"), "{fresh}");
        let flow = models_text(
            "providers: { mine: { api: openai-chat, base_url: 'https://mine.example/v1' } }\n",
            &[("theirs".to_string(), entry.clone())],
        )
        .unwrap();
        assert_eq!(azazel_ai::models_file::parse(&flow).unwrap().providers.len(), 2);

        // Replacing an entry keeps the others.
        let replaced = models_text(&text, &[("mine".to_string(), entry)]).unwrap();
        let file = azazel_ai::models_file::parse(&replaced).unwrap();
        assert_eq!(
            file.providers["mine"].base_url.as_deref(),
            Some("https://theirs.example")
        );
        assert_eq!(file.providers.len(), 2);
    }

    #[test]
    fn a_models_file_that_would_not_load_is_refused() {
        let broken: serde_yaml_ng::Value = serde_yaml_ng::from_str("api: no-such-format\n").unwrap();
        let refused = models_text("", &[("odd".to_string(), broken)]).unwrap_err();
        assert!(refused.contains("providers.odd"), "{refused}");
    }

    #[test]
    fn values_are_written_as_toml_writes_them() {
        let list = toml::Value::Array(vec![
            toml::Value::String("a b".into()),
            toml::Value::String("c".into()),
        ]);
        assert_eq!(item_of(&list).unwrap().to_string(), r#"["a b", "c"]"#);
        assert_eq!(item_of(&toml::Value::Boolean(true)).unwrap().to_string(), "true");
        assert_eq!(
            item_of(&toml::Value::String("x".into())).unwrap().to_string(),
            r#""x""#
        );
    }
}
