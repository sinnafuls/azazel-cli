//! The values behind the settings screen: what each layer of the configuration says about a
//! key, which of them applies, and the file a change is written to.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use azazel_ai::{ModelSpec, Router};
use azazel_core::config::{self, LayerKind};
use azazel_core::settings::{Kind, Setting, lookup};
use azazel_core::{Config, LoadOptions, Paths};
use toml::{Table, Value};

use super::LiveSettings;

/// The lines that say what a run of the settings changed, for the transcript.
pub(crate) type Changes = Arc<Mutex<Vec<String>>>;

/// Where a value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// `~/.azazel/config.toml`.
    Home,
    /// `<project>/.azazel/config.toml`.
    Project,
    /// `<project>/.azazel/config.local.toml`.
    Local,
    /// An `AZAZEL_<TABLE>__<KEY>` variable, by its name.
    Env(String),
    /// `--set` and the other flags of the command line.
    Flag,
}

impl Source {
    /// One word for the column beside a value.
    pub(crate) fn word(&self) -> &'static str {
        match self {
            Source::Home => "home",
            Source::Project => "project",
            Source::Local => "local",
            Source::Env(_) => "env",
            Source::Flag => "flag",
        }
    }

    /// Who sets the value, as the end of a sentence: `set in the project config`.
    pub(crate) fn phrase(&self) -> String {
        match self {
            Source::Home => "set in the home config".to_string(),
            Source::Project => "set in the project config".to_string(),
            Source::Local => "set in the project's config.local.toml".to_string(),
            Source::Env(name) => format!("set by {name}"),
            Source::Flag => "set on the command line".to_string(),
        }
    }
}

/// The file changes are written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Home,
    Project,
}

impl Scope {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Scope::Home => "home",
            Scope::Project => "project",
        }
    }
}

pub(crate) struct Store {
    paths: Paths,
    options: LoadOptions,
    live: Option<Arc<LiveSettings>>,
    router: Option<Arc<Router>>,
    scope: Scope,
    /// What each layer sets, lowest first: the files, the environment, the command line.
    layers: Vec<(Source, Table)>,
    /// The configuration in effect, as a table.
    effective: Table,
    changes: Changes,
}

/// A value as a row shows it.
pub(crate) fn shown(value: Option<&Value>) -> String {
    match value {
        Some(Value::Boolean(true)) => "on".to_string(),
        Some(Value::Boolean(false)) => "off".to_string(),
        Some(Value::String(text)) if text.is_empty() => "(not set)".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) if items.is_empty() => "(none)".to_string(),
        Some(Value::Array(items)) => items.iter().map(literal).collect::<Vec<_>>().join(", "),
        Some(Value::Table(entries)) if entries.is_empty() => "(none)".to_string(),
        Some(Value::Table(entries)) => entries.keys().cloned().collect::<Vec<_>>().join(", "),
        Some(other) => other.to_string(),
        None => "(not set)".to_string(),
    }
}

/// A value as it is typed: a text bare where that reads back as the same text, anything else
/// as TOML.
pub(crate) fn literal(value: &Value) -> String {
    match value {
        Value::String(text)
            if !text.is_empty()
                && text.trim() == text
                && config::parse_literal(text) == Value::String(text.clone()) =>
        {
            text.clone()
        }
        other => other.to_string(),
    }
}

/// One entry of a table as it is typed: `rs = rustfmt --edition 2024`.
pub(crate) fn entry_text(key: &str, value: &Value) -> String {
    format!("{key} = {}", literal(value))
}

/// What was typed for an entry of a table, as its name and its value.
pub(crate) fn parse_entry(typed: &str) -> Result<(String, Value), String> {
    let (key, raw) = typed
        .split_once('=')
        .ok_or_else(|| "expected name = value".to_string())?;
    let key = key.trim().trim_matches('"').to_string();
    if key.is_empty() {
        return Err("expected name = value".to_string());
    }
    Ok((key, config::parse_literal(raw.trim())))
}

/// What the user typed for a setting, as the value to save. `like` is the setting's default,
/// which decides whether a number is whole.
pub(crate) fn parse_value(kind: Kind, typed: &str, like: Option<&Value>) -> Result<Value, String> {
    let typed = typed.trim();
    match kind {
        Kind::Switch => match typed {
            "on" | "true" | "yes" => Ok(Value::Boolean(true)),
            "off" | "false" | "no" => Ok(Value::Boolean(false)),
            _ => Err("expected on or off".to_string()),
        },
        Kind::Choice(options) => match options.iter().find(|option| **option == typed) {
            Some(option) => Ok(Value::String(option.to_string())),
            None => Err(format!("expected one of: {}", options.join(", "))),
        },
        Kind::Number if matches!(like, Some(Value::Float(_))) => typed
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(Value::Float)
            .ok_or_else(|| "expected a number that is not negative".to_string()),
        Kind::Number => typed
            .parse::<u64>()
            .ok()
            .and_then(|number| i64::try_from(number).ok())
            .map(Value::Integer)
            .ok_or_else(|| "expected a whole number that is not negative".to_string()),
        Kind::Text | Kind::Theme => Ok(Value::String(typed.to_string())),
        Kind::List => Ok(Value::Array(
            typed
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| Value::String(item.to_string()))
                .collect(),
        )),
        Kind::Table => Err("a table is edited entry by entry".to_string()),
        Kind::Role => Err("a role takes a model from the picker".to_string()),
    }
}

/// Whether the configuration accepts `value` at `path`, with its own error when it does not
/// (an unknown mode, a model name that does not parse).
pub(crate) fn validate(path: &str, value: &Value) -> Result<(), String> {
    let mut nested = value.clone();
    for part in path.rsplit('.') {
        let mut table = Table::new();
        table.insert(part.to_string(), nested);
        nested = Value::Table(table);
    }
    nested
        .try_into::<Config>()
        .map(|_| ())
        .map_err(|e| e.message().to_string())
}

/// A value as a value of the config file. A table becomes an inline table.
fn edit_value(value: &Value) -> toml_edit::Value {
    match value {
        Value::Boolean(on) => (*on).into(),
        Value::Integer(number) => (*number).into(),
        Value::Float(number) => (*number).into(),
        Value::String(text) => text.as_str().into(),
        Value::Datetime(when) => when.to_string().into(),
        Value::Array(items) => toml_edit::Value::Array(items.iter().map(edit_value).collect()),
        Value::Table(entries) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, value) in entries {
                table.insert(key, edit_value(value));
            }
            toml_edit::Value::InlineTable(table)
        }
    }
}

/// The role a `roles.<name>` row is about.
pub(crate) fn role_name(setting: &Setting) -> &'static str {
    setting.path.strip_prefix("roles.").unwrap_or(setting.path)
}

/// The chain as written in config (`["anthropic/claude-x", "@smol"]`), for showing.
pub(crate) fn chain_label(chain: &[ModelSpec]) -> String {
    chain
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Store {
    /// The values as the files, the environment and the command line give them now. Fails
    /// when a config file cannot be read as one.
    pub(crate) fn open(
        paths: Paths,
        options: LoadOptions,
        live: Option<Arc<LiveSettings>>,
        router: Option<Arc<Router>>,
        changes: Changes,
    ) -> Result<Store, String> {
        let mut store = Store {
            paths,
            options,
            live,
            router,
            scope: Scope::Home,
            layers: Vec::new(),
            effective: Table::new(),
            changes,
        };
        store.reload()?;
        Ok(store)
    }

    /// Reads every layer again.
    pub(crate) fn reload(&mut self) -> Result<(), String> {
        let loaded = Config::load(&self.paths, &self.options).map_err(|e| e.to_string())?;
        let mut layers: Vec<(Source, Table)> = loaded
            .layers
            .into_iter()
            .map(|layer| {
                let source = match layer.kind {
                    LayerKind::Home => Source::Home,
                    LayerKind::Project => Source::Project,
                    LayerKind::ProjectLocal => Source::Local,
                };
                (source, layer.table)
            })
            .collect();
        for (name, table) in config::env_overrides(&self.options.env) {
            layers.push((Source::Env(name), table));
        }
        if !self.options.overrides.is_empty() {
            layers.push((Source::Flag, self.options.overrides.clone()));
        }
        self.effective = Table::try_from(&loaded.config).map_err(|e| e.to_string())?;
        self.layers = layers;
        Ok(())
    }

    pub(crate) fn home_config(&self) -> PathBuf {
        self.paths.home_config()
    }

    /// Notes a line for the summary the transcript gets when the settings close.
    pub(crate) fn note(&self, line: String) {
        self.changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }

    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    /// Whether changes may go to the project's config: only in a trusted project that is not
    /// the home directory itself.
    pub(crate) fn can_project(&self) -> bool {
        self.options.trusted && self.paths.project_config() != self.paths.home_config()
    }

    /// Switches between the home config and the project's. Says why not when the project's is
    /// not to be written.
    pub(crate) fn toggle_scope(&mut self) -> Result<Scope, String> {
        self.scope = match self.scope {
            Scope::Project => Scope::Home,
            Scope::Home if self.can_project() => Scope::Project,
            Scope::Home => {
                return Err(
                    "changes go to the project's config only in a trusted project (/trust)".to_string(),
                );
            }
        };
        Ok(self.scope)
    }

    /// The file changes are written to.
    pub(crate) fn target(&self) -> PathBuf {
        match self.scope {
            Scope::Home => self.paths.home_config(),
            Scope::Project => self.paths.project_config(),
        }
    }

    fn target_source(&self) -> Source {
        match self.scope {
            Scope::Home => Source::Home,
            Scope::Project => Source::Project,
        }
    }

    /// The value in effect.
    pub(crate) fn value(&self, path: &str) -> Option<&Value> {
        lookup(&self.effective, path)
    }

    /// What the file that changes go to says, if anything.
    pub(crate) fn own(&self, path: &str) -> Option<&Value> {
        let target = self.target_source();
        self.layers
            .iter()
            .find(|(source, _)| *source == target)
            .and_then(|(_, table)| lookup(table, path))
    }

    /// The layers that set a key, the one that wins first.
    pub(crate) fn sources(&self, path: &str) -> Vec<&Source> {
        self.layers
            .iter()
            .rev()
            .filter(|(_, table)| lookup(table, path).is_some())
            .map(|(source, _)| source)
            .collect()
    }

    /// Whether the lists of every layer are joined for this key (the permission rules).
    pub(crate) fn joined(path: &str) -> bool {
        let parts: Vec<String> = path.split('.').map(str::to_string).collect();
        config::concatenates(&parts)
    }

    /// Whether the value in effect is not the default. The `default` role has no default to
    /// differ from.
    pub(crate) fn changed(&self, setting: &Setting) -> bool {
        match setting.default_value() {
            Some(default) => self.value(setting.path) != Some(&default),
            None => false,
        }
    }

    /// What a row shows as the value of a setting.
    pub(crate) fn shown(&self, setting: &Setting) -> String {
        match setting.kind {
            Kind::Role => self.role_shown(setting),
            Kind::List | Kind::Table => match self.value(setting.path) {
                Some(Value::Array(items)) if items.len() > 3 => format!("{} entries", items.len()),
                Some(Value::Table(entries)) if entries.len() > 3 => format!("{} entries", entries.len()),
                value => shown(value),
            },
            _ => shown(self.value(setting.path)),
        }
    }

    /// The model a role resolves to, and the role it gets it from when it has none of its own.
    fn role_shown(&self, setting: &Setting) -> String {
        let role = role_name(setting);
        let Some(router) = &self.router else {
            return shown(self.value(setting.path));
        };
        let model = router
            .resolve(&[ModelSpec::Role(role.to_string())])
            .first()
            .map(ToString::to_string)
            .unwrap_or_else(|| "no model".to_string());
        match router.roles().get(role) {
            Some(chain) if !chain.iter().any(|spec| matches!(spec, ModelSpec::Model(_))) => {
                format!("{model} (via {})", chain_label(chain))
            }
            _ => model,
        }
    }

    /// The entries of a list or a table as they are edited: the list of the file changes go to
    /// when the lists of all layers are joined, else the value in effect.
    pub(crate) fn entries(&self, setting: &Setting) -> Vec<String> {
        let value = match Store::joined(setting.path) {
            true => self.own(setting.path),
            false => self.value(setting.path),
        };
        match value {
            Some(Value::Array(items)) => items.iter().map(literal).collect(),
            Some(Value::Table(entries)) => entries
                .iter()
                .map(|(key, value)| entry_text(key, value))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Shows a value in the running UI without saving it.
    pub(crate) fn preview(&self, setting: &Setting, value: &Value) {
        if let (true, Some(live)) = (setting.live, &self.live) {
            (live.0)(setting.path, value);
        }
    }

    /// Writes `item` at the path of a setting (and `under` it, for one entry of a table) in
    /// the file changes go to, or removes what is there. The file is put back as it was when
    /// the configuration no longer loads with the change. Returns the value that was in effect
    /// before.
    fn write(
        &mut self,
        setting: &Setting,
        under: Option<&str>,
        value: Option<&Value>,
    ) -> Result<Option<Value>, String> {
        let target = self.target();
        let before = std::fs::read_to_string(&target).ok();
        let was = self.value(setting.path).cloned();
        let mut parts: Vec<&str> = setting.path.split('.').collect();
        parts.extend(under);
        let item = value.map(|value| toml_edit::Item::Value(edit_value(value)));
        crate::init::set_config_value(&target, &parts, item).map_err(|e| format!("{e:#}"))?;
        if let Err(why) = self.reload() {
            let restored = match &before {
                Some(text) => std::fs::write(&target, text),
                None => std::fs::remove_file(&target),
            };
            if let Err(e) = restored {
                tracing::warn!("{} could not be put back: {e}", target.display());
            }
            // The layers as they were; what failed to load is reported, not this.
            let _ = self.reload();
            return Err(why);
        }
        let now = self.value(setting.path);
        if let (true, Some(live), Some(now)) = (setting.live, &self.live, now)
            && Some(now) != was.as_ref()
        {
            (live.0)(setting.path, now);
        }
        Ok(was)
    }

    /// What follows `path = value` in the line that says what was saved: that the next session
    /// has it, or who still sets something else here. `wanted` is the value that was asked for.
    fn after(&self, setting: &Setting, wanted: Option<&Value>) -> String {
        let now = self.value(setting.path);
        let top = self.sources(setting.path).into_iter().next().cloned();
        let mut out = String::new();
        if self.scope == Scope::Project {
            out.push_str(" in the project config");
        }
        match (wanted, top) {
            // Another layer wins over the file that was written.
            (Some(wanted), Some(top))
                if top != self.target_source() && Some(wanted) != now && !Store::joined(setting.path) =>
            {
                out.push_str(&format!("; here it stays {}, {}", shown(now), top.phrase()));
            }
            _ if !setting.live => out.push_str(" (applies to the next session)"),
            _ => {}
        }
        out
    }

    /// Saves `value` for a setting in the file changes go to, and applies it to the running UI
    /// when it is one that can change at once. Returns the line that says what happened.
    pub(crate) fn save(&mut self, setting: &Setting, value: Value) -> Result<String, String> {
        let fail = |why: String| format!("{}: {why}", setting.label);
        if setting.kind == Kind::Role {
            return Err(fail("a role takes a model from the picker".to_string()));
        }
        validate(setting.path, &value).map_err(fail)?;
        // Nothing typed for a text puts it back on what it is without this file; so does no
        // rule at all for a list of rules.
        let empty = match &value {
            Value::String(text) => text.is_empty(),
            Value::Array(items) => items.is_empty() && Store::joined(setting.path),
            _ => false,
        };
        // The default is not written when no file below says otherwise, which keeps the file to
        // what was really chosen.
        let lower = self
            .layers
            .iter()
            .take_while(|(source, _)| *source != self.target_source())
            .any(|(_, table)| lookup(table, setting.path).is_some());
        let is_default = setting.default_value().as_ref() == Some(&value) && !lower;
        let written = (!empty && !is_default).then_some(&value);
        self.write(setting, None, written).map_err(fail)?;
        let wanted = (!empty).then_some(&value);
        let line = format!(
            "{} = {}{}",
            setting.path,
            shown(wanted.or(self.value(setting.path))),
            self.after(setting, wanted)
        );
        self.note(line.clone());
        Ok(line)
    }

    /// Sets one entry of a table setting.
    pub(crate) fn set_entry(&mut self, setting: &Setting, key: &str, value: Value) -> Result<String, String> {
        let fail = |why: String| format!("{}: {why}", setting.label);
        let mut table = Table::new();
        table.insert(key.to_string(), value.clone());
        validate(setting.path, &Value::Table(table)).map_err(fail)?;
        self.write(setting, Some(key), Some(&value)).map_err(fail)?;
        let line = format!(
            "{}.{} = {}{}",
            setting.path,
            key,
            literal(&value),
            self.after(setting, None)
        );
        self.note(line.clone());
        Ok(line)
    }

    /// Removes one entry of a table setting from the file changes go to.
    pub(crate) fn remove_entry(&mut self, setting: &Setting, key: &str) -> Result<String, String> {
        let fail = |why: String| format!("{}: {why}", setting.label);
        let path = format!("{}.{key}", setting.path);
        if self.own(&path).is_none() {
            let who = match self.sources(&path).first() {
                Some(source) => source.phrase(),
                None => "not set".to_string(),
            };
            return Err(fail(format!(
                "{key} is not in the {} config; it is {who}",
                self.scope.word()
            )));
        }
        self.write(setting, Some(key), None).map_err(fail)?;
        let line = match self.sources(&path).first() {
            Some(source) => format!("{path} removed; it is still {}", source.phrase()),
            None => format!("{path} removed"),
        };
        self.note(line.clone());
        Ok(line)
    }

    /// Takes a setting out of the file changes go to, which puts it back on its default
    /// unless another layer sets it. A role goes back to the role it falls back to.
    pub(crate) fn reset(&mut self, setting: &Setting) -> Result<String, String> {
        let fail = |why: String| format!("{}: {why}", setting.label);
        if self.own(setting.path).is_none() {
            return Err(fail(match self.sources(setting.path).first() {
                Some(source) => format!(
                    "nothing to reset in the {} config; it is {}",
                    self.scope.word(),
                    source.phrase()
                ),
                None => "already on its default".to_string(),
            }));
        }
        if setting.kind == Kind::Role && setting.default_value().is_none() {
            return Err(fail(
                "the default model has nothing to fall back to; choose another model instead".to_string(),
            ));
        }
        self.write(setting, None, None).map_err(fail)?;
        if setting.kind == Kind::Role
            && let Some(router) = &self.router
        {
            // The running session follows: the role is what the layers left say it is.
            let chain: Vec<ModelSpec> = match self.value(setting.path) {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| item.as_str()?.parse().ok())
                    .collect(),
                _ => Vec::new(),
            };
            router.set_role(role_name(setting), chain);
        }
        let now = match setting.kind {
            Kind::Role => self.role_shown(setting),
            _ => shown(self.value(setting.path)),
        };
        let who = match self.sources(setting.path).first() {
            Some(source) => source.phrase(),
            None => "the default".to_string(),
        };
        let when = match setting.live || setting.kind == Kind::Role {
            true => "",
            false => " (applies to the next session)",
        };
        let line = format!("{} reset: {now}, {who}{when}", setting.path);
        self.note(line.clone());
        Ok(line)
    }

    /// The default of a setting as a row shows it.
    pub(crate) fn default_shown(setting: &Setting) -> String {
        match (setting.kind, setting.default_value()) {
            (Kind::Role, None) => "none".to_string(),
            (_, default) => shown(default.as_ref()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use azazel_core::settings::setting;

    use super::*;

    /// A home and a project directory with the config files given.
    pub(crate) struct Fixture {
        _tmp: tempfile::TempDir,
        pub paths: Paths,
        /// What reached the running UI, in order.
        pub applied: Arc<Mutex<Vec<(String, Value)>>>,
        pub changes: Changes,
    }

    pub(crate) fn fixture(home: &str, project: &str) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let home_dir = tmp.path().join("home");
        let project_dir = tmp.path().join("proj");
        std::fs::create_dir_all(&home_dir).unwrap();
        std::fs::create_dir_all(project_dir.join(".azazel")).unwrap();
        let paths = Paths::discover(&project_dir, Some(&home_dir.to_string_lossy()));
        for (path, text) in [(paths.home_config(), home), (paths.project_config(), project)] {
            if !text.is_empty() {
                std::fs::write(path, text).unwrap();
            }
        }
        Fixture {
            _tmp: tmp,
            paths,
            applied: Arc::default(),
            changes: Changes::default(),
        }
    }

    impl Fixture {
        pub(crate) fn open(&self, options: LoadOptions) -> Store {
            let sink = self.applied.clone();
            let live = LiveSettings(Arc::new(move |path: &str, value: &Value| {
                sink.lock().unwrap().push((path.to_string(), value.clone()));
            }));
            Store::open(
                self.paths.clone(),
                options,
                Some(Arc::new(live)),
                None,
                self.changes.clone(),
            )
            .unwrap()
        }

        pub(crate) fn trusted(&self) -> Store {
            self.open(LoadOptions {
                trusted: true,
                ..LoadOptions::default()
            })
        }

        pub(crate) fn home_text(&self) -> String {
            std::fs::read_to_string(self.paths.home_config()).unwrap_or_default()
        }

        pub(crate) fn project_text(&self) -> String {
            std::fs::read_to_string(self.paths.project_config()).unwrap_or_default()
        }

        pub(crate) fn applied(&self) -> Vec<(String, Value)> {
            self.applied.lock().unwrap().clone()
        }
    }

    pub(crate) fn row(path: &str) -> &'static Setting {
        setting(path).unwrap_or_else(|| panic!("{path} has no row"))
    }

    fn text(value: &str) -> Value {
        Value::String(value.to_string())
    }

    #[test]
    fn a_value_is_what_the_highest_layer_says_and_the_row_names_that_layer() {
        let f = fixture(
            "[tui]\ntheme = \"light\"\n[editor]\nmode = \"vim\"\n[notify]\nbell = true\n",
            "[tui]\ntheme = \"tokyo-night\"\n",
        );
        let mut overrides = Table::new();
        config::set_override(&mut overrides, "limits.max_turns=7").unwrap();
        let store = f.open(LoadOptions {
            trusted: true,
            overrides,
            env: vec![
                ("AZAZEL_NOTIFY__BELL".to_string(), "false".to_string()),
                ("AZAZEL_HOME".to_string(), "elsewhere".to_string()),
            ],
        });

        assert_eq!(store.value("tui.theme"), Some(&text("tokyo-night")));
        assert_eq!(store.sources("tui.theme"), [&Source::Project, &Source::Home]);
        assert_eq!(store.sources("editor.mode"), [&Source::Home]);
        assert_eq!(store.value("notify.bell"), Some(&Value::Boolean(false)));
        assert_eq!(
            store.sources("notify.bell"),
            [&Source::Env("AZAZEL_NOTIFY__BELL".to_string()), &Source::Home]
        );
        assert_eq!(store.sources("limits.max_turns"), [&Source::Flag]);
        assert_eq!(store.value("limits.max_turns"), Some(&Value::Integer(7)));
        assert!(store.sources("tui.glyphs").is_empty());

        assert!(store.changed(row("tui.theme")) && store.changed(row("editor.mode")));
        assert!(!store.changed(row("tui.glyphs")));
        // The home config says what the default says: that is not a change.
        assert!(store.changed(row("notify.bell")));
        assert_eq!(store.own("notify.bell"), Some(&Value::Boolean(true)));
        assert_eq!(
            Source::Env("AZAZEL_NOTIFY__BELL".into()).phrase(),
            "set by AZAZEL_NOTIFY__BELL"
        );
    }

    #[test]
    fn a_saved_setting_lands_in_the_home_config_and_reaches_the_ui_when_it_applies_at_once() {
        let f = fixture("# mine\n[tui]\ntheme = \"light\"\n", "");
        let mut store = f.trusted();
        assert_eq!(shown(store.value("tui.theme")), "light");
        assert_eq!(shown(store.value("editor.command")), "(not set)");
        assert_eq!(shown(store.value("advisor.model")), "(none)");
        assert_eq!(shown(store.value("notify.bell")), "on");

        assert_eq!(
            store.save(row("editor.mode"), text("vim")).unwrap(),
            "editor.mode = vim"
        );
        assert_eq!(
            store.save(row("limits.max_cost_usd"), Value::Float(2.5)).unwrap(),
            "limits.max_cost_usd = 2.5 (applies to the next session)"
        );
        let chain = parse_value(Kind::List, "anthropic/a, @slow", None).unwrap();
        store.save(row("advisor.model"), chain).unwrap();
        // Only the setting that applies at once reached the UI.
        assert_eq!(f.applied(), [("editor.mode".to_string(), text("vim"))]);

        let saved = f.home_text();
        assert!(saved.starts_with("# mine\n[tui]\ntheme = \"light\"\n"), "{saved}");
        let loaded: Config = saved.parse::<Table>().unwrap().try_into().unwrap();
        assert_eq!(loaded.editor.mode, config::EditorMode::Vim);
        assert_eq!(loaded.limits.max_cost_usd, 2.5);
        assert_eq!(loaded.advisor.model.len(), 2);
        assert_eq!(shown(store.value("advisor.model")), "anthropic/a, @slow");
        assert_eq!(store.sources("editor.mode"), [&Source::Home]);
        assert_eq!(f.changes.lock().unwrap().len(), 3);

        // A value the configuration rejects is not written.
        let refused = store.save(row("editor.mode"), text("nano"));
        assert!(refused.unwrap_err().starts_with("Keys in the input: "));
        assert!(f.home_text().contains("mode = \"vim\""));
    }

    #[test]
    fn the_default_and_an_empty_text_take_the_key_out_of_the_file() {
        let f = fixture(
            "[tui]\ntheme = \"light\"\nshow_thinking = true\n[checkpoints]\nexclude = [\"target/\"]\n",
            "",
        );
        let mut store = f.trusted();
        // Nothing typed for a text puts it back on its default.
        let line = store.save(row("tui.theme"), text("")).unwrap();
        assert_eq!(line, "tui.theme = dark");
        assert!(!f.home_text().contains("theme"), "{}", f.home_text());
        assert_eq!(f.applied(), [("tui.theme".to_string(), text("dark"))]);
        // So does the default itself.
        store
            .save(row("tui.show_thinking"), Value::Boolean(false))
            .unwrap();
        assert!(!f.home_text().contains("show_thinking"), "{}", f.home_text());
        // An empty list is not the default of a list that has entries by default: it is kept.
        store
            .save(row("checkpoints.exclude"), Value::Array(Vec::new()))
            .unwrap();
        assert!(f.home_text().contains("exclude = []"), "{}", f.home_text());
        assert_eq!(
            store.value("checkpoints.exclude"),
            Some(&Value::Array(Vec::new()))
        );
        assert!(store.changed(row("checkpoints.exclude")));
    }

    #[test]
    fn a_change_that_another_layer_overrides_is_saved_and_said_to_be_overridden() {
        let f = fixture("", "[tui]\ntheme = \"tokyo-night\"\n[editor]\nmode = \"vim\"\n");
        let mut store = f.trusted();
        let line = store.save(row("tui.theme"), text("light")).unwrap();
        assert_eq!(
            line,
            "tui.theme = light; here it stays tokyo-night, set in the project config"
        );
        assert!(f.home_text().contains("theme = \"light\""));
        assert_eq!(store.value("tui.theme"), Some(&text("tokyo-night")));
        // What applies did not change, so the UI was not told.
        assert!(f.applied().is_empty());

        // The default in the home config would be no change, but the file is not the one that
        // decides here either way.
        let line = store.save(row("editor.mode"), text("emacs")).unwrap();
        assert_eq!(
            line,
            "editor.mode = emacs; here it stays vim, set in the project config"
        );
        assert_eq!(f.home_text().matches("mode").count(), 0, "{}", f.home_text());
    }

    #[test]
    fn the_project_config_is_written_only_in_a_trusted_project() {
        let f = fixture("[tui]\ntheme = \"light\"\n", "");
        let mut untrusted = f.open(LoadOptions::default());
        assert!(!untrusted.can_project());
        assert!(untrusted.toggle_scope().unwrap_err().contains("trusted"));
        assert_eq!(untrusted.scope(), Scope::Home);

        let mut store = f.trusted();
        assert_eq!(store.toggle_scope(), Ok(Scope::Project));
        assert_eq!(store.target(), f.paths.project_config());
        let line = store.save(row("tui.theme"), text("gruvbox-dark")).unwrap();
        assert_eq!(line, "tui.theme = gruvbox-dark in the project config");
        assert!(f.project_text().contains("theme = \"gruvbox-dark\""));
        assert!(f.home_text().contains("theme = \"light\""));
        assert_eq!(store.sources("tui.theme"), [&Source::Project, &Source::Home]);
        assert_eq!(f.applied(), [("tui.theme".to_string(), text("gruvbox-dark"))]);

        // The default is written to the project when the home config says something else:
        // taking the key out would let the home value through.
        store.save(row("tui.theme"), text("dark")).unwrap();
        assert!(
            f.project_text().contains("theme = \"dark\""),
            "{}",
            f.project_text()
        );
        assert_eq!(store.value("tui.theme"), Some(&text("dark")));
        assert_eq!(store.toggle_scope(), Ok(Scope::Home));
    }

    #[test]
    fn reset_takes_the_key_out_of_the_file_changes_go_to() {
        let f = fixture(
            "[tui]\ntheme = \"light\"\n[limits]\nmax_turns = 9\n",
            "[tui]\ntheme = \"tokyo-night\"\n",
        );
        let mut store = f.trusted();
        assert_eq!(
            store.reset(row("limits.max_turns")).unwrap(),
            "limits.max_turns reset: 0, the default (applies to the next session)"
        );
        assert!(!f.home_text().contains("max_turns"));
        // Another layer still sets it: the line says who.
        assert_eq!(
            store.reset(row("tui.theme")).unwrap(),
            "tui.theme reset: tokyo-night, set in the project config"
        );
        assert!(!f.home_text().contains("theme"));
        // Nothing of the home config is left to reset.
        assert_eq!(
            store.reset(row("tui.theme")).unwrap_err(),
            "Theme: nothing to reset in the home config; it is set in the project config"
        );
        assert_eq!(
            store.reset(row("tui.glyphs")).unwrap_err(),
            "Symbols: already on its default"
        );
        store.toggle_scope().unwrap();
        store.reset(row("tui.theme")).unwrap();
        assert_eq!(store.value("tui.theme"), Some(&text("dark")));
        assert_eq!(f.applied().last(), Some(&("tui.theme".to_string(), text("dark"))));
    }

    #[test]
    fn a_table_is_edited_one_entry_at_a_time() {
        let f = fixture("# keep\n[tools.format]\npy = \"ruff format\"\n", "");
        let mut store = f.trusted();
        let formatters = row("tools.format");
        assert_eq!(store.entries(formatters), ["py = ruff format"]);

        let (key, value) = parse_entry("rs = rustfmt --edition 2024").unwrap();
        assert_eq!(
            store.set_entry(formatters, &key, value).unwrap(),
            "tools.format.rs = rustfmt --edition 2024 (applies to the next session)"
        );
        assert_eq!(
            store.entries(formatters),
            ["py = ruff format", "rs = rustfmt --edition 2024"]
        );
        let saved = f.home_text();
        assert!(
            saved.starts_with("# keep\n[tools.format]\npy = \"ruff format\"\n"),
            "{saved}"
        );
        assert!(saved.contains("rs = \"rustfmt --edition 2024\""), "{saved}");

        // A value that is not text is TOML, and one the configuration refuses is not written.
        let order = row("accounts.order");
        let (key, value) = parse_entry("anthropic = [\"work\", \"default\"]").unwrap();
        store.set_entry(order, &key, value).unwrap();
        assert_eq!(store.entries(order), ["anthropic = [\"work\", \"default\"]"]);
        let (key, value) = parse_entry("openai = 3").unwrap();
        assert!(
            store
                .set_entry(order, &key, value)
                .unwrap_err()
                .starts_with("Account order: ")
        );
        let servers = row("lsp.servers");
        let (key, value) =
            parse_entry("rust = { command = \"rust-analyzer\", extensions = [\"rs\"] }").unwrap();
        store.set_entry(servers, &key, value).unwrap();
        let loaded: Config = f.home_text().parse::<Table>().unwrap().try_into().unwrap();
        assert_eq!(loaded.lsp.servers["rust"].command, "rust-analyzer");
        assert_eq!(loaded.accounts.order["anthropic"], ["work", "default"]);

        assert_eq!(
            store.remove_entry(formatters, "py").unwrap(),
            "tools.format.py removed"
        );
        assert_eq!(store.entries(formatters), ["rs = rustfmt --edition 2024"]);
        assert!(
            store
                .remove_entry(formatters, "go")
                .unwrap_err()
                .contains("go is not in the home config")
        );
        assert!(parse_entry("no value").is_err());
        assert!(parse_entry(" = x").is_err());
    }

    #[test]
    fn rules_of_other_layers_are_not_copied_into_the_file_being_edited() {
        let f = fixture(
            "[permissions]\nallow = [\"shell(cargo build:*)\"]\n",
            "[permissions]\nallow = [\"shell(cargo test:*)\"]\n",
        );
        let mut store = f.trusted();
        let allow = row("permissions.allow");
        // Both layers apply; the editor holds the home config's own.
        assert_eq!(
            shown(store.value("permissions.allow")),
            "shell(cargo build:*), shell(cargo test:*)"
        );
        assert_eq!(store.entries(allow), ["shell(cargo build:*)"]);
        let rules = Value::Array(vec![text("shell(cargo build:*)"), text("shell(git status)")]);
        assert_eq!(
            store.save(allow, rules).unwrap(),
            "permissions.allow = shell(cargo build:*), shell(git status) (applies to the next session)"
        );
        let home: Table = f.home_text().parse().unwrap();
        assert_eq!(
            lookup(&home, "permissions.allow")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            store
                .value("permissions.allow")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(3)
        );
        // No rules at all takes the key out of the file.
        store.save(allow, Value::Array(Vec::new())).unwrap();
        assert!(!f.home_text().contains("allow"), "{}", f.home_text());
    }

    #[test]
    fn a_change_is_taken_back_when_the_configuration_no_longer_loads() {
        let f = fixture("[tui]\ntheme = \"light\"\n", "");
        let mut store = f.trusted();
        // The project's file is broken by hand while the settings are open.
        std::fs::write(f.paths.project_config(), "[tui\n").unwrap();
        let before = f.home_text();
        let failed = store.save(row("editor.mode"), text("vim")).unwrap_err();
        assert!(failed.starts_with("Keys in the input: "), "{failed}");
        assert!(failed.contains("config.toml"), "{failed}");
        assert_eq!(f.home_text(), before);
        assert!(f.applied().is_empty());
        assert!(f.changes.lock().unwrap().is_empty());
    }

    #[test]
    fn typed_values_are_parsed_by_kind_and_checked_against_the_configuration() {
        let float = Value::Float(0.0);
        assert_eq!(parse_value(Kind::Switch, "on", None), Ok(Value::Boolean(true)));
        assert!(parse_value(Kind::Switch, "maybe", None).is_err());
        assert_eq!(
            parse_value(Kind::Number, " 2.5 ", Some(&float)),
            Ok(Value::Float(2.5))
        );
        assert_eq!(parse_value(Kind::Number, "12", None), Ok(Value::Integer(12)));
        assert_eq!(
            parse_value(Kind::Number, "5000000000", None),
            Ok(Value::Integer(5_000_000_000))
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
            Ok(Value::Array(vec![text("anthropic/a"), text("@slow")]))
        );
        assert_eq!(parse_value(Kind::Theme, " zenburn ", None), Ok(text("zenburn")));

        assert!(validate("editor.mode", &text("vim")).is_ok());
        assert!(validate("editor.mode", &text("nano")).is_err());
        assert!(
            validate(
                "advisor.model",
                &parse_value(Kind::List, "not a model", None).unwrap()
            )
            .is_err()
        );
        assert!(validate("limits.max_turns", &Value::Integer(5)).is_ok());
    }

    #[test]
    fn a_value_reads_back_as_it_is_shown_for_typing() {
        for value in [
            text("rustfmt --edition 2024"),
            text("true"),
            text("3"),
            text(" padded "),
            text(""),
            Value::Integer(3),
            Value::Boolean(true),
            Value::Array(vec![text("a"), text("b c")]),
            toml::from_str::<Table>("v = { style = \"short\", width = 3 }").unwrap()["v"].clone(),
        ] {
            let typed = entry_text("key", &value);
            assert_eq!(parse_entry(&typed), Ok(("key".to_string(), value)), "{typed}");
        }
        assert_eq!(entry_text("rs", &text("rustfmt")), "rs = rustfmt");
        assert_eq!(entry_text("n", &text("3")), "n = \"3\"");
    }
}
