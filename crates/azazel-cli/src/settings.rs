//! `/settings`: signing in with a subscription, storing API keys, choosing the default model,
//! signing out and changing the everyday settings, all from inside the terminal UI, so that
//! neither a first run nor a change of taste needs config editing.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::login::{forget_key, sign_in_from_ui, store_key};
use async_trait::async_trait;
use azazel_ai::catalog::Cost;
use azazel_ai::{Credentials, ModelSpec, ProviderConfig, Router};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Config, ExtCtx, ExtError, Extension, Level, Registrar, Ui,
    UiError,
};

/// How long a provider may take to list its models before the built-in catalogue is used.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// What a subscription login is called, for the providers that have one built in.
fn subscription_name(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("Claude Pro or Max"),
        "openai-codex" => Some("ChatGPT Plus or Pro"),
        _ => None,
    }
}

/// The providers a person can sign in to with an account.
fn login_providers(providers: &[ProviderConfig]) -> Vec<&ProviderConfig> {
    providers.iter().filter(|p| p.oauth.is_some()).collect()
}

/// The providers that take an API key: they name the variable the key would come from.
fn key_providers(providers: &[ProviderConfig]) -> Vec<&ProviderConfig> {
    providers
        .iter()
        .filter(|p| p.requires_key && !p.api_key_env.is_empty())
        .collect()
}

/// The chain as written in config (`["anthropic/claude-x", "@smol"]`), for showing.
fn chain_label(chain: &[ModelSpec]) -> String {
    chain
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a model picker says beside a model: its context window and what a million tokens in
/// and out cost, or `subscription` when the provider is used through a login.
pub(crate) fn model_detail(context_window: u64, cost: Option<&Cost>, login: bool) -> String {
    let window = match context_window {
        n if n >= 1_000_000 && n % 100_000 == 0 => format!("{}M", trim_number(n as f64 / 1e6)),
        n if n >= 1000 => format!("{}k", n / 1000),
        n => n.to_string(),
    };
    match (login, cost) {
        (true, _) => format!("ctx {window} · subscription"),
        (false, Some(cost)) => format!(
            "ctx {window} · ${}/${}",
            trim_number(cost.input),
            trim_number(cost.output)
        ),
        (false, None) => format!("ctx {window}"),
    }
}

/// A number without the zeros a fixed precision would leave: `3`, `1.25`, `0.075`.
fn trim_number(number: f64) -> String {
    let fixed = format!("{number:.3}");
    fixed.trim_end_matches('0').trim_end_matches('.').to_string()
}

struct Settings {
    providers: Vec<ProviderConfig>,
    credentials: Arc<Credentials>,
    home_config: PathBuf,
    /// The configuration this run started with, as a table, for showing values the user's
    /// config file does not set.
    effective: toml::Table,
}

/// The menu rows, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Subscription,
    ApiKey,
    DefaultModel,
    Roles,
    /// One group of the settings in [`SETTINGS`].
    Section(&'static str),
    SignOut,
    Done,
}

/// How the settings menu reaches the running UI: called with the path and the new value of a
/// setting that applies at once. The terminal UI registers it as a service.
pub struct LiveSettings(pub Arc<ApplySetting>);

/// Called with a setting's path (`editor.mode`) and its new value.
pub type ApplySetting = dyn Fn(&str, &toml::Value) + Send + Sync;

/// How a setting is edited.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// On or off; choosing the row flips it.
    Switch,
    /// One of these values.
    Choice(&'static [&'static str]),
    /// A number, whole or with a fraction as the setting's default is.
    Number,
    Text,
    /// Several values, typed with commas between them.
    List,
}

/// One row of a settings section.
struct Setting {
    section: &'static str,
    /// Where it lives in the config file, as `table.key`.
    path: &'static str,
    label: &'static str,
    kind: Kind,
    /// Applies to the running UI at once; the others take effect in the next session.
    live: bool,
}

const fn setting(
    section: &'static str,
    path: &'static str,
    label: &'static str,
    kind: Kind,
    live: bool,
) -> Setting {
    Setting {
        section,
        path,
        label,
        kind,
        live,
    }
}

/// Every setting the menu can change, section by section. Each is saved in the user's
/// `config.toml`; what a project's own config sets on top still wins in that project.
const SETTINGS: &[Setting] = &[
    setting(
        "Appearance",
        "tui.theme",
        "Theme (dark, light or a file in ~/.azazel/themes)",
        Kind::Text,
        true,
    ),
    setting(
        "Appearance",
        "tui.images",
        "Inline images",
        Kind::Choice(&["auto", "off", "kitty", "iterm", "sixel"]),
        false,
    ),
    setting(
        "Appearance",
        "tui.show_thinking",
        "Show the model's thinking",
        Kind::Switch,
        true,
    ),
    setting(
        "Appearance",
        "tui.expand_tools",
        "Show tool output in full",
        Kind::Switch,
        true,
    ),
    setting(
        "Editor",
        "editor.mode",
        "Keys in the input",
        Kind::Choice(&["emacs", "vim"]),
        true,
    ),
    setting(
        "Editor",
        "editor.command",
        "External editor command (empty uses $VISUAL, then $EDITOR)",
        Kind::Text,
        true,
    ),
    setting(
        "Editor",
        "editor.fold_paste_lines",
        "Fold pastes longer than this many lines (0 never folds)",
        Kind::Number,
        true,
    ),
    setting(
        "Permissions",
        "permissions.mode",
        "Mode a session starts in",
        Kind::Choice(&["plan", "ask", "accept-edits", "auto"]),
        false,
    ),
    setting(
        "Permissions",
        "permissions.destructive",
        "Commands that destroy work",
        Kind::Choice(&["ask", "allow", "deny"]),
        false,
    ),
    setting(
        "Notifications",
        "notify.bell",
        "Ring the terminal bell",
        Kind::Switch,
        false,
    ),
    setting(
        "Notifications",
        "notify.desktop",
        "Desktop notifications",
        Kind::Choice(&["auto", "terminal", "system", "off"]),
        false,
    ),
    setting(
        "Notifications",
        "notify.title",
        "Show the session in the terminal title",
        Kind::Switch,
        false,
    ),
    setting(
        "Notifications",
        "notify.progress",
        "Show progress in the terminal tab",
        Kind::Switch,
        false,
    ),
    setting(
        "Notifications",
        "notify.min_secs",
        "Notify about runs that took at least this many seconds",
        Kind::Number,
        false,
    ),
    setting(
        "Notifications",
        "notify.keep_awake",
        "Keep the computer awake while agents work",
        Kind::Switch,
        false,
    ),
    setting(
        "Limits",
        "limits.max_cost_usd",
        "Stop a run at this cost in USD (0 is no cap)",
        Kind::Number,
        false,
    ),
    setting(
        "Limits",
        "limits.warn_cost_usd",
        "Warn at this cost in USD (0 is no warning)",
        Kind::Number,
        false,
    ),
    setting(
        "Limits",
        "limits.max_turns",
        "Requests per prompt for the lead (0 is no limit)",
        Kind::Number,
        false,
    ),
    setting(
        "Sessions",
        "session.auto_name",
        "Name sessions automatically",
        Kind::Switch,
        false,
    ),
    setting(
        "Sessions",
        "session.restore_model",
        "Resume on the model and thinking level last used",
        Kind::Switch,
        false,
    ),
    setting(
        "Sessions",
        "session.retention_days",
        "Delete sessions unused for this many days (0 keeps all)",
        Kind::Number,
        false,
    ),
    setting("Memory", "memory.enabled", "Memory", Kind::Switch, false),
    setting(
        "Memory",
        "memory.auto_extract",
        "Save memories from conversations",
        Kind::Switch,
        false,
    ),
    setting(
        "Memory",
        "memory.auto_index",
        "Index the project for search",
        Kind::Switch,
        false,
    ),
    setting(
        "Updates",
        "update.check",
        "Look for a newer release once a day",
        Kind::Switch,
        false,
    ),
    setting(
        "Updates",
        "update.auto",
        "Install a newer release when the UI exits",
        Kind::Switch,
        false,
    ),
    setting(
        "Voice",
        "voice.stt",
        "Transcription model (provider/model)",
        Kind::Text,
        false,
    ),
    setting(
        "Voice",
        "voice.tts",
        "Speech model (provider/model)",
        Kind::Text,
        false,
    ),
    setting("Voice", "voice.voice", "Voice", Kind::Text, false),
    setting(
        "Voice",
        "voice.auto_send",
        "Send what was dictated at once",
        Kind::Switch,
        false,
    ),
    setting(
        "Voice",
        "voice.max_secs",
        "Longest recording in seconds",
        Kind::Number,
        false,
    ),
    setting(
        "Team",
        "team.coordinator",
        "Coordinator mode: the lead only plans and delegates",
        Kind::Switch,
        false,
    ),
    setting(
        "Team",
        "team.max_parallel",
        "Agents working at the same time",
        Kind::Number,
        false,
    ),
    setting(
        "Team",
        "advisor.when",
        "Second-model advisor",
        Kind::Choice(&["off", "stop"]),
        false,
    ),
    setting(
        "Team",
        "advisor.model",
        "Advisor model (several with commas: first that answers)",
        Kind::List,
        false,
    ),
];

/// The sections of [`SETTINGS`], in the order they first appear.
fn sections() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for setting in SETTINGS {
        if !out.contains(&setting.section) {
            out.push(setting.section);
        }
    }
    out
}

/// The value at `table.key` in a configuration table.
fn lookup<'a>(table: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
    let mut parts = path.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.get(part)?;
    }
    Some(value)
}

/// A value as a menu row shows it.
fn shown(value: Option<&toml::Value>) -> String {
    match value {
        Some(toml::Value::Boolean(true)) => "on".to_string(),
        Some(toml::Value::Boolean(false)) => "off".to_string(),
        Some(toml::Value::String(text)) if text.is_empty() => "(not set)".to_string(),
        Some(toml::Value::String(text)) => text.clone(),
        Some(toml::Value::Array(items)) if items.is_empty() => "(none)".to_string(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map_or_else(|| item.to_string(), str::to_string))
            .collect::<Vec<_>>()
            .join(", "),
        Some(other) => other.to_string(),
        None => "(not set)".to_string(),
    }
}

/// What the user typed for a setting, as the value to save. `like` is the setting's current
/// value, which decides whether a number is whole.
fn parse_value(kind: Kind, typed: &str, like: Option<&toml::Value>) -> Result<toml::Value, String> {
    let typed = typed.trim();
    match kind {
        Kind::Switch => match typed {
            "on" | "true" | "yes" => Ok(toml::Value::Boolean(true)),
            "off" | "false" | "no" => Ok(toml::Value::Boolean(false)),
            _ => Err("expected on or off".to_string()),
        },
        Kind::Choice(options) => match options.iter().find(|option| **option == typed) {
            Some(option) => Ok(toml::Value::String(option.to_string())),
            None => Err(format!("expected one of: {}", options.join(", "))),
        },
        Kind::Number if matches!(like, Some(toml::Value::Float(_))) => typed
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(toml::Value::Float)
            .ok_or_else(|| "expected a number that is not negative".to_string()),
        Kind::Number => typed
            .parse::<u32>()
            .map(|number| toml::Value::Integer(i64::from(number)))
            .map_err(|_| "expected a whole number that is not negative".to_string()),
        Kind::Text => Ok(toml::Value::String(typed.to_string())),
        Kind::List => Ok(toml::Value::Array(
            typed
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| toml::Value::String(item.to_string()))
                .collect(),
        )),
    }
}

/// Whether the configuration accepts `value` at `path`, with its own error when it does not
/// (an unknown mode, a model name that does not parse).
fn validate(path: &str, value: &toml::Value) -> Result<(), String> {
    let mut nested = value.clone();
    for part in path.rsplit('.') {
        let mut table = toml::Table::new();
        table.insert(part.to_string(), nested);
        nested = toml::Value::Table(table);
    }
    nested
        .try_into::<Config>()
        .map(|_| ())
        .map_err(|e| e.message().to_string())
}

/// A value as an item of the config file.
fn config_item(value: &toml::Value) -> Option<toml_edit::Item> {
    Some(match value {
        toml::Value::Boolean(on) => toml_edit::value(*on),
        toml::Value::Integer(number) => toml_edit::value(*number),
        toml::Value::Float(number) => toml_edit::value(*number),
        toml::Value::String(text) => toml_edit::value(text.as_str()),
        toml::Value::Array(items) => toml_edit::value(toml_edit::Array::from_iter(
            items.iter().filter_map(|item| item.as_str()),
        )),
        _ => return None,
    })
}

/// The roles a person can give a model, in the order the menu lists them, with what each is for.
/// The lead and every built-in agent have a role of their own, which falls back to a general one
/// until it is set.
const ROLES: [(&str, &str); 18] = [
    ("lead", "the agent you talk to"),
    ("scout", "fast read-only exploration"),
    ("planner", "writes implementation plans"),
    ("coder", "implements changes"),
    ("tester", "writes and runs tests"),
    ("reviewer", "reviews diffs"),
    ("verifier", "checks a goal is really met"),
    ("researcher", "web research"),
    ("designer", "user interface work"),
    ("artist", "images, video, 3D and audio"),
    ("playtester", "plays builds and reports defects"),
    ("default", "everything without a model of its own"),
    ("smol", "small, quick jobs: summaries, memory"),
    ("slow", "the most capable model, for hard reviews"),
    ("plan", "planning"),
    ("verify", "verification"),
    ("vision", "work that needs to see images"),
    ("memory", "extracting memories"),
];

impl Settings {
    /// Who the providers' credentials come from, one line per provider that has one.
    fn signed_in(&self) -> Vec<(String, String)> {
        self.providers
            .iter()
            .filter(|p| p.requires_key)
            .filter_map(|p| self.credentials.source(p).map(|source| (p.id.clone(), source)))
            .collect()
    }

    /// The value of a setting: what the user's config file says, else what this run started with.
    fn current(&self, path: &str) -> Option<toml::Value> {
        let saved = std::fs::read_to_string(&self.home_config)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok());
        saved
            .as_ref()
            .and_then(|table| lookup(table, path))
            .or_else(|| lookup(&self.effective, path))
            .cloned()
    }

    /// Saves `value` for a setting in the user's config file and applies it to the running UI
    /// when it is one that can change at once. Returns the line that says what happened.
    fn save(&self, setting: &Setting, value: toml::Value, ctx: &ExtCtx) -> Result<String, String> {
        validate(setting.path, &value).map_err(|why| format!("{}: {why}", setting.label))?;
        let path: Vec<&str> = setting.path.split('.').collect();
        // Nothing typed puts the setting back on its default.
        let cleared = matches!(&value, toml::Value::String(text) if text.is_empty())
            || matches!(&value, toml::Value::Array(items) if items.is_empty());
        let item = if cleared { None } else { config_item(&value) };
        crate::init::set_config_value(&self.home_config, &path, item).map_err(|e| format!("{e:#}"))?;
        let when = if setting.live {
            if let Some(live) = ctx.services.get::<LiveSettings>() {
                (live.0)(setting.path, &value);
            }
            ""
        } else {
            " (applies to the next session)"
        };
        Ok(format!("{} = {}{when}", setting.path, shown(Some(&value))))
    }

    /// Shows one section until the user goes back; each change is saved as it is made.
    async fn section(&self, name: &str, ui: &Arc<Ui>, ctx: &ExtCtx, done: &mut Vec<String>) {
        let rows: Vec<&Setting> = SETTINGS.iter().filter(|s| s.section == name).collect();
        loop {
            // A tab sets the value apart from the label: the UI shows it in a column of its own.
            let mut labels: Vec<String> = rows
                .iter()
                .map(|setting| {
                    format!(
                        "{}\t{}",
                        setting.label,
                        shown(self.current(setting.path).as_ref())
                    )
                })
                .collect();
            labels.push("Back".to_string());
            let heading = format!("{name}\nsaved in {}", self.home_config.display());
            let Ok(index) = ui.0.select(&heading, &labels).await else {
                return;
            };
            let Some(setting) = rows.get(index) else {
                return;
            };
            let current = self.current(setting.path);
            let typed = match setting.kind {
                Kind::Switch => Some(match current {
                    Some(toml::Value::Boolean(true)) => "off".to_string(),
                    _ => "on".to_string(),
                }),
                Kind::Choice(options) => {
                    let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
                    ui.0.select(setting.label, &options)
                        .await
                        .ok()
                        .and_then(|chosen| options.get(chosen).cloned())
                }
                Kind::Number | Kind::Text | Kind::List => {
                    let prompt = format!("{} (now: {})", setting.label, shown(current.as_ref()));
                    ui.0.input(&prompt).await.ok()
                }
            };
            let Some(typed) = typed else { continue };
            let saved = parse_value(setting.kind, &typed, current.as_ref())
                .map_err(|why| format!("{}: {why}", setting.label))
                .and_then(|value| self.save(setting, value, ctx));
            match saved {
                Ok(line) => {
                    ui.0.notify(Level::Info, &line);
                    done.push(line);
                }
                Err(why) => ui.0.notify(Level::Error, &why),
            }
        }
    }

    fn default_model(&self, router: Option<&Router>) -> Option<String> {
        let chain = router?.roles().get("default").cloned()?;
        (!chain.is_empty()).then(|| chain_label(&chain))
    }

    fn menu(&self, router: Option<&Router>) -> (String, Vec<(Item, String)>) {
        let signed_in = self.signed_in();
        let accounts = match signed_in.is_empty() {
            true => "no provider has a credential yet".to_string(),
            false => {
                let names: Vec<&str> = signed_in.iter().map(|(id, _)| id.as_str()).collect();
                format!("credentials for: {}", names.join(", "))
            }
        };
        let default = self.default_model(router);
        // The first line is the title of the menu; a tab sets a row's detail apart.
        let heading = format!(
            "Settings\n{accounts} · default model: {}",
            default.as_deref().unwrap_or("none")
        );
        let mut items = vec![
            (
                Item::Subscription,
                "Sign in with a subscription\tClaude, ChatGPT".to_string(),
            ),
            (Item::ApiKey, "Add an API key".to_string()),
            (
                Item::DefaultModel,
                format!(
                    "Choose the default model\tnow: {}",
                    default.as_deref().unwrap_or("none")
                ),
            ),
            (
                Item::Roles,
                "Models for each role\tlead, coder, scout, reviewer, ...".to_string(),
            ),
        ];
        items.extend(
            sections()
                .into_iter()
                .map(|section| (Item::Section(section), section.to_string())),
        );
        let stored = self.credentials.load();
        if !stored.is_empty() {
            items.push((Item::SignOut, "Sign out or forget a key".to_string()));
        }
        items.push((Item::Done, "Done".to_string()));
        (heading, items)
    }

    async fn subscription(&self, ui: &Arc<Ui>) -> Result<Option<String>, String> {
        let candidates = login_providers(&self.providers);
        let labels: Vec<String> = candidates
            .iter()
            .map(|p| match subscription_name(&p.id) {
                Some(name) => format!("{name}\t{}", p.id),
                None => format!("{}\t{}", p.name, p.id),
            })
            .collect();
        let Ok(index) = ui.0.select("Sign in with which account?", &labels).await else {
            return Ok(None);
        };
        let provider = candidates[index];
        let Some(settings) = &provider.oauth else {
            return Ok(None);
        };
        let line = sign_in_from_ui(&self.credentials, provider, settings, ui.clone(), None).await?;
        Ok(line.map(|line| format!("{line}\n{provider}", provider = provider.id)))
    }

    async fn api_key(&self, ui: &Arc<Ui>) -> Result<Option<String>, String> {
        let candidates = key_providers(&self.providers);
        let labels: Vec<String> = candidates
            .iter()
            .map(|p| {
                let state = match self.credentials.source(p) {
                    Some(source) => format!(" · has a key from {source}"),
                    None => String::new(),
                };
                format!("{}\t{}{state}", p.id, p.api_key_env.join(" or "))
            })
            .collect();
        let Ok(index) = ui.0.select("Store an API key for which provider?", &labels).await else {
            return Ok(None);
        };
        let provider = candidates[index];
        let prompt = format!("API key for {} (stored in ~/.azazel/auth.json)", provider.id);
        match ui.0.secret(&prompt).await {
            Ok(key) => store_key(&self.credentials, provider, &key, None)
                .map(|line| Some(format!("{line}\n{provider}", provider = provider.id))),
            Err(_) => Ok(None),
        }
    }

    /// Lets the user pick a model of `only` (or of a provider they choose), then makes it the
    /// default for this session and later ones.
    async fn model_for(
        &self,
        ui: &Arc<Ui>,
        router: &Router,
        only: Option<&str>,
        role: &str,
    ) -> Result<Option<String>, String> {
        let provider = match only {
            Some(id) => id.to_string(),
            None => {
                // Providers that can answer: with a credential, or local servers that need none.
                let usable: Vec<&ProviderConfig> = self
                    .providers
                    .iter()
                    .filter(|p| !p.requires_key || self.credentials.source(p).is_some())
                    .collect();
                let labels: Vec<String> = usable
                    .iter()
                    .map(|p| match p.requires_key {
                        true => p.id.clone(),
                        false => format!("{}\tlocal server", p.id),
                    })
                    .collect();
                let Ok(index) = ui.0.select("Models of which provider?", &labels).await else {
                    return Ok(None);
                };
                usable[index].id.clone()
            }
        };
        ui.0.notify(Level::Info, &format!("asking {provider} for its models…"));
        let mut ids: Vec<String> = match router.provider(&provider) {
            Some(handle) => match tokio::time::timeout(LIST_TIMEOUT, handle.list_models()).await {
                Ok(Ok(models)) => models.into_iter().map(|m| m.id).collect(),
                Ok(Err(e)) => {
                    ui.0.notify(Level::Warn, &format!("{provider} did not list its models: {e}"));
                    Vec::new()
                }
                Err(_) => Vec::new(),
            },
            None => return Err(format!("`{provider}` is not a provider")),
        };
        if ids.is_empty() {
            ids = router
                .catalog()
                .for_provider(&provider)
                .into_iter()
                .map(|m| m.id.clone())
                .collect();
        }
        if ids.is_empty() {
            return Err(format!("{provider} offers no models"));
        }
        ids.sort();
        ids.dedup();
        // Beside each model: its context window and price, as far as the catalogue knows them.
        let login = self
            .providers
            .iter()
            .find(|p| p.id == provider)
            .and_then(|config| self.credentials.source(config))
            .is_some_and(|source| source.starts_with("auth.json (login"));
        let catalog = router.catalog();
        let labels: Vec<String> = ids
            .iter()
            .map(|id| match catalog.contains(&provider, id) {
                true => {
                    let info = catalog.get(&provider, id);
                    let detail = model_detail(info.context_window, info.cost.as_ref(), login);
                    format!("{id}\t{detail}")
                }
                false if login => format!("{id}\tsubscription"),
                false => id.clone(),
            })
            .collect();
        let Ok(index) =
            ui.0.select(&format!("Model for {role} ({provider})"), &labels)
                .await
        else {
            return Ok(None);
        };
        let model = format!("{provider}/{}", ids[index]);
        let spec: ModelSpec = model.parse().map_err(|e| format!("{model}: {e}"))?;
        crate::init::write_role(&self.home_config, role, Some(&model)).map_err(|e| format!("{e:#}"))?;
        router.set_role(role, vec![spec]);
        let what = match role {
            "default" => "the default model".to_string(),
            role => format!("the {role} model"),
        };
        Ok(Some(format!(
            "{what} is now {model} (saved in {})",
            self.home_config.display()
        )))
    }

    /// Lets the person pick a role and give it a model, or put it back on its fallback.
    async fn roles(&self, ui: &Arc<Ui>, router: &Router) -> Result<Option<String>, String> {
        let configured = router.roles();
        let labels: Vec<String> = ROLES
            .iter()
            .map(|(role, what)| {
                let model = router
                    .resolve(&[ModelSpec::Role(role.to_string())])
                    .first()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "no model".to_string());
                let via = match configured.get(*role) {
                    Some(chain) if chain.iter().any(|spec| matches!(spec, ModelSpec::Model(_))) => {
                        String::new()
                    }
                    Some(chain) => format!(" (via {})", chain_label(chain)),
                    None => String::new(),
                };
                format!("{role}\t{model}{via} · {what}")
            })
            .collect();
        let Ok(index) = ui.0.select("Which role?", &labels).await else {
            return Ok(None);
        };
        let role = ROLES[index].0;
        let fallback = Config::default().roles.get(role).cloned();
        let mut actions = vec!["Choose a model".to_string()];
        if let Some(chain) = &fallback {
            actions.push(format!("Use the fallback\t{}", chain_label(chain)));
        }
        let Ok(action) = ui.0.select(&format!("Model for {role}"), &actions).await else {
            return Ok(None);
        };
        match (action, fallback) {
            (1, Some(chain)) => {
                crate::init::write_role(&self.home_config, role, None).map_err(|e| format!("{e:#}"))?;
                let label = chain_label(&chain);
                router.set_role(role, chain);
                Ok(Some(format!("{role} now follows {label}")))
            }
            _ => self.model_for(ui, router, None, role).await,
        }
    }

    async fn sign_out(&self, ui: &Arc<Ui>) -> Result<Option<String>, String> {
        let mut stored: Vec<String> = self.credentials.load().into_keys().collect();
        stored.sort();
        let Ok(index) =
            ui.0.select("Forget the key or login of which provider?", &stored)
                .await
        else {
            return Ok(None);
        };
        forget_key(&self.credentials, &stored[index]).map(Some)
    }
}

/// The provider id a successful sign-in or key line ends with (see `subscription`, `api_key`).
fn split_provider(done: String) -> (String, String) {
    match done.rsplit_once('\n') {
        Some((line, provider)) => (line.to_string(), provider.to_string()),
        None => (done, String::new()),
    }
}

#[async_trait]
impl CommandHandler for Settings {
    async fn run(&self, _: &str, ctx: &ExtCtx) -> CommandOutcome {
        let Some(ui) = ctx.services.get::<Ui>() else {
            return CommandOutcome::Error("settings need the interactive UI".to_string());
        };
        let router = ctx.services.get::<Router>();
        let mut done: Vec<String> = Vec::new();
        loop {
            let (heading, items) = self.menu(router.as_deref());
            let labels: Vec<String> = items.iter().map(|(_, label)| label.clone()).collect();
            let chosen = match ui.0.select(&heading, &labels).await {
                Ok(index) => items[index].0,
                Err(UiError::Cancelled | UiError::NoUser) => break,
            };
            let outcome = match chosen {
                Item::Done => break,
                Item::Subscription => self.subscription(&ui).await,
                Item::ApiKey => self.api_key(&ui).await,
                Item::DefaultModel => match router.as_deref() {
                    Some(router) => self.model_for(&ui, router, None, "default").await,
                    None => Err("no model router in this session".to_string()),
                },
                Item::Roles => match router.as_deref() {
                    Some(router) => self.roles(&ui, router).await,
                    None => Err("no model router in this session".to_string()),
                },
                Item::SignOut => self.sign_out(&ui).await,
                Item::Section(name) => {
                    self.section(name, &ui, ctx, &mut done).await;
                    Ok(None)
                }
            };
            match outcome {
                Ok(Some(line)) => {
                    let (line, provider) = split_provider(line);
                    ui.0.notify(Level::Info, &line);
                    done.push(line);
                    // A first credential with no default model yet goes straight on to choosing
                    // one of that provider's models.
                    if let Some(router) = router.as_deref()
                        && !provider.is_empty()
                        && self.default_model(Some(router)).is_none()
                    {
                        match self.model_for(&ui, router, Some(&provider), "default").await {
                            Ok(Some(line)) => {
                                ui.0.notify(Level::Info, &line);
                                done.push(line);
                            }
                            Ok(None) => {}
                            Err(why) => ui.0.notify(Level::Error, &why),
                        }
                    }
                }
                Ok(None) => {}
                Err(why) => ui.0.notify(Level::Error, &why),
            }
        }
        if !done.is_empty() {
            azazel_core::ext::config_changed(&ctx.services, "settings").await;
        }
        match done.is_empty() {
            true => CommandOutcome::Done,
            false => CommandOutcome::Show(format!("settings changed:\n- {}", done.join("\n- "))),
        }
    }
}

/// Registers `/settings`.
pub struct SettingsExt {
    pub providers: Vec<ProviderConfig>,
    pub credentials: Arc<Credentials>,
    pub home_config: PathBuf,
    /// The configuration this run started with.
    pub config: Config,
}

impl Extension for SettingsExt {
    fn id(&self) -> &str {
        "settings"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(Command {
            name: "settings".to_string(),
            description: "Sign in, add API keys, choose models and change settings".to_string(),
            argument_hint: None,
            handler: Arc::new(Settings {
                providers: self.providers.clone(),
                credentials: self.credentials.clone(),
                home_config: self.home_config.clone(),
                effective: toml::Table::try_from(&self.config).unwrap_or_default(),
            }),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use azazel_ai::providers::presets;

    use super::*;

    #[test]
    fn subscriptions_and_keys_are_offered_where_each_makes_sense() {
        let providers = presets::presets();
        let logins: Vec<&str> = login_providers(&providers)
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(
            logins,
            [
                "anthropic",
                "github-copilot",
                "google-gemini-cli",
                "openai-codex",
                "openrouter",
                "xai-oauth"
            ]
        );

        let keys: Vec<&str> = key_providers(&providers).iter().map(|p| p.id.as_str()).collect();
        // A subscription-only backend takes no key; a local server needs none.
        assert!(keys.contains(&"anthropic") && keys.contains(&"openai"));
        assert!(!keys.contains(&"openai-codex"));
        assert!(!keys.contains(&"ollama"));
    }

    #[test]
    fn a_model_row_shows_its_window_and_its_price_or_that_it_is_a_subscription() {
        let cost = |input, output| Cost {
            input,
            output,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        assert_eq!(
            model_detail(200_000, Some(&cost(3.0, 15.0)), false),
            "ctx 200k · $3/$15"
        );
        assert_eq!(
            model_detail(1_000_000, Some(&cost(1.25, 10.0)), false),
            "ctx 1M · $1.25/$10"
        );
        assert_eq!(
            model_detail(1_048_576, Some(&cost(0.075, 0.3)), false),
            "ctx 1048k · $0.075/$0.3"
        );
        assert_eq!(model_detail(128_000, None, false), "ctx 128k");
        assert_eq!(model_detail(8192, None, false), "ctx 8k");
        assert_eq!(model_detail(512, None, false), "ctx 512");
        // A login is not paid for by the token, whatever the catalogue says the model costs.
        assert_eq!(
            model_detail(200_000, Some(&cost(15.0, 75.0)), true),
            "ctx 200k · subscription"
        );
        assert_eq!(model_detail(2_500_000, None, true), "ctx 2.5M · subscription");
    }

    #[test]
    fn a_menu_choice_line_carries_the_provider_it_set_up() {
        assert_eq!(
            split_provider("stored the openai key in auth.json\nopenai".to_string()),
            (
                "stored the openai key in auth.json".to_string(),
                "openai".to_string()
            )
        );
        // A line from sign-out names no provider to go on with.
        assert_eq!(split_provider("forgot it".to_string()).1, "");
    }

    #[test]
    fn the_menu_shows_the_current_default_and_offers_sign_out_only_with_something_stored() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            providers: presets::presets(),
            credentials: Arc::new(Credentials::with_env(
                dir.path().join("auth.json"),
                Arc::new(|_: &str| None),
            )),
            home_config: dir.path().join("config.toml"),
            effective: toml::Table::try_from(Config::default()).unwrap(),
        };
        let router = Router::new(
            Vec::new(),
            azazel_ai::Catalog::default(),
            Default::default(),
            Default::default(),
        );
        let (heading, items) = settings.menu(Some(&router));
        assert!(heading.contains("default model: none"), "{heading}");
        assert!(!items.iter().any(|(item, _)| *item == Item::SignOut));

        router.set_role("default", vec!["anthropic/some-model".parse().unwrap()]);
        settings
            .credentials
            .store(
                "openai",
                azazel_ai::Credential::ApiKey {
                    key: "sk".to_string(),
                },
            )
            .unwrap();
        let (heading, items) = settings.menu(Some(&router));
        assert!(
            heading.contains("default model: anthropic/some-model"),
            "{heading}"
        );
        assert!(heading.contains("credentials for: openai"), "{heading}");
        assert!(items.iter().any(|(item, _)| *item == Item::SignOut));
    }

    fn settings_in(dir: &std::path::Path) -> Settings {
        Settings {
            providers: Vec::new(),
            credentials: Arc::new(Credentials::with_env(
                dir.join("auth.json"),
                Arc::new(|_: &str| None),
            )),
            home_config: dir.join("config.toml"),
            effective: toml::Table::try_from(Config::default()).unwrap(),
        }
    }

    fn ctx() -> ExtCtx {
        ExtCtx {
            services: Arc::new(azazel_agent::Services::new()),
            cwd: PathBuf::new(),
            session_id: String::new(),
        }
    }

    #[test]
    fn every_setting_exists_in_the_configuration_with_a_value_of_its_kind() {
        let defaults = toml::Table::try_from(Config::default()).unwrap();
        for setting in SETTINGS {
            let value = lookup(&defaults, setting.path)
                .unwrap_or_else(|| panic!("{} is not a configuration key", setting.path));
            let fits = match setting.kind {
                Kind::Switch => value.is_bool(),
                Kind::Choice(options) => {
                    for option in options {
                        let candidate = toml::Value::String(option.to_string());
                        validate(setting.path, &candidate)
                            .unwrap_or_else(|why| panic!("{} = {option}: {why}", setting.path));
                    }
                    value.as_str().is_some_and(|now| options.contains(&now))
                }
                Kind::Number => value.is_integer() || value.is_float(),
                Kind::Text => value.is_str(),
                Kind::List => value.is_array(),
            };
            assert!(fits, "{} is {value:?}, not a {:?}", setting.path, setting.kind);
        }
        assert_eq!(
            sections(),
            [
                "Appearance",
                "Editor",
                "Permissions",
                "Notifications",
                "Limits",
                "Sessions",
                "Memory",
                "Updates",
                "Voice",
                "Team"
            ]
        );
    }

    #[test]
    fn typed_values_are_parsed_by_kind_and_checked_against_the_configuration() {
        let float = toml::Value::Float(0.0);
        assert_eq!(
            parse_value(Kind::Switch, "on", None),
            Ok(toml::Value::Boolean(true))
        );
        assert!(parse_value(Kind::Switch, "maybe", None).is_err());
        assert_eq!(
            parse_value(Kind::Number, " 2.5 ", Some(&float)),
            Ok(toml::Value::Float(2.5))
        );
        assert_eq!(
            parse_value(Kind::Number, "12", None),
            Ok(toml::Value::Integer(12))
        );
        assert!(
            parse_value(Kind::Number, "1.5", None).is_err(),
            "a whole number stays whole"
        );
        assert!(parse_value(Kind::Number, "-1", Some(&float)).is_err());
        assert!(
            parse_value(Kind::Choice(&["emacs", "vim"]), "nano", None)
                .unwrap_err()
                .contains("emacs, vim")
        );
        assert_eq!(
            parse_value(Kind::List, "anthropic/a, @slow ,", None),
            Ok(toml::Value::Array(vec![
                toml::Value::String("anthropic/a".into()),
                toml::Value::String("@slow".into())
            ]))
        );

        assert!(validate("editor.mode", &toml::Value::String("vim".into())).is_ok());
        assert!(validate("editor.mode", &toml::Value::String("nano".into())).is_err());
        assert!(
            validate(
                "advisor.model",
                &parse_value(Kind::List, "not a model", None).unwrap()
            )
            .is_err()
        );
        assert!(validate("limits.max_turns", &toml::Value::Integer(5)).is_ok());
    }

    #[test]
    fn a_saved_setting_lands_in_the_config_file_is_read_back_and_reaches_the_ui_when_live() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_in(dir.path());
        std::fs::write(&settings.home_config, "# mine\n[tui]\ntheme = \"light\"\n").unwrap();
        let by_path = |path: &str| SETTINGS.iter().find(|s| s.path == path).unwrap();
        assert_eq!(shown(settings.current("tui.theme").as_ref()), "light");
        assert_eq!(shown(settings.current("editor.mode").as_ref()), "emacs");
        assert_eq!(shown(settings.current("editor.command").as_ref()), "(not set)");
        assert_eq!(shown(settings.current("advisor.model").as_ref()), "(none)");
        assert_eq!(shown(settings.current("notify.bell").as_ref()), "on");

        let context = ctx();
        let seen: Arc<std::sync::Mutex<Vec<(String, toml::Value)>>> = Arc::default();
        let sink = seen.clone();
        context.services.insert(Arc::new(LiveSettings(Arc::new(
            move |path: &str, value: &toml::Value| {
                sink.lock().unwrap().push((path.to_string(), value.clone()));
            },
        ))));

        let line = settings
            .save(
                by_path("editor.mode"),
                toml::Value::String("vim".into()),
                &context,
            )
            .unwrap();
        assert_eq!(line, "editor.mode = vim");
        let line = settings
            .save(by_path("limits.max_cost_usd"), toml::Value::Float(2.5), &context)
            .unwrap();
        assert_eq!(line, "limits.max_cost_usd = 2.5 (applies to the next session)");
        settings
            .save(
                by_path("advisor.model"),
                parse_value(Kind::List, "anthropic/a, @slow", None).unwrap(),
                &context,
            )
            .unwrap();
        // Only the live setting reached the UI.
        assert_eq!(
            *seen.lock().unwrap(),
            [("editor.mode".to_string(), toml::Value::String("vim".into()))]
        );

        let saved = std::fs::read_to_string(&settings.home_config).unwrap();
        assert!(saved.starts_with("# mine\n[tui]\ntheme = \"light\"\n"), "{saved}");
        let loaded: Config = saved.parse::<toml::Table>().unwrap().try_into().unwrap();
        assert_eq!(loaded.editor.mode, azazel_core::config::EditorMode::Vim);
        assert_eq!(loaded.limits.max_cost_usd, 2.5);
        assert_eq!(loaded.advisor.model.len(), 2);
        assert_eq!(
            shown(settings.current("advisor.model").as_ref()),
            "anthropic/a, @slow"
        );

        // A value the configuration rejects is not written; an empty one removes the key.
        let refused = settings.save(
            by_path("editor.mode"),
            toml::Value::String("nano".into()),
            &context,
        );
        assert!(refused.unwrap_err().starts_with("Keys in the input: "));
        settings
            .save(by_path("tui.theme"), toml::Value::String(String::new()), &context)
            .unwrap();
        let saved = std::fs::read_to_string(&settings.home_config).unwrap();
        assert!(!saved.contains("theme"), "{saved}");
        assert_eq!(shown(settings.current("tui.theme").as_ref()), "dark");
    }
}
