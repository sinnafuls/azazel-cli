//! The commands of RPC mode that report and change how azazel is set up: the settings, the theme,
//! what a request is made of, what the session cost, and what the project and the permissions
//! allow. They are the settings screen and the reports of the terminal as data, so that a client
//! that is not a terminal (the page of `azazel remote`) can offer the same.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use azazel_agent::AgentHandle;
use azazel_core::SessionStore;
use azazel_core::cache::{CacheLedger, WARMING_PURPOSE};
use azazel_core::settings::{Kind, Page, SETTINGS, Setting, setting};
use azazel_core::side::Side;
use azazel_core::trust::TrustStore;
use azazel_protocol::{
    ContextCategory, ContextReport, CostReport, DirList, Forgotten, ModelSpend, SaveTo, SettingChanged,
    SettingList, SettingRow, SideSpend, ThemeColors, TrustState,
};
use serde_json::Value;

use crate::app::App;
use crate::settings::LiveSettings;
use crate::settings::store::{Changes, Store, shown, validate};

/// What the session changed with `scope: session`: kept in memory, shown over the files.
#[derive(Default)]
pub(crate) struct SessionSettings(Mutex<BTreeMap<String, toml::Value>>);

impl SessionSettings {
    fn get(&self, path: &str) -> Option<toml::Value> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .cloned()
    }

    fn set(&self, path: &str, value: Option<toml::Value>) {
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match value {
            Some(value) => all.insert(path.to_string(), value),
            None => all.remove(path),
        };
    }
}

/// The session's changes, registered when the app is built (`App::build`).
fn overlay(app: &App) -> Arc<SessionSettings> {
    app.registry.services.get::<SessionSettings>().unwrap_or_default()
}

fn open(app: &App) -> Result<Store, String> {
    Store::open(
        app.paths.clone(),
        app.load_options.clone(),
        app.registry.services.get::<LiveSettings>(),
        Some(app.router.clone()),
        Changes::default(),
    )
    .map_err(|why| format!("the configuration does not load: {why}"))
}

fn kind_of(kind: Kind) -> (&'static str, Vec<String>) {
    match kind {
        Kind::Switch => ("switch", Vec::new()),
        Kind::Choice(choices) => ("choice", choices.iter().map(|c| c.to_string()).collect()),
        Kind::Theme => ("theme", Vec::new()),
        Kind::Number => ("number", Vec::new()),
        Kind::Text => ("text", Vec::new()),
        Kind::List => ("list", Vec::new()),
        Kind::Table => ("table", Vec::new()),
        Kind::Role => ("role", Vec::new()),
    }
}

fn json_of(value: Option<&toml::Value>) -> Value {
    value
        .and_then(|value| serde_json::to_value(value).ok())
        .unwrap_or(Value::Null)
}

/// `applies`: whether this session applies live settings as they are set (only a terminal UI
/// does; any other session applies the theme alone), so `live` never offers what is refused.
fn row(
    store: &Store,
    over: &SessionSettings,
    setting: &Setting,
    themes: &[String],
    applies: bool,
) -> SettingRow {
    let (kind, mut choices) = kind_of(setting.kind);
    if setting.kind == Kind::Theme {
        choices = themes.to_vec();
    }
    let own = over.get(setting.path);
    let source = match &own {
        Some(_) => Some("session".to_string()),
        None => store
            .sources(setting.path)
            .first()
            .map(|source| source.word().to_string()),
    };
    SettingRow {
        path: setting.path.to_string(),
        page: setting.page.title().to_string(),
        group: setting.group.to_string(),
        label: setting.label.to_string(),
        kind: kind.to_string(),
        choices,
        live: setting.live && (applies || setting.path == "tui.theme"),
        description: setting.description.to_string(),
        value: json_of(own.as_ref().or(store.value(setting.path))),
        default: json_of(setting.default_value().as_ref()),
        source,
        gated: azazel_core::config::is_trust_gated(setting.path),
    }
}

/// `get_settings`: every setting of the settings screen with its value and who set it.
pub(crate) fn get_settings(app: &App) -> Result<SettingList, String> {
    let store = open(app)?;
    let over = overlay(app);
    let themes = crate::settings::theme_names(&app.paths.home);
    let applies = app.registry.services.get::<LiveSettings>().is_some();
    Ok(SettingList {
        settings: SETTINGS
            .iter()
            .map(|setting| row(&store, &over, setting, &themes, applies))
            .collect(),
        pages: Page::ALL.iter().map(|page| page.title().to_string()).collect(),
        project: store.can_project(),
        home_config: store.home_config().display().to_string(),
        project_config: app.paths.project_config().display().to_string(),
    })
}

/// `set_setting`: changes one setting where `scope` says. `null` takes the setting out of the
/// place the scope names.
pub(crate) async fn set_setting(
    app: &App,
    path: &str,
    value: Value,
    scope: SaveTo,
) -> Result<SettingChanged, String> {
    let setting =
        setting(path).ok_or_else(|| format!("`{path}` is not a setting; get_settings lists them"))?;
    if setting.kind == Kind::Role {
        return Err("a model role is changed with set_role".to_string());
    }
    let value = match value {
        Value::Null => None,
        value => Some(toml::Value::try_from(value).map_err(|e| format!("{}: {e}", setting.label))?),
    };
    let over = overlay(app);
    let message = match scope {
        SaveTo::Session => {
            if !setting.live {
                return Err(format!(
                    "{} applies to the next session: save it to the home or the project config",
                    setting.label
                ));
            }
            // Only a terminal UI applies a live setting as it is set; any other session has the
            // theme, which clients read through `get_theme` and the `theme` frame, and nothing
            // else. Saying a change was made for the session would be untrue.
            let live = app.registry.services.get::<LiveSettings>();
            if live.is_none() && path != "tui.theme" {
                return Err(format!(
                    "{} cannot be changed for just this session here: only the theme can; \
                     save it to the home or the project config for the next session",
                    setting.label
                ));
            }
            if let Some(value) = &value {
                validate(path, value).map_err(|why| format!("{}: {why}", setting.label))?;
                // A theme is read from a file when it is not a built-in name: a path a client
                // chose must never be read by the session.
                if setting.kind == Kind::Theme {
                    session_theme(app, value)?;
                }
            }
            // What the configuration says now, for a value that is taken back.
            let now = match &value {
                Some(value) => value.clone(),
                None => open(app)?
                    .value(path)
                    .cloned()
                    .ok_or_else(|| format!("{}: nothing is set", setting.label))?,
            };
            over.set(path, value.clone());
            if let Some(live) = live {
                (live.0)(path, &now);
            }
            match value {
                Some(_) => format!("{path} = {} for this session", shown(Some(&now))),
                None => format!("{path} is back on {} for this session", shown(Some(&now))),
            }
        }
        SaveTo::Home | SaveTo::Project => {
            let mut store = open(app)?;
            if scope == SaveTo::Project {
                store.toggle_scope()?;
            }
            let line = match value {
                Some(value) => store.save(setting, value)?,
                None => store.reset(setting)?,
            };
            // The file is what counts now; a session value would hide it.
            over.set(path, None);
            azazel_core::ext::config_changed(&app.registry.services, "settings").await;
            line
        }
    };
    let store = open(app)?;
    let themes = crate::settings::theme_names(&app.paths.home);
    let applies = app.registry.services.get::<LiveSettings>().is_some();
    Ok(SettingChanged {
        setting: row(&store, &over, setting, &themes, applies),
        message,
    })
}

/// Whether `value` names a theme a session may switch to: `auto`, a built-in theme or one in the
/// home's `themes` directory, by name. A path is not a name, and the session reads no file a
/// client points it at.
fn session_theme(app: &App, value: &toml::Value) -> Result<(), String> {
    let name = value.as_str().ok_or("Theme: a theme is a name")?;
    let known = name == "auto"
        || crate::settings::theme_names(&app.paths.home)
            .iter()
            .any(|n| n == name);
    match known {
        true => Ok(()),
        false => Err(format!(
            "no theme named `{name}`; the themes are listed in the setting's choices"
        )),
    }
}

/// `get_theme`: the colours of the configured theme as RGB. The theme is the one in effect,
/// which a session change of `tui.theme` may have moved from the file's.
pub(crate) fn get_theme(app: &App, light: Option<bool>) -> Result<ThemeColors, String> {
    let name = match overlay(app).get("tui.theme") {
        Some(toml::Value::String(name)) => name,
        _ => match open(app)?.value("tui.theme") {
            Some(toml::Value::String(name)) => name.clone(),
            _ => app.config.tui.theme.clone(),
        },
    };
    let (theme, _) = crate::tui::named_theme(&app.paths, &name, light);
    Ok(ThemeColors {
        light: theme.is_light(),
        roles: theme
            .rgb_roles()
            .into_iter()
            .map(|(role, (r, g, b))| (role.name().to_string(), format!("#{r:02x}{g:02x}{b:02x}")))
            .collect(),
        name,
    })
}

/// `get_context`: the last request of the lead by category, or of the session's file when this
/// process has sent none; `None` before any.
pub(crate) fn get_context(lead: &AgentHandle, session: &SessionStore) -> Option<ContextReport> {
    let report = lead
        .last_request()
        .map(|record| record.report())
        .or_else(|| SessionStore::last_request(&session.path()))?;
    let categories = report
        .categories
        .iter()
        .map(|category| ContextCategory {
            kind: serde_json::to_value(category.kind)
                .ok()
                .and_then(|kind| kind.as_str().map(str::to_string))
                .unwrap_or_default(),
            name: category.name.clone(),
            label: crate::context::label(category),
            items: category.items as u64,
            estimate: category.estimate,
            tokens: category.tokens,
        })
        .collect();
    Some(ContextReport {
        agent: report.agent,
        request: report.request,
        model: report.model,
        window: report.window,
        estimate: report.estimate,
        reported: report.reported,
        tokens: report.tokens,
        categories,
    })
}

/// `get_cost`: what `/cost` says, as data.
pub(crate) fn get_cost(app: &App, lead: &AgentHandle, session: &SessionStore) -> CostReport {
    let status = lead.status();
    let note = app.router.tier().map(crate::routing::tier_cost_note);
    let cache = app
        .registry
        .services
        .get::<CacheLedger>()
        .map(|ledger| ledger.report(&session.id()))
        .unwrap_or_default();
    let warming = cache.warming.as_ref().map(|warming| SideSpend {
        purpose: WARMING_PURPOSE.to_string(),
        usage: super::session::usage(&warming.usage),
    });
    let side = app
        .registry
        .services
        .get::<Side>()
        .map(|side| side.costs.by_purpose())
        .unwrap_or_default()
        .into_iter()
        .filter(|(purpose, _)| warming.is_none() || purpose != WARMING_PURPOSE)
        .map(|(purpose, usage)| SideSpend {
            purpose,
            usage: super::session::usage(&usage),
        })
        .collect();
    CostReport {
        requests: status.requests,
        usage: super::session::usage(&status.usage),
        note,
        cache_hit_rate: cache.session().hit_rate(),
        models: cache
            .models
            .iter()
            .map(|(model, totals)| ModelSpend {
                model: model.clone(),
                requests: totals.requests,
                input: totals.input,
                cache_read: totals.cache_read,
                cache_write: totals.cache_write,
                hit_rate: totals.hit_rate(),
            })
            .collect(),
        side,
        warming,
    }
}

/// `trust_project`: decides as `/trust yes` or `/trust no` does.
pub(crate) fn trust_project(app: &App, trusted: bool) -> Result<TrustState, String> {
    let message = crate::reports::trust(app, if trusted { "yes" } else { "no" })?;
    Ok(TrustState {
        root: app.paths.project_root.display().to_string(),
        trusted: app.trusted,
        saved: TrustStore::new(app.paths.trust_file()).decision(&app.paths.project_root),
        message,
    })
}

/// `add_dir`: as `/add-dir`.
pub(crate) fn add_dir(app: &App, path: &str) -> Result<DirList, String> {
    let message = crate::reports::add_dir(app, path.trim())?;
    Ok(DirList {
        dirs: app
            .permissions
            .roots()
            .iter()
            .map(|root| root.display().to_string())
            .collect(),
        message,
    })
}

/// `forget_approvals`: as `/forget-approvals`.
pub(crate) fn forget_approvals(app: &App) -> Result<Forgotten, String> {
    let (count, message) = crate::reports::forget_approvals_counted(app)?;
    Ok(Forgotten {
        count: count as u64,
        message,
    })
}
