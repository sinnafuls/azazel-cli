//! What the readers of the sources share: where a user's files are, how the names of providers,
//! wire formats and models translate, and how Markdown resources are rewritten for azazel.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use azazel_ai::providers::presets;
use azazel_ai::{AuthStyle, Cost, ModelOverride, ModelSpec, ProviderSettings, ThinkingLevel, WireApi};
use azazel_core::frontmatter;
use serde_json::{Map, Value};

use super::model::{Body, FileItem, Found, Key, Kind, Provider, Scope, Secret, Source};

/// The user's directories and environment, as the sources look for their data in them.
#[derive(Debug, Clone)]
pub struct Env {
    /// The user's home directory.
    pub home: PathBuf,
    /// Where programs keep per-user configuration: `%APPDATA%` on Windows,
    /// `~/Library/Application Support` on macOS, `$XDG_CONFIG_HOME` or `~/.config` elsewhere.
    pub config: PathBuf,
    vars: HashMap<String, String>,
}

impl Env {
    /// The directories of the user running this process. `None` when the home directory is
    /// not known.
    pub fn detect() -> Option<Env> {
        let vars: HashMap<String, String> = std::env::vars().collect();
        let home = std::env::home_dir()?;
        let set = |name: &str| {
            vars.get(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let config = if cfg!(windows) {
            set("APPDATA").unwrap_or_else(|| home.join("AppData").join("Roaming"))
        } else if cfg!(target_os = "macos") {
            home.join("Library").join("Application Support")
        } else {
            set("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
        };
        Some(Env { home, config, vars })
    }

    /// A home directory with nothing set in the environment.
    pub fn at(home: &Path) -> Env {
        Env {
            home: home.to_path_buf(),
            config: home.join(".config"),
            vars: HashMap::new(),
        }
    }

    pub fn with_var(mut self, name: &str, value: &str) -> Env {
        self.vars.insert(name.to_string(), value.to_string());
        self
    }

    /// An environment variable that is set to something.
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    /// The directory an environment variable names, else `default` below the home directory.
    pub fn dir(&self, variable: &str, default: &str) -> PathBuf {
        self.var(variable)
            .map_or_else(|| self.home.join(default), PathBuf::from)
    }
}

// ---------------------------------------------------------------- reading

/// The text of a file that may be absent. A file that is there and cannot be read is a problem
/// of the source.
pub fn read_text(found: &mut Found, path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            found.problem(path, e);
            None
        }
    }
}

pub fn read_json(found: &mut Found, path: &Path) -> Option<Value> {
    let text = read_text(found, path)?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(e) => {
            found.problem(path, format!("not valid JSON: {e}"));
            None
        }
    }
}

pub fn read_yaml(found: &mut Found, path: &Path) -> Option<Value> {
    let text = read_text(found, path)?;
    match serde_yaml_ng::from_str::<Value>(&text) {
        Ok(value) => Some(value),
        Err(e) => {
            found.problem(path, format!("not valid YAML: {e}"));
            None
        }
    }
}

pub fn read_toml(found: &mut Found, path: &Path) -> Option<toml::Table> {
    let text = read_text(found, path)?;
    match text.parse::<toml::Table>() {
        Ok(table) => Some(table),
        Err(e) => {
            found.problem(path, format!("not valid TOML: {e}"));
            None
        }
    }
}

/// `NAME=value` lines of an environment file: `export` prefixes, quotes and `#` comments are
/// understood.
pub fn env_lines(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        if line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        let value = match value.chars().next() {
            Some(quote @ ('"' | '\'')) => value[1..].split(quote).next().unwrap_or_default(),
            _ => value.split(" #").next().unwrap_or_default().trim_end(),
        };
        let named = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if named && !value.is_empty() {
            out.push((name.to_string(), value.to_string()));
        }
    }
    out
}

/// A time as the sources write it: an RFC 3339 string, or a number of milliseconds (or of
/// seconds) since 1970.
pub fn time_of(value: Option<&Value>) -> Option<jiff::Timestamp> {
    match value? {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => {
            let number = number.as_f64()?;
            // No conversation predates 1973 in milliseconds, and none postdates 5138 in seconds.
            let millis = if number >= 1e11 { number } else { number * 1000.0 };
            jiff::Timestamp::from_millisecond(millis as i64).ok()
        }
        _ => None,
    }
}

/// The first line of `text`, cut to what a name of a session can hold.
pub fn title_of(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut title: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        title.push('…');
    }
    Some(title)
}

/// An e-mail address with the name before the `@` cut to its first letter.
pub fn redact_email(email: &str) -> String {
    match email.split_once('@') {
        Some((name, domain)) => {
            let first: String = name.chars().take(1).collect();
            format!("{first}***@{domain}")
        }
        None => email.to_string(),
    }
}

/// A JSON value as a TOML one; `None` for what TOML cannot hold (a null).
pub fn toml_of(value: &Value) -> Option<toml::Value> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(flag) => toml::Value::Boolean(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(whole) => toml::Value::Integer(whole),
            None => toml::Value::Float(number.as_f64()?),
        },
        Value::String(text) => toml::Value::String(text.clone()),
        Value::Array(items) => toml::Value::Array(items.iter().filter_map(toml_of).collect()),
        Value::Object(map) => toml::Value::Table(
            map.iter()
                .filter_map(|(key, value)| Some((key.clone(), toml_of(value)?)))
                .collect(),
        ),
    })
}

/// A TOML value as a JSON one.
pub fn json_of(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::String(text.clone()),
        toml::Value::Integer(whole) => Value::from(*whole),
        toml::Value::Float(number) => {
            serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number)
        }
        toml::Value::Boolean(flag) => Value::Bool(*flag),
        toml::Value::Datetime(when) => Value::String(when.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(json_of).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(key, value)| (key.clone(), json_of(value)))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------- providers and models

/// The azazel id of a provider the sources name differently.
pub fn provider_id(id: &str) -> String {
    match id {
        "lm-studio" => "lmstudio",
        "llama.cpp" | "llama-cpp" => "llamacpp",
        "claude" | "claude-api" => "anthropic",
        "gemini" => "google",
        "copilot" => "github-copilot",
        "codex" | "openai-chatgpt" => "openai-codex",
        "google-vertex" => "vertex",
        "amazon-bedrock" => "bedrock",
        "azure-openai" | "azure-openai-responses" => "azure",
        "kimi-coding" => "kimi-code",
        "vercel" => "vercel-ai-gateway",
        other => other,
    }
    .to_string()
}

/// The wire format a source's name for one stands for.
pub fn wire_api(name: &str) -> Option<WireApi> {
    Some(match name {
        "openai-completions" | "openai-chat" | "chat" | "openai-compatible" => WireApi::OpenAiChat,
        "openai-responses" | "openai-codex-responses" | "responses" => WireApi::OpenAiResponses,
        "anthropic-messages" | "anthropic-compatible" | "anthropic" => WireApi::AnthropicMessages,
        "google-generative-ai" | "google-generative" => WireApi::GoogleGenerative,
        "google-gemini-cli" => WireApi::GoogleGeminiCli,
        "azure-openai-responses" => WireApi::AzureOpenAiResponses,
        "bedrock-converse-stream" | "bedrock-converse" => WireApi::BedrockConverse,
        "ollama-chat" => WireApi::OllamaChat,
        "openrouter" => WireApi::OpenRouter,
        _ => return None,
    })
}

/// What a wire format is called in `config.toml`.
pub fn api_name(api: WireApi) -> String {
    serde_json::to_value(api)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The wire format a built-in provider speaks, for the replies of a conversation.
pub fn api_of_provider(provider: &str) -> Option<WireApi> {
    presets::resolve(&BTreeMap::new())
        .ok()?
        .into_iter()
        .find(|preset| preset.id == provider)
        .map(|preset| preset.api)
}

/// The built-in provider whose key the environment variable `name` holds.
pub fn provider_of_variable(name: &str) -> Option<String> {
    let providers = presets::resolve(&BTreeMap::new()).ok()?;
    // The provider that looks for the variable first is the one it was made for.
    let rank = |provider: &presets::ProviderConfig| provider.api_key_env.iter().position(|var| var == name);
    providers
        .iter()
        .filter_map(|provider| Some((rank(provider)?, provider.id.clone())))
        .min()
        .map(|(_, id)| id)
}

/// The providers the model selectors of a source may name, and where each one's models went.
#[derive(Debug, Default, Clone)]
pub struct Providers {
    /// The providers the source defines itself, by their azazel id.
    custom: HashSet<String>,
    /// `(provider, model)` as the source writes them, for a model that was given a provider of
    /// its own because it speaks another wire format than its siblings.
    moved: HashMap<(String, String), String>,
}

impl Providers {
    /// A model selector of a source (`provider/model[:effort]`, or `@role`) as azazel writes
    /// it. `Err` says why azazel cannot use it.
    pub fn spec(&self, selector: &str) -> Result<String, String> {
        let selector = selector.trim();
        if let Some(role) = selector.strip_prefix('@') {
            return match role.is_empty() {
                true => Err("an empty role name".to_string()),
                false => Ok(selector.to_string()),
            };
        }
        let (provider, rest) = selector
            .split_once('/')
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .ok_or_else(|| format!("`{selector}` is not `provider/model`"))?;
        // A trailing `:word` is an effort only when it names one; model ids hold colons too.
        let (model, effort) = match rest.rsplit_once(':') {
            Some((model, suffix)) if !model.is_empty() => match suffix.parse::<ThinkingLevel>() {
                Ok(level) => (model, Some(level)),
                // Efforts that mean "whatever is the default".
                Err(_) if matches!(suffix, "auto" | "inherit" | "none" | "default") => (model, None),
                Err(_) => (rest, None),
            },
            _ => (rest, None),
        };
        let provider = match self.moved.get(&(provider.to_string(), model.to_string())) {
            Some(moved) => moved.clone(),
            None => provider_id(provider),
        };
        if !presets::is_builtin(&provider) && !self.custom.contains(&provider) {
            return Err(format!("azazel has no provider `{provider}`"));
        }
        let spec = match effort {
            Some(level) => format!("{provider}/{model}:{level}"),
            None => format!("{provider}/{model}"),
        };
        spec.parse::<ModelSpec>().map_err(|e| e.to_string())?;
        Ok(spec)
    }

    /// A chain of selectors, without the ones azazel cannot use; those are noted on `found`
    /// under `what` (`role smol`).
    pub fn chain(&self, found: &mut Found, what: &str, selectors: &[String]) -> Vec<String> {
        let mut chain: Vec<String> = Vec::new();
        // A model is in a chain once, whatever effort it is named with a second time.
        let model = |spec: &str| match spec.parse::<ModelSpec>() {
            Ok(ModelSpec::Model(model)) => model.key(),
            _ => spec.to_string(),
        };
        for selector in selectors {
            match self.spec(selector) {
                Ok(spec) if chain.iter().any(|known| model(known) == model(&spec)) => {}
                Ok(spec) => chain.push(spec),
                Err(why) => found.lossy(Kind::Models, format!("{what}: `{selector}` is left out: {why}")),
            }
        }
        chain
    }

    pub fn add_custom(&mut self, id: &str) {
        self.custom.insert(id.to_string());
    }
}

/// The selectors in a role's value: one string, a comma-separated one, or a list.
pub fn selectors(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => text
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect(),
        Value::Array(items) => items.iter().flat_map(selectors).collect(),
        _ => Vec::new(),
    }
}

fn text_of<'a>(entry: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
        .collect()
}

/// The models of a provider: each one's id, and what the source says about it.
type Models = Vec<(String, ModelOverride)>;

/// The suffix a provider's id gets for the models that speak `api` when its other models speak
/// another format.
fn api_suffix(api: WireApi) -> &'static str {
    match api {
        WireApi::OpenAiChat => "chat",
        WireApi::OpenAiResponses => "responses",
        WireApi::AnthropicMessages => "anthropic",
        WireApi::GoogleGenerative => "google",
        _ => "alt",
    }
}

/// What pi and omp say about a model (`models.json`, `models.yml`).
fn pi_model(entry: &Map<String, Value>) -> ModelOverride {
    let whole = |key: &str| entry.get(key).and_then(Value::as_u64).filter(|n| *n > 0);
    let context_window = whole("contextWindow");
    // A model may not be allowed more output than its window holds.
    let max_output = whole("maxTokens")
        .map(|max| context_window.map_or(max, |window| max.min(window)))
        .and_then(|max| u32::try_from(max).ok());
    let thinking_levels: Vec<ThinkingLevel> = entry
        .get("thinking")
        .and_then(|thinking| thinking.get("efforts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|effort| effort.as_str()?.parse().ok())
        .collect();
    let price = |cost: &Value, key: &str| cost.get(key).and_then(Value::as_f64);
    let cost = entry.get("cost").and_then(|cost| {
        Some(Cost {
            input: price(cost, "input")?,
            output: price(cost, "output")?,
            cache_read: price(cost, "cacheRead").unwrap_or(0.0),
            cache_write: price(cost, "cacheWrite").unwrap_or(0.0),
        })
    });
    let inputs = entry.get("input").and_then(Value::as_array);
    ModelOverride {
        name: text_of(entry, "name").map(str::to_string),
        context_window,
        max_output,
        vision: inputs.map(|inputs| inputs.iter().any(|input| input.as_str() == Some("image"))),
        reasoning: entry.get("reasoning").and_then(Value::as_bool),
        thinking_levels: (!thinking_levels.is_empty()).then_some(thinking_levels),
        cost,
    }
}

/// The providers of a pi `models.json` or an omp `models.yml` (`root` is the parsed file), added
/// to `found` with their keys. `env_names` says how a key that looks like a variable name is
/// read: pi writes those as `$NAME`, omp as the bare name.
pub fn pi_providers(found: &mut Found, root: &Value, file: &Path, bare_env_names: bool) -> Providers {
    let mut known = Providers::default();
    let providers = root.get("providers").and_then(Value::as_object);
    for (source_id, entry) in providers.into_iter().flatten() {
        let Some(entry) = entry.as_object() else { continue };
        let id = provider_id(source_id);
        let what = format!("provider {source_id}");
        let builtin = presets::is_builtin(&id);
        let own_api = text_of(entry, "api");
        let api = own_api.and_then(wire_api);
        if let (Some(name), None) = (own_api, api) {
            found.lossy(
                Kind::Models,
                format!("{what}: azazel does not speak `{name}`; the provider is left out"),
            );
            continue;
        }
        let base_url = text_of(entry, "baseUrl").map(str::to_string);
        if !builtin && (api.is_none() || base_url.is_none()) {
            found.lossy(
                Kind::Models,
                format!("{what}: it names no `api` and `baseUrl`, and azazel has no provider of that name"),
            );
            continue;
        }
        let mut settings = ProviderSettings {
            name: text_of(entry, "name").map(str::to_string),
            api,
            base_url,
            headers: string_map(entry.get("headers")),
            ..ProviderSettings::default()
        };
        if entry.get("authHeader").and_then(Value::as_bool) == Some(true) {
            settings.auth = Some(AuthStyle::Bearer);
        }
        let mut key = None;
        match (text_of(entry, "auth"), text_of(entry, "apiKey")) {
            (Some("oauth"), _) => found.lossy(
                Kind::Models,
                format!("{what}: it signs in with OAuth, which is not carried over"),
            ),
            (Some("none"), _) | (_, None) => {}
            (_, Some(value)) => {
                let variable = |name: &str| {
                    name.len() > 2
                        && name.starts_with(|c: char| c.is_ascii_uppercase())
                        && name
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                };
                if let Some(command) = value.strip_prefix('!') {
                    settings.api_key_command = Some(command.trim().to_string());
                } else if let Some(name) = value.strip_prefix('$').filter(|name| variable(name)) {
                    settings.api_key_env = Some(name.to_string());
                } else if bare_env_names && variable(value) {
                    settings.api_key_env = Some(value.to_string());
                } else {
                    key = Some(Secret::new(value));
                }
            }
        }
        for dropped in [
            "compat",
            "modelOverrides",
            "discovery",
            "transport",
            "remoteCompaction",
        ] {
            if entry.contains_key(dropped) {
                found.lossy(Kind::Models, format!("{what}: `{dropped}` has no equivalent"));
            }
        }

        // Models by the wire format each one speaks: azazel's providers speak one.
        let mut groups: Vec<(Option<WireApi>, Models)> = Vec::new();
        let models = entry.get("models").and_then(Value::as_array);
        for model in models.into_iter().flatten() {
            let Some(model) = model.as_object() else { continue };
            let Some(model_id) = text_of(model, "id") else {
                continue;
            };
            let model_api = match text_of(model, "api") {
                Some(name) => match wire_api(name) {
                    Some(api) => Some(api),
                    None => {
                        found.lossy(
                            Kind::Models,
                            format!("{what}: model {model_id} speaks `{name}`, which azazel does not; it is left out"),
                        );
                        continue;
                    }
                },
                None => api,
            };
            let row = (model_id.to_string(), pi_model(model));
            match groups.iter_mut().find(|(api, _)| *api == model_api) {
                Some((_, rows)) => rows.push(row),
                None => groups.push((model_api, vec![row])),
            }
        }
        if groups.is_empty() {
            groups.push((api, Vec::new()));
        }
        // The provider keeps its id for the models that speak its own format.
        groups.sort_by_key(|(group_api, _)| *group_api != api);
        for (index, (group_api, models)) in groups.into_iter().enumerate() {
            let group_id = match (index, group_api) {
                (0, _) | (_, None) => id.clone(),
                (_, Some(other)) => {
                    let moved = format!("{id}-{}", api_suffix(other));
                    for (model, _) in &models {
                        known
                            .moved
                            .insert((source_id.clone(), model.clone()), moved.clone());
                    }
                    found.lossy(
                        Kind::Models,
                        format!(
                            "{what}: its {} models that speak `{}` become the provider `{moved}` (an azazel provider \
                             speaks one wire format)",
                            models.len(),
                            api_name(other)
                        ),
                    );
                    moved
                }
            };
            let mut group = settings.clone();
            if index > 0 {
                group.api = group_api;
                // A copy of a built-in provider is a new one, and needs its address spelled out.
                if group.base_url.is_none() {
                    found.lossy(
                        Kind::Models,
                        format!("{what}: `{group_id}` is left out: it would need the address of `{id}`"),
                    );
                    continue;
                }
            }
            if let Some(key) = &key {
                found.keys.push(Key {
                    provider: group_id.clone(),
                    key: key.clone(),
                    origin: format!("{} ({what})", file_name(file)),
                });
            }
            known.add_custom(&group_id);
            found.providers.push(Provider {
                id: group_id,
                settings: group,
                models,
            });
        }
    }
    known
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- Markdown resources

/// The Markdown files below `dir`, deepest last; `recurse` looks in its directories too.
pub fn markdown_files(dir: &Path, recurse: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![(dir.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                if recurse && depth < 6 {
                    pending.push((path, depth + 1));
                }
            } else if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("mdc"))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The skill directories below `root`: every directory that holds a `SKILL.md`, down to four
/// levels (pi nests them); nothing below a skill is looked at.
pub fn skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.flatten().map(|entry| entry.path()) {
            if !path.is_dir() {
                continue;
            }
            if path.join("SKILL.md").is_file() {
                out.push(path);
            } else if depth < 3 {
                pending.push((path, depth + 1));
            }
        }
    }
    out.sort();
    out
}

/// Adds the skills below `root` to `found`, each copied whole under `skills/<directory>`.
pub fn add_skills(found: &mut Found, root: &Path, scope: &Scope) {
    for dir in skill_dirs(root) {
        let Some(name) = dir.file_name() else { continue };
        let mut notes = Vec::new();
        let file = dir.join("SKILL.md");
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                if let Err(why) =
                    azazel_core::skills::parse_skill(&file, &text, azazel_core::SkillSource::Home)
                {
                    notes.push(format!("azazel will not load it as it is: {why}"));
                }
            }
            Err(e) => notes.push(format!("SKILL.md cannot be read: {e}")),
        }
        found.files.push(FileItem {
            kind: Kind::Skills,
            scope: scope.clone(),
            to: Path::new("skills").join(name),
            from: dir,
            body: Body::Copy,
            notes,
        });
    }
}

/// A frontmatter block and body as one file.
pub fn with_frontmatter(meta: &Map<String, Value>, body: &str) -> String {
    if meta.is_empty() {
        return body.to_string();
    }
    let yaml = serde_yaml_ng::to_string(meta).unwrap_or_default();
    format!("---\n{}---\n{}", yaml, body.strip_prefix('\n').unwrap_or(body))
}

/// `key: value` lines as strings, for a frontmatter block that is not YAML.
fn loose_frontmatter(yaml: &str) -> Map<String, Value> {
    yaml.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .filter(|(key, value)| !key.is_empty() && !key.starts_with('#') && !value.is_empty())
        .map(|(key, value)| {
            let unquoted = ['"', '\'']
                .iter()
                .find_map(|quote| value.strip_prefix(*quote)?.strip_suffix(*quote))
                .unwrap_or(value);
            (key.to_string(), Value::String(unquoted.to_string()))
        })
        .collect()
}

/// The model of a command or an agent as azazel names it: Claude Code's aliases become roles,
/// a selector is translated, and anything else is dropped with a note.
fn resource_model(value: &Value, providers: &Providers, notes: &mut Vec<String>) -> Option<Value> {
    let mut chain = Vec::new();
    for selector in selectors(value) {
        let mapped = azazel_core::agents::compat_model(&selector);
        if mapped != selector {
            chain.push(mapped.to_string());
            continue;
        }
        if selector == "inherit" {
            continue;
        }
        match providers.spec(&selector) {
            Ok(spec) => chain.push(spec),
            Err(why) => notes.push(format!("model `{selector}` dropped: {why}")),
        }
    }
    match chain.len() {
        0 => None,
        1 => Some(Value::String(chain.remove(0))),
        _ => Some(Value::Array(chain.into_iter().map(Value::String).collect())),
    }
}

/// The placeholders of a command's body as azazel fills them in. What azazel has no form for
/// stays as written and is named in `notes`.
pub fn command_body(body: &str, source: Source, notes: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut unsupported: Vec<String> = Vec::new();
    let mut note = |what: String| {
        if !unsupported.contains(&what) {
            unsupported.push(what);
        }
    };
    let mut rest = body;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        // `$$` is a literal dollar in Codex prompts.
        if source == Source::Codex && after.starts_with('$') {
            out.push('$');
            rest = &after[1..];
            continue;
        }
        if let Some(tail) = after.strip_prefix('@') {
            // `$@[2]`, `$@[2:3]`: slices of the arguments.
            if tail.starts_with('[') {
                let end = tail.find(']').map_or(tail.len(), |end| end + 1);
                note(format!("$@{}", &tail[..end]));
                out.push_str("$@");
                out.push_str(&tail[..end]);
                rest = &tail[end..];
            } else {
                out.push_str("$ARGUMENTS");
                rest = tail;
            }
            continue;
        }
        if let Some(tail) = after.strip_prefix("{@") {
            let end = tail.find('}').map_or(tail.len(), |end| end + 1);
            note(format!("${{@{}", &tail[..end]));
            out.push_str("${@");
            out.push_str(&tail[..end]);
            rest = &tail[end..];
            continue;
        }
        if let Some(tail) = after.strip_prefix("ARGUMENTS[") {
            // Claude Code counts the arguments of `$ARGUMENTS[n]` from zero.
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            match (digits.parse::<usize>(), tail[digits.len()..].strip_prefix(']')) {
                (Ok(index), Some(tail)) if index < 9 => {
                    out.push_str(&format!("${}", index + 1));
                    rest = tail;
                    continue;
                }
                _ => {}
            }
        }
        // Codex fills `$NAME` from `NAME=value` among the arguments.
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        let named =
            name.len() > 1 && name.starts_with(|c: char| c.is_ascii_uppercase()) && name != "ARGUMENTS";
        if named && source == Source::Codex {
            note(format!("${name}"));
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    if !unsupported.is_empty() {
        notes.push(format!(
            "{} left as written: azazel fills in $ARGUMENTS, $1 to $9 and ${{N:-default}} only",
            unsupported.join(", ")
        ));
    }
    out
}

/// Adds the commands (prompt templates) below `dir` to `found`. A command's name is its path
/// below `dir`, as in azazel.
pub fn add_commands(found: &mut Found, dir: &Path, scope: &Scope, recurse: bool, providers: &Providers) {
    let source = found.source;
    for file in markdown_files(dir, recurse) {
        let Some(text) = read_text(found, &file) else {
            continue;
        };
        let Ok(relative) = file.strip_prefix(dir) else {
            continue;
        };
        let mut notes = Vec::new();
        let (meta, body) = match frontmatter::split(&text) {
            // The harnesses read this block more loosely than YAML allows (Codex's own example
            // of an `argument-hint` is no YAML), so a block that is none is read line by line.
            Some((yaml, body)) => (
                frontmatter::parse(yaml).unwrap_or_else(|_| loose_frontmatter(yaml)),
                body,
            ),
            None => (Map::new(), text.as_str()),
        };
        let mut kept = Map::new();
        let mut dropped = Vec::new();
        for (key, value) in &meta {
            match key.as_str() {
                "description" => {
                    kept.insert(key.clone(), value.clone());
                }
                "argument-hint" | "argumentHint" => {
                    kept.insert("argument-hint".to_string(), value.clone());
                }
                "model" => {
                    if let Some(model) = resource_model(value, providers, &mut notes) {
                        kept.insert(key.clone(), model);
                    }
                }
                other => dropped.push(other.to_string()),
            }
        }
        if !dropped.is_empty() {
            notes.push(format!("frontmatter dropped: {}", dropped.join(", ")));
        }
        let body = command_body(body, source, &mut notes);
        found.files.push(FileItem {
            kind: Kind::Commands,
            scope: scope.clone(),
            to: Path::new("commands").join(relative).with_extension("md"),
            from: file,
            body: Body::Text(with_frontmatter(&kept, &body)),
            notes,
        });
    }
}

/// A tool list of an agent definition (a comma-separated string or a list) with the names of
/// Claude Code's tools translated.
fn tool_list(value: &Value) -> Value {
    let names = frontmatter::string_or_list(Some(value)).unwrap_or_default();
    let mapped: Vec<Value> = names
        .iter()
        .map(|entry| {
            let (name, pattern) = match entry.split_once('(') {
                Some((name, pattern)) => (name.trim(), Some(pattern)),
                None => (entry.trim(), None),
            };
            // Claude Code's names, and the two tools pi and omp call otherwise.
            let name = match azazel_core::agents::compat_tool(name) {
                "bash" => "shell",
                "find" => "glob",
                other => other,
            };
            Value::String(match pattern {
                Some(pattern) => format!("{name}({pattern}"),
                None => name.to_string(),
            })
        })
        .collect();
    Value::Array(mapped)
}

/// Adds the agent definitions below `dir` to `found`, their frontmatter mapped to azazel's keys.
/// Keys azazel has nothing for are dropped and named.
pub fn add_agents(found: &mut Found, dir: &Path, scope: &Scope, providers: &Providers) {
    for file in markdown_files(dir, true) {
        let Some(text) = read_text(found, &file) else {
            continue;
        };
        let Ok(relative) = file.strip_prefix(dir) else {
            continue;
        };
        let Some((yaml, body)) = frontmatter::split(&text) else {
            found.lossy(
                Kind::Agents,
                format!("{}: no frontmatter, so it is no agent definition", file.display()),
            );
            continue;
        };
        let meta = match frontmatter::parse(yaml) {
            Ok(meta) => meta,
            Err(why) => {
                found.lossy(Kind::Agents, format!("{}: {why}", file.display()));
                continue;
            }
        };
        let mut notes = Vec::new();
        let mut kept = Map::new();
        let mut dropped = Vec::new();
        for (key, value) in &meta {
            match key.as_str() {
                "name" | "description" | "skills" | "spawns" => {
                    kept.insert(key.clone(), value.clone());
                }
                "tools" => {
                    // `*` is every tool, which is what leaving the key out means.
                    if value.as_str().map(str::trim) != Some("*") {
                        kept.insert(key.clone(), tool_list(value));
                    }
                }
                "disallowedTools" | "disallowed-tools" => {
                    kept.insert("disallowed-tools".to_string(), tool_list(value));
                }
                "model" => {
                    if let Some(model) = resource_model(value, providers, &mut notes) {
                        kept.insert(key.clone(), model);
                    }
                }
                "thinking" | "thinkingLevel" | "thinking-level" | "effort" => {
                    match value
                        .as_str()
                        .and_then(|level| level.parse::<ThinkingLevel>().ok())
                    {
                        Some(level) => {
                            kept.insert("thinking".to_string(), Value::String(level.to_string()));
                        }
                        None => dropped.push(key.clone()),
                    }
                }
                "maxTurns" | "max-turns" => {
                    kept.insert("max-turns".to_string(), value.clone());
                }
                "permissionMode" | "mode" => match value.as_str() {
                    Some("plan") => {
                        kept.insert("mode".to_string(), Value::String("plan".to_string()));
                    }
                    Some("inherit" | "default") | None => {}
                    Some(other) => notes.push(format!(
                        "permission mode `{other}` dropped: an agent follows the session's mode"
                    )),
                },
                "isolation" => match value.as_str() {
                    Some("worktree") => {
                        kept.insert(key.clone(), value.clone());
                    }
                    _ => dropped.push(key.clone()),
                },
                "output" | "outputSchema" | "output-schema" if value.is_object() => {
                    kept.insert("output-schema".to_string(), value.clone());
                }
                other => dropped.push(other.to_string()),
            }
        }
        if !dropped.is_empty() {
            notes.push(format!("frontmatter dropped: {}", dropped.join(", ")));
        }
        let text = with_frontmatter(&kept, body);
        if let Err(why) = azazel_core::agents::parse_def(&file, &text, azazel_core::agents::DefSource::Home) {
            found.lossy(
                Kind::Agents,
                format!("{}: not a definition azazel can use: {why}", file.display()),
            );
            continue;
        }
        found.files.push(FileItem {
            kind: Kind::Agents,
            scope: scope.clone(),
            to: Path::new("agents").join(relative).with_extension("md"),
            from: file,
            body: Body::Text(text),
            notes,
        });
    }
}

/// `text` with the lines that only name a file (`@path`) replaced by that file, as Claude Code
/// and omp read their context files. A copy no longer sits beside the files it names, so they
/// come along. Returns how many includes could not be followed.
fn inline_includes(
    text: &str,
    dir: &Path,
    home: &Path,
    depth: usize,
    seen: &mut Vec<PathBuf>,
) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut missing = 0;
    let mut fenced = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        }
        let named = trimmed
            .strip_prefix('@')
            .filter(|path| !fenced && !path.is_empty() && !path.contains(char::is_whitespace));
        let Some(named) = named else {
            out.push_str(line);
            continue;
        };
        let path = match named.strip_prefix("~/") {
            Some(below_home) => home.join(below_home),
            None => dir.join(named),
        };
        let included = (depth > 0 && !seen.contains(&path))
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten();
        match included {
            Some(included) => {
                seen.push(path.clone());
                let (included, below) =
                    inline_includes(&included, path.parent().unwrap_or(dir), home, depth - 1, seen);
                missing += below;
                out.push_str(included.trim_end());
                out.push('\n');
            }
            // `@word` that names no file is a mention, not an include.
            None if !path.exists() && !named.contains(['/', '.']) => out.push_str(line),
            None => {
                missing += 1;
                out.push_str(line);
            }
        }
    }
    (out, missing)
}

/// Adds a context file (an `AGENTS.md`, a `CLAUDE.md`) that azazel does not read where it is,
/// as a rule that always applies. `name` is the file's name below `rules/`.
pub fn add_context_file(found: &mut Found, file: &Path, scope: &Scope, name: &str, env: &Env) {
    let Some(text) = read_text(found, file) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    let mut notes = Vec::new();
    let mut seen = vec![file.to_path_buf()];
    let dir = file.parent().unwrap_or(Path::new(""));
    let (text, missing) = inline_includes(&text, dir, &env.home, 4, &mut seen);
    if seen.len() > 1 {
        notes.push(format!("{} included files are written into it", seen.len() - 1));
    }
    if missing > 0 {
        notes.push(format!("{missing} `@path` includes could not be followed"));
    }
    let body = match frontmatter::split(&text) {
        // It is a rule already; whatever it says about when it applies stays.
        Some(_) => text,
        None => {
            let mut meta = Map::new();
            meta.insert(
                "description".to_string(),
                Value::String(format!(
                    "Instructions from {} ({})",
                    found.source.label(),
                    file_name(file)
                )),
            );
            meta.insert("always".to_string(), Value::Bool(true));
            with_frontmatter(&meta, &text)
        }
    };
    found.files.push(FileItem {
        kind: Kind::Rules,
        scope: scope.clone(),
        from: file.to_path_buf(),
        to: Path::new("rules").join(name),
        body: Body::Text(body),
        notes,
    });
}

/// Adds the rule files below `dir` to `found` under `rules/<prefix>/`. Azazel reads `paths`
/// (or `globs`) and `always` (or `alwaysApply`) as the sources write them.
pub fn add_rules(found: &mut Found, dir: &Path, scope: &Scope, prefix: &str) {
    for file in markdown_files(dir, true) {
        let Ok(relative) = file.strip_prefix(dir) else {
            continue;
        };
        found.files.push(FileItem {
            kind: Kind::Rules,
            scope: scope.clone(),
            to: Path::new("rules")
                .join(prefix)
                .join(relative)
                .with_extension("md"),
            from: file,
            body: Body::Copy,
            notes: Vec::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found() -> Found {
        Found::new(Source::Omp, Path::new("/source"))
    }

    #[test]
    fn a_selector_keeps_its_effort_and_takes_azazels_provider_name() {
        let providers = Providers::default();
        assert_eq!(
            providers.spec("anthropic/claude-x:high").unwrap(),
            "anthropic/claude-x:high"
        );
        assert_eq!(providers.spec("lm-studio/qwen:auto").unwrap(), "lmstudio/qwen");
        // A colon in the model id is no effort.
        assert_eq!(
            providers.spec("openrouter/deepseek/v4:free").unwrap(),
            "openrouter/deepseek/v4:free"
        );
        assert_eq!(providers.spec("@smol").unwrap(), "@smol");
        let unknown = providers.spec("nowhere/model").unwrap_err();
        assert!(unknown.contains("no provider `nowhere`"), "{unknown}");
    }

    #[test]
    fn a_provider_whose_models_speak_two_formats_becomes_two_providers() {
        let root: Value = serde_json::json!({ "providers": { "gate": {
            "baseUrl": "https://gate.example/v1",
            "api": "openai-completions",
            "apiKey": "literal-key-value",
            "models": [
                { "id": "chatty", "contextWindow": 1000, "maxTokens": 4000, "input": ["text", "image"] },
                { "id": "responsive", "api": "openai-responses", "reasoning": true,
                  "thinking": { "mode": "effort", "efforts": ["low", "high", "ludicrous"] },
                  "cost": { "input": 1.5, "output": 6, "cacheRead": 0.1 } },
            ],
        } } });
        let mut found = found();
        let providers = pi_providers(&mut found, &root, Path::new("models.yml"), true);

        let ids: Vec<&str> = found.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["gate", "gate-responses"]);
        assert_eq!(found.providers[0].settings.api, Some(WireApi::OpenAiChat));
        assert_eq!(found.providers[1].settings.api, Some(WireApi::OpenAiResponses));
        assert_eq!(
            found.providers[1].settings.base_url.as_deref(),
            Some("https://gate.example/v1")
        );
        let (_, chatty) = &found.providers[0].models[0];
        // The output limit is held to the window, which azazel requires.
        assert_eq!(
            (chatty.context_window, chatty.max_output, chatty.vision),
            (Some(1000), Some(1000), Some(true))
        );
        let (_, responsive) = &found.providers[1].models[0];
        assert_eq!(
            responsive.thinking_levels,
            Some(vec![ThinkingLevel::Low, ThinkingLevel::High])
        );
        assert_eq!(
            responsive.cost.map(|cost| (cost.input, cost.cache_read)),
            Some((1.5, 0.1))
        );

        // The key goes to both, and never into the provider's settings.
        let keyed: Vec<&str> = found.keys.iter().map(|key| key.provider.as_str()).collect();
        assert_eq!(keyed, ["gate", "gate-responses"]);
        assert!(found.providers.iter().all(|p| p.settings.api_key.is_none()));
        // A role that names the moved model follows it.
        assert_eq!(
            providers.spec("gate/responsive:low").unwrap(),
            "gate-responses/responsive:low"
        );
        assert_eq!(providers.spec("gate/chatty").unwrap(), "gate/chatty");
    }

    #[test]
    fn a_key_that_names_a_variable_or_a_command_is_not_taken_for_a_key() {
        let root: Value = serde_json::json!({ "providers": {
            "a": { "baseUrl": "https://a.example", "api": "anthropic-messages", "apiKey": "A_GATEWAY_KEY" },
            "b": { "baseUrl": "https://b.example", "api": "openai-responses", "apiKey": "!pass show b" },
            "c": { "baseUrl": "https://c.example", "api": "mistral-conversations" },
        } });
        let mut omp = found();
        pi_providers(&mut omp, &root, Path::new("models.yml"), true);
        assert!(omp.keys.is_empty());
        assert_eq!(
            omp.providers[0].settings.api_key_env.as_deref(),
            Some("A_GATEWAY_KEY")
        );
        assert_eq!(
            omp.providers[1].settings.api_key_command.as_deref(),
            Some("pass show b")
        );
        assert_eq!(
            omp.providers.len(),
            2,
            "a wire format azazel lacks leaves the provider out"
        );
        assert!(
            omp.lossy
                .iter()
                .any(|note| note.text.contains("mistral-conversations"))
        );

        // pi writes a variable as `$NAME`; a bare upper-case word is the key itself there.
        let mut pi = Found::new(Source::Pi, Path::new("/source"));
        pi_providers(&mut pi, &root, Path::new("models.json"), false);
        assert_eq!(pi.keys.len(), 1);
        assert_eq!(pi.providers[0].settings.api_key_env, None);
    }

    #[test]
    fn command_placeholders_become_azazels() {
        let mut notes = Vec::new();
        assert_eq!(
            command_body("Review $@ then $1 and ${2:-main}", Source::Pi, &mut notes),
            "Review $ARGUMENTS then $1 and ${2:-main}"
        );
        assert!(notes.is_empty());

        let sliced = command_body("Take ${@:2} and $@[1:2]", Source::Omp, &mut notes);
        assert_eq!(sliced, "Take ${@:2} and $@[1:2]");
        assert!(
            notes[0].contains("${@:2}") && notes[0].contains("$@[1:2]"),
            "{notes:?}"
        );

        let mut notes = Vec::new();
        assert_eq!(
            command_body(
                "Costs $$5: stage $FILES for $ARGUMENTS",
                Source::Codex,
                &mut notes
            ),
            "Costs $5: stage $FILES for $ARGUMENTS"
        );
        assert!(notes[0].starts_with("$FILES left as written"), "{notes:?}");

        let mut notes = Vec::new();
        assert_eq!(
            command_body(
                "First $ARGUMENTS[0], third $ARGUMENTS[2]",
                Source::Claude,
                &mut notes
            ),
            "First $1, third $3"
        );
    }

    #[test]
    fn times_are_read_from_text_seconds_and_milliseconds() {
        let text = serde_json::json!("2026-02-16T10:20:30.000Z");
        let millis = serde_json::json!(1771237230000u64);
        let seconds = serde_json::json!(1771237230u64);
        let expected: jiff::Timestamp = "2026-02-16T10:20:30Z".parse().unwrap();
        assert_eq!(time_of(Some(&text)), Some(expected));
        assert_eq!(time_of(Some(&millis)), Some(expected));
        assert_eq!(time_of(Some(&seconds)), Some(expected));
        assert_eq!(time_of(Some(&Value::Null)), None);
    }

    #[test]
    fn environment_files_and_addresses_are_read_plainly() {
        let lines = env_lines("# keys\nexport A_KEY=\"one two\"\nB_KEY=three # trailing\nbad line\nC=\n");
        assert_eq!(
            lines,
            [
                ("A_KEY".to_string(), "one two".to_string()),
                ("B_KEY".to_string(), "three".to_string())
            ]
        );
        assert_eq!(redact_email("jane.doe@example.com"), "j***@example.com");
        assert_eq!(
            provider_of_variable("ANTHROPIC_API_KEY").as_deref(),
            Some("anthropic")
        );
        assert_eq!(provider_of_variable("NO_SUCH_KEY"), None);
    }

    #[test]
    fn a_context_file_becomes_a_rule_that_always_applies_with_its_includes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("style.md"), "Use tabs.\n").unwrap();
        let file = dir.path().join("CLAUDE.md");
        std::fs::write(
            &file,
            "# Mine\n@style.md\nAsk @alice.\n```\n@style.md\n```\n@gone.md\n",
        )
        .unwrap();
        let mut found = Found::new(Source::Claude, dir.path());
        add_context_file(&mut found, &file, &Scope::Home, "claude.md", &Env::at(dir.path()));

        let item = &found.files[0];
        assert_eq!(item.to, Path::new("rules").join("claude.md"));
        let Body::Text(text) = &item.body else {
            panic!("a converted file")
        };
        let (yaml, body) = frontmatter::split(text).unwrap();
        assert_eq!(frontmatter::parse(yaml).unwrap()["always"], Value::Bool(true));
        assert_eq!(
            body,
            "# Mine\nUse tabs.\nAsk @alice.\n```\n@style.md\n```\n@gone.md\n"
        );
        assert!(
            item.notes.iter().any(|note| note.contains("1 included")),
            "{:?}",
            item.notes
        );
        assert!(
            item.notes.iter().any(|note| note.contains("1 `@path`")),
            "{:?}",
            item.notes
        );
    }
}
