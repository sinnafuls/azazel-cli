//! `/settings`: signing in with a subscription, storing API keys, choosing models, signing out
//! and changing every setting, all from inside the terminal UI, so that neither a first run nor
//! a change of taste needs config editing.
//!
//! In the terminal UI it is one screen of pages behind a row of tabs ([`screen`]). Where there
//! is no such screen (a session in another process, an RPC client) the same pages are a chain
//! of prompts.

mod screen;
mod store;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::login::{forget_key, sign_in_from_ui, store_key};
use async_trait::async_trait;
use azazel_ai::catalog::Cost;
use azazel_ai::{Credentials, ModelSpec, ProviderConfig, Router};
use azazel_core::settings::{Kind, Page, SETTINGS, Setting};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Config, ExtCtx, ExtError, Extension, Level, LoadOptions, Paths,
    PermissionMode, Registrar, Ui, UiError,
};
use azazel_tui::Theme;
use tokio::sync::oneshot;

pub(crate) use self::screen::Screen;
use self::screen::{Accounts, Request};
use self::store::{Changes, Store, chain_label, parse_entry, parse_value, role_name, shown};

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
    paths: Paths,
    /// How the configuration was loaded: trust, the environment, the command line.
    options: LoadOptions,
}

/// How the settings reach the running UI: called with the path and the new value of a
/// setting that applies at once. The terminal UI registers it as a service.
pub struct LiveSettings(pub Arc<ApplySetting>);

/// Called with a setting's path (`editor.mode`) and its new value.
pub type ApplySetting = dyn Fn(&str, &toml::Value) + Send + Sync;

/// How the settings screen gets onto the terminal: called with the screen, it shows it and
/// gives it back once the screen has given up the keyboard. The terminal UI registers it as a
/// service; without it the settings are a chain of prompts.
pub(crate) struct SettingsHost(pub Arc<ShowScreen>);

pub(crate) type ShowScreen = dyn Fn(Box<Screen>) -> oneshot::Receiver<Box<Screen>> + Send + Sync;

/// The themes there are to choose from: the built-in ones, then the files in `themes` under
/// `home` (`~/.azazel`) by name.
pub(crate) fn theme_names(home: &Path) -> Vec<String> {
    // `auto` first: it is one of the built-in two, whichever suits the terminal.
    let mut names = vec![crate::tui::AUTO_THEME.to_string()];
    names.extend(Theme::BUILTINS.iter().map(ToString::to_string));
    let mut custom: Vec<String> = std::fs::read_dir(home.join("themes"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "toml"))
        .filter_map(|path| Some(path.file_stem()?.to_string_lossy().into_owned()))
        .filter(|name| !Theme::BUILTINS.contains(&name.as_str()) && name != crate::tui::AUTO_THEME)
        .collect();
    custom.sort();
    names.extend(custom);
    names
}

/// A row of the menu the chain of prompts starts with.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Item {
    Page(Page),
    Done,
}

/// A row of a page in the chain of prompts.
#[derive(Clone, Copy)]
enum Entry {
    Run(Request),
    Edit(&'static Setting),
    Back,
}

impl Settings {
    /// Who the providers' credentials come from, one line per provider that has one.
    fn signed_in(&self) -> Vec<(String, String)> {
        self.providers
            .iter()
            .filter(|p| p.requires_key)
            .filter_map(|p| self.credentials.source(p).map(|source| (p.id.clone(), source)))
            .collect()
    }

    /// What the Accounts page shows.
    fn accounts(&self) -> Accounts {
        Accounts {
            signed_in: self.signed_in(),
            stored: !self.credentials.load().is_empty(),
        }
    }

    fn default_model(&self, router: Option<&Router>) -> Option<String> {
        let chain = router?.roles().get("default").cloned()?;
        (!chain.is_empty()).then(|| chain_label(&chain))
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
        let line = sign_in_from_ui(&self.credentials, provider, settings, &Default::default(), ui.clone(), None).await?;
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
    /// model of `role` for this session and later ones.
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
        let home_config = self.paths.home_config();
        crate::init::write_role(&home_config, role, Some(&model)).map_err(|e| format!("{e:#}"))?;
        router.set_role(role, vec![spec]);
        let what = match role {
            "default" => "the default model".to_string(),
            role => format!("the {role} model"),
        };
        Ok(Some(format!(
            "{what} is now {model} (saved in {})",
            home_config.display()
        )))
    }

    /// Gives a role a model, or puts it back on the role it falls back to. The default role
    /// has nothing to fall back to: it goes straight to the models.
    async fn role(&self, ui: &Arc<Ui>, router: &Router, role: &str) -> Result<Option<String>, String> {
        let Some(fallback) = Config::default().roles.get(role).cloned() else {
            return self.model_for(ui, router, None, role).await;
        };
        let actions = [
            "Choose a model".to_string(),
            format!("Use the fallback\t{}", chain_label(&fallback)),
        ];
        let Ok(action) = ui.0.select(&format!("Model for {role}"), &actions).await else {
            return Ok(None);
        };
        if action != 1 {
            return self.model_for(ui, router, None, role).await;
        }
        crate::init::write_role(&self.paths.home_config(), role, None).map_err(|e| format!("{e:#}"))?;
        let label = chain_label(&fallback);
        router.set_role(role, fallback);
        Ok(Some(format!("{role} now follows {label}")))
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

    /// Runs what a row of the Accounts or the Models page stands for: its prompts, and what
    /// they led to.
    async fn flow(
        &self,
        request: Request,
        ui: &Arc<Ui>,
        router: Option<&Router>,
    ) -> Result<Option<String>, String> {
        match request {
            Request::Close => Ok(None),
            Request::Subscription => self.subscription(ui).await,
            Request::ApiKey => self.api_key(ui).await,
            Request::SignOut => self.sign_out(ui).await,
            Request::Role(setting) => match router {
                Some(router) => self.role(ui, router, role_name(setting)).await,
                None => Err("no model router in this session".to_string()),
            },
        }
    }

    /// Says what a sign-in, a key or a choice of model came to and notes it for the summary.
    /// A first credential with no default model yet goes straight on to choosing one of that
    /// provider's models. Returns the last line, for the screen to show.
    async fn settle(
        &self,
        outcome: Result<Option<String>, String>,
        ui: &Arc<Ui>,
        router: Option<&Router>,
        store: &Store,
    ) -> Option<(Level, String)> {
        let line = match outcome {
            Ok(Some(line)) => line,
            Ok(None) => return None,
            Err(why) => {
                ui.0.notify(Level::Error, &why);
                return Some((Level::Error, why));
            }
        };
        let (mut line, provider) = split_provider(line);
        ui.0.notify(Level::Info, &line);
        store.note(line.clone());
        if let Some(router) = router
            && !provider.is_empty()
            && self.default_model(Some(router)).is_none()
        {
            match self.model_for(ui, router, Some(&provider), "default").await {
                Ok(Some(chosen)) => {
                    ui.0.notify(Level::Info, &chosen);
                    store.note(chosen.clone());
                    line = chosen;
                }
                Ok(None) => {}
                Err(why) => {
                    ui.0.notify(Level::Error, &why);
                    return Some((Level::Error, why));
                }
            }
        }
        Some((Level::Info, line))
    }

    /// The settings as one screen: it is shown until it gives up the keyboard, which is when it
    /// is closed or when a row needs prompts of its own; after those it opens again where it
    /// was.
    async fn paged(&self, host: &SettingsHost, store: Store, ui: &Arc<Ui>, router: Option<&Router>) {
        let mut screen = Box::new(Screen::new(store, theme_names(&self.paths.home)));
        loop {
            screen.refresh(self.accounts());
            // No answer: the UI went away with the screen.
            let Ok(back) = (host.0)(screen).await else {
                return;
            };
            screen = back;
            let request = screen.take_request();
            if request == Request::Close {
                return;
            }
            let outcome = self.flow(request, ui, router).await;
            if let Some((level, line)) = self.settle(outcome, ui, router, screen.store()).await {
                screen.say(level, &line);
            }
        }
    }

    /// The first menu of the chain of prompts: the pages.
    fn menu(&self, store: &Store, router: Option<&Router>) -> (String, Vec<(Item, String)>) {
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
        let mut items: Vec<(Item, String)> = Page::ALL
            .into_iter()
            .map(|page| {
                let changed = SETTINGS
                    .iter()
                    .filter(|setting| setting.page == page && store.changed(setting))
                    .count();
                let detail = match changed {
                    0 => page.lead().to_string(),
                    changed => format!("{changed} changed · {}", page.lead()),
                };
                (Item::Page(page), format!("{}\t{detail}", page.title()))
            })
            .collect();
        items.push((Item::Done, "Done".to_string()));
        (heading, items)
    }

    /// The rows of a page in the chain of prompts: what the Accounts page can do, then the
    /// settings with their values.
    fn entries(&self, page: Page, store: &Store) -> Vec<(Entry, String)> {
        let mut entries = Vec::new();
        if page == Page::Accounts {
            entries.push((
                Entry::Run(Request::Subscription),
                "Sign in with a subscription\tClaude, ChatGPT".to_string(),
            ));
            entries.push((Entry::Run(Request::ApiKey), "Add an API key".to_string()));
            if !self.credentials.load().is_empty() {
                entries.push((
                    Entry::Run(Request::SignOut),
                    "Sign out or forget a key".to_string(),
                ));
            }
        }
        for setting in SETTINGS.iter().filter(|setting| setting.page == page) {
            let marker = match store.changed(setting) {
                true => " *",
                false => "",
            };
            let label = match setting.kind {
                Kind::Role => format!("{} model", setting.label),
                _ => setting.label.to_string(),
            };
            entries.push((
                Entry::Edit(setting),
                format!("{label}\t{}{marker}", store.shown(setting)),
            ));
        }
        entries.push((Entry::Back, "Back".to_string()));
        entries
    }

    /// Asks for a new value of a setting and saves it. A list is typed with commas between
    /// its entries; a table one entry at a time, as `name = value`, and `name =` removes one.
    async fn ask(
        &self,
        setting: &'static Setting,
        store: &mut Store,
        ui: &Arc<Ui>,
    ) -> Option<Result<String, String>> {
        let now = shown(store.value(setting.path));
        let typed = match setting.kind {
            Kind::Role => return None,
            Kind::Switch => match store.value(setting.path) {
                Some(toml::Value::Boolean(true)) => "off".to_string(),
                _ => "on".to_string(),
            },
            Kind::Choice(options) => {
                let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
                let chosen = ui.0.select(setting.label, &options).await.ok()?;
                options.get(chosen)?.clone()
            }
            Kind::Theme => {
                let options = theme_names(&self.paths.home);
                let chosen = ui.0.select(setting.label, &options).await.ok()?;
                options.get(chosen)?.clone()
            }
            Kind::Number | Kind::Text => {
                let prompt = format!("{} (now: {now})\n{}", setting.label, setting.description);
                ui.0.input(&prompt).await.ok()?
            }
            Kind::List => {
                let prompt = format!(
                    "{} (now: {now})\n{} Several with commas between them.",
                    setting.label, setting.description
                );
                ui.0.input(&prompt).await.ok()?
            }
            Kind::Table => {
                let prompt = format!(
                    "{} (now: {now})\n{} Type name = value to set an entry, name = to remove one.",
                    setting.label, setting.description
                );
                let typed = ui.0.input(&prompt).await.ok()?;
                if typed.trim().is_empty() {
                    return None;
                }
                let fail = |why: String| format!("{}: {why}", setting.label);
                return Some(match parse_entry(&typed).map_err(fail) {
                    Ok((key, toml::Value::String(text))) if text.is_empty() => {
                        store.remove_entry(setting, &key)
                    }
                    Ok((key, value)) => store.set_entry(setting, &key, value),
                    Err(why) => Err(why),
                });
            }
        };
        // Bypass as the mode every session starts in is confirmed, with "No" first.
        if setting.path == "permissions.mode" && typed == PermissionMode::Bypass.as_str() {
            let question = "Start every session in bypass mode?\nNothing is asked about and no \
                            safety guard applies: only your deny rules and the push guard are left.";
            let answers = ["No".to_string(), "Yes, bypass permissions".to_string()];
            if ui.0.select(question, &answers).await.ok() != Some(1) {
                return None;
            }
        }
        let default = setting.default_value();
        Some(
            parse_value(setting.kind, &typed, default.as_ref())
                .map_err(|why| format!("{}: {why}", setting.label))
                .and_then(|value| store.save(setting, value)),
        )
    }

    /// The settings as a chain of prompts, for a UI that has no screen for them: the pages,
    /// then the rows of one, then the value of one. Each change is saved as it is made.
    async fn chain(&self, mut store: Store, ui: &Arc<Ui>, router: Option<&Router>) {
        loop {
            let (heading, items) = self.menu(&store, router);
            let labels: Vec<String> = items.iter().map(|(_, label)| label.clone()).collect();
            let page = match ui.0.select(&heading, &labels).await {
                Ok(index) => match items[index].0 {
                    Item::Page(page) => page,
                    Item::Done => return,
                },
                Err(UiError::Cancelled | UiError::NoUser) => return,
            };
            loop {
                let entries = self.entries(page, &store);
                let labels: Vec<String> = entries.iter().map(|(_, label)| label.clone()).collect();
                let heading = format!(
                    "{}\n{} Saved in {}.",
                    page.title(),
                    page.lead(),
                    store.home_config().display()
                );
                let Ok(index) = ui.0.select(&heading, &labels).await else {
                    break;
                };
                let request = match entries[index].0 {
                    Entry::Back => break,
                    Entry::Run(request) => request,
                    Entry::Edit(setting) if setting.kind == Kind::Role => Request::Role(setting),
                    Entry::Edit(setting) => {
                        match self.ask(setting, &mut store, ui).await {
                            Some(Ok(line)) => ui.0.notify(Level::Info, &line),
                            Some(Err(why)) => ui.0.notify(Level::Error, &why),
                            None => {}
                        }
                        continue;
                    }
                };
                let outcome = self.flow(request, ui, router).await;
                self.settle(outcome, ui, router, &store).await;
                if let Err(why) = store.reload() {
                    ui.0.notify(Level::Error, &why);
                }
            }
        }
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
        let changes = Changes::default();
        let store = Store::open(
            self.paths.clone(),
            self.options.clone(),
            ctx.services.get::<LiveSettings>(),
            router.clone(),
            changes.clone(),
        );
        let store = match store {
            Ok(store) => store,
            Err(why) => return CommandOutcome::Error(format!("the configuration does not load: {why}")),
        };
        match ctx.services.get::<SettingsHost>() {
            Some(host) => self.paged(&host, store, &ui, router.as_deref()).await,
            None => self.chain(store, &ui, router.as_deref()).await,
        }
        let done = std::mem::take(&mut *changes.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
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
    pub paths: Paths,
    /// How this run loaded its configuration: the settings read the layers the same way.
    pub options: LoadOptions,
}

impl Extension for SettingsExt {
    fn id(&self) -> &str {
        "settings"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(Command {
            name: "settings".to_string(),
            description: "Sign in, add API keys, choose models and change every setting".to_string(),
            argument_hint: None,
            handler: Arc::new(Settings {
                providers: self.providers.clone(),
                credentials: self.credentials.clone(),
                paths: self.paths.clone(),
                options: self.options.clone(),
            }),
        });
        Ok(())
    }
}
#[cfg(test)]
pub(crate) use self::store::tests::{Fixture, fixture};

/// A settings screen over the config files of a fixture, for tests of the box it is drawn in.
#[cfg(test)]
pub(crate) fn screen_on(fixture: &Fixture) -> Box<Screen> {
    Box::new(Screen::new(fixture.trusted(), theme_names(&fixture.paths.home)))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use azazel_ai::providers::presets;
    use azazel_core::{ApprovalDecision, ApprovalRequest, Question, UiBridge};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    fn themes_to_choose_from_are_auto_the_built_in_ones_then_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let names = theme_names(dir.path());
        assert_eq!(names[0], "auto");
        assert_eq!(&names[1..], Theme::BUILTINS);
        let themes = dir.path().join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(themes.join("zenburn.toml"), "accent = \"#dcdccc\"\n").unwrap();
        std::fs::write(themes.join("amber.toml"), "accent = \"#ffb000\"\n").unwrap();
        // A file named like a built-in theme, or like `auto`, is not a second entry; other
        // files are not themes.
        std::fs::write(themes.join("dark.toml"), "accent = \"#ffffff\"\n").unwrap();
        std::fs::write(themes.join("auto.toml"), "accent = \"#ffffff\"\n").unwrap();
        std::fs::write(themes.join("notes.txt"), "not a theme").unwrap();
        let names = theme_names(dir.path());
        assert_eq!(&names[1..=Theme::BUILTINS.len()], Theme::BUILTINS);
        assert_eq!(&names[Theme::BUILTINS.len() + 1..], ["amber", "zenburn"]);
    }

    #[test]
    fn the_settings_that_apply_at_once_are_the_ones_the_ui_knows_how_to_apply() {
        let live: Vec<&str> = SETTINGS
            .iter()
            .filter(|setting| setting.live)
            .map(|setting| setting.path)
            .collect();
        assert_eq!(
            live,
            [
                "tui.favourites",
                "tui.theme",
                "tui.glyphs",
                "tui.indicator",
                "tui.session_accent",
                "tui.tool_style",
                "tui.redact",
                "tui.popups",
                "tui.show_thinking",
                "tui.expand_tools",
                "statusline.preset",
                "statusline.left",
                "statusline.right",
                "statusline.separator",
                "statusline.segments",
                "statusline.command",
                "editor.mode",
                "editor.command",
                "editor.fold_paste_lines",
                "tui.queue_mode",
            ]
        );
    }

    /// What the person at a UI without a settings screen does, prompt by prompt.
    enum Answer {
        /// Picks the row that starts with this.
        Pick(&'static str),
        Type(&'static str),
    }

    #[derive(Default)]
    struct Scripted {
        answers: Mutex<VecDeque<Answer>>,
        /// Every list that was shown: its prompt and its rows.
        lists: Mutex<Vec<(String, Vec<String>)>>,
        notes: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: Vec<Answer>) -> Arc<Scripted> {
            Arc::new(Scripted {
                answers: Mutex::new(answers.into()),
                ..Scripted::default()
            })
        }

        fn typed(&self) -> Result<String, UiError> {
            match self.answers.lock().unwrap().pop_front() {
                Some(Answer::Type(text)) => Ok(text.to_string()),
                Some(Answer::Pick(row)) => panic!("a text was asked for where {row} was to be picked"),
                None => Err(UiError::Cancelled),
            }
        }
    }

    #[async_trait]
    impl UiBridge for Scripted {
        async fn ask(&self, _: &Question) -> Result<Vec<String>, UiError> {
            Err(UiError::NoUser)
        }

        async fn confirm(&self, _: &str) -> Result<bool, UiError> {
            Err(UiError::NoUser)
        }

        async fn select(&self, prompt: &str, options: &[String]) -> Result<usize, UiError> {
            self.lists
                .lock()
                .unwrap()
                .push((prompt.to_string(), options.to_vec()));
            match self.answers.lock().unwrap().pop_front() {
                Some(Answer::Pick(row)) => Ok(options
                    .iter()
                    .position(|option| option.starts_with(row))
                    .unwrap_or_else(|| panic!("{row} is not among {options:?}"))),
                Some(Answer::Type(text)) => panic!("a list was shown where {text} was to be typed"),
                None => Err(UiError::Cancelled),
            }
        }

        async fn input(&self, _: &str) -> Result<String, UiError> {
            self.typed()
        }

        fn notify(&self, _: Level, text: &str) {
            self.notes.lock().unwrap().push(text.to_string());
        }

        async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
            None
        }
    }

    fn handler(fixture: &Fixture) -> Settings {
        Settings {
            providers: presets::presets_with(&|_| None),
            credentials: Arc::new(Credentials::with_env(
                fixture.paths.auth_file(),
                Arc::new(|_: &str| None),
            )),
            paths: fixture.paths.clone(),
            options: LoadOptions {
                trusted: true,
                ..LoadOptions::default()
            },
        }
    }

    fn ctx(ui: Arc<Scripted>) -> ExtCtx {
        let services = azazel_agent::Services::new();
        services.insert(Arc::new(Ui(ui)));
        ExtCtx {
            services: Arc::new(services),
            cwd: std::path::PathBuf::new(),
            session_id: String::new(),
        }
    }

    #[tokio::test]
    async fn without_a_screen_the_same_pages_are_a_chain_of_prompts() {
        let f = fixture("# mine\n[tui]\ntheme = \"light\"\n", "");
        let ui = Scripted::new(vec![
            Answer::Pick("Appearance"),
            Answer::Pick("Show the model's thinking"),
            Answer::Pick("Theme"),
            Answer::Pick("tokyo-night"),
            Answer::Pick("Back"),
            Answer::Pick("Tools"),
            Answer::Pick("Formatters"),
            Answer::Type("rs = rustfmt --edition 2024"),
            Answer::Pick("Disabled tools"),
            Answer::Type("browser, pty"),
            Answer::Pick("Largest tool result"),
            Answer::Type("lots"),
            Answer::Pick("Back"),
            Answer::Pick("Permissions"),
            Answer::Pick("Mode a session starts in"),
            Answer::Pick("bypass"),
            Answer::Pick("No"),
            Answer::Pick("Back"),
            Answer::Pick("Done"),
        ]);
        let outcome = handler(&f).run("", &ctx(ui.clone())).await;
        let CommandOutcome::Show(summary) = outcome else {
            panic!("no summary");
        };
        assert_eq!(
            summary,
            "settings changed:\n\
             - tui.show_thinking = on\n\
             - tui.theme = tokyo-night\n\
             - tools.format.rs = rustfmt --edition 2024 (applies to the next session)\n\
             - tools.disabled = browser, pty (applies to the next session)"
        );
        let saved = f.home_text();
        assert!(
            saved.starts_with("# mine\n[tui]\ntheme = \"tokyo-night\"\n"),
            "{saved}"
        );
        let loaded: Config = saved.parse::<toml::Table>().unwrap().try_into().unwrap();
        assert!(loaded.tui.show_thinking);
        assert_eq!(loaded.tools.format["rs"], "rustfmt --edition 2024");
        assert_eq!(loaded.tools.disabled, ["browser", "pty"]);
        // What was refused, and the mode that was not confirmed, changed nothing.
        assert_eq!(loaded.tools.max_result_bytes, 50_000);
        assert_eq!(loaded.permissions.mode, PermissionMode::Manual);
        assert!(
            ui.notes
                .lock()
                .unwrap()
                .contains(&"Largest tool result: expected a whole number that is not negative".to_string())
        );

        let lists = ui.lists.lock().unwrap();
        // The first menu is the pages, each with what is on it.
        let (heading, pages) = &lists[0];
        assert_eq!(
            heading,
            "Settings\nno provider has a credential yet · default model: none"
        );
        let titles: Vec<&str> = pages.iter().map(|row| row.split('\t').next().unwrap()).collect();
        let mut expected: Vec<&str> = Page::ALL.iter().map(|page| page.title()).collect();
        expected.push("Done");
        assert_eq!(titles, expected);
        assert!(pages[2].starts_with("Appearance\t1 changed · "), "{}", pages[2]);
        // A page lists its settings with their values; what is not the default is marked.
        let (heading, rows) = &lists[1];
        assert!(heading.starts_with("Appearance\nColours, symbols"), "{heading}");
        assert_eq!(rows[0], "Theme\tlight *");
        assert_eq!(rows[1], "Symbols\tauto");
        assert_eq!(rows.last().unwrap(), "Back");
    }

    #[tokio::test]
    async fn the_screen_comes_back_after_a_key_was_stored_and_says_what_changed_when_it_closes() {
        let f = fixture("", "");
        let ui = Scripted::new(vec![Answer::Pick("openai"), Answer::Type("sk-test-key")]);
        let context = ctx(ui.clone());
        let key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
        // What the person at the terminal presses each time the screen is shown.
        let mut visits: VecDeque<Vec<KeyEvent>> = VecDeque::from([
            vec![key(KeyCode::Down), key(KeyCode::Enter)],
            vec![
                key(KeyCode::Char('/')),
                key(KeyCode::Char('b')),
                key(KeyCode::Char('e')),
                key(KeyCode::Char('l')),
                key(KeyCode::Char('l')),
                key(KeyCode::Enter),
                key(KeyCode::Esc),
                key(KeyCode::Esc),
            ],
        ]);
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let sink = seen.clone();
        let visits = Mutex::new(std::mem::take(&mut visits));
        context.services.insert(Arc::new(SettingsHost(Arc::new(
            move |mut screen: Box<Screen>| {
                let view = screen.view(100, 30, "> ", &Theme::dark());
                let plain = view
                    .lines
                    .iter()
                    .map(|line| azazel_tui::text::strip_ansi(line).trim_end().to_string())
                    .collect();
                sink.lock().unwrap().push(plain);
                let keys = visits.lock().unwrap().pop_front().expect("shown a third time");
                let done = keys.iter().any(|key| screen.on_key(key));
                assert!(done, "the screen kept the keyboard");
                let (reply, back) = oneshot::channel();
                reply.send(screen).ok();
                back
            },
        ))));

        let outcome = handler(&f).run("", &context).await;
        let CommandOutcome::Show(summary) = outcome else {
            panic!("no summary");
        };
        let lines: Vec<&str> = summary.lines().collect();
        assert_eq!(lines.len(), 3, "{summary}");
        assert!(lines[1].contains("openai"), "{summary}");
        assert_eq!(lines[2], "- notify.bell = off (applies to the next session)");
        assert!(f.home_text().contains("bell = false"));
        let stored = std::fs::read_to_string(f.paths.auth_file()).unwrap();
        assert!(stored.contains("sk-test-key"));

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        // First nothing is stored; after the key the page lists it and offers to forget it,
        // and says what the prompts came to.
        assert!(!seen[0].iter().any(|row| row.contains("Sign out")));
        assert!(!seen[0].contains(&"Credentials".to_string()));
        assert!(seen[1].iter().any(|row| row.contains("Sign out or forget a key")));
        assert!(seen[1].contains(&"Credentials".to_string()));
        assert!(
            seen[1].iter().any(|row| row.trim_start().starts_with("openai")),
            "{:#?}",
            seen[1]
        );
        assert!(seen[1].iter().any(|row| row == lines[1].trim_start_matches("- ")));
        // The prompts in between were those of the key: the provider, then the key.
        let lists = ui.lists.lock().unwrap();
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].0, "Store an API key for which provider?");
    }

    #[tokio::test]
    async fn settings_need_someone_to_show_them_to() {
        let f = fixture("", "");
        let context = ExtCtx {
            services: Arc::new(azazel_agent::Services::new()),
            cwd: std::path::PathBuf::new(),
            session_id: String::new(),
        };
        let outcome = handler(&f).run("", &context).await;
        assert!(matches!(outcome, CommandOutcome::Error(why) if why.contains("interactive UI")));

        // A config file that does not load is said, with the file.
        std::fs::write(f.paths.home_config(), "[tui\n").unwrap();
        let outcome = handler(&f).run("", &ctx(Scripted::new(Vec::new()))).await;
        assert!(
            matches!(&outcome, CommandOutcome::Error(why) if why.contains("config.toml")),
            "{outcome:?}"
        );
    }
}
