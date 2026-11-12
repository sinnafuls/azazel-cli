//! `/settings`: signing in with a subscription, storing API keys, choosing the default model and
//! signing out, all from inside the terminal UI, so that a first run needs no config editing.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::login::{forget_key, sign_in_from_ui, store_key};
use async_trait::async_trait;
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

struct Settings {
    providers: Vec<ProviderConfig>,
    credentials: Arc<Credentials>,
    home_config: PathBuf,
}

/// The menu rows, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Subscription,
    ApiKey,
    DefaultModel,
    Roles,
    SignOut,
    Done,
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
        let heading = format!(
            "Settings · {accounts} · default model: {}",
            default.as_deref().unwrap_or("none")
        );
        let mut items = vec![
            (
                Item::Subscription,
                "Sign in with a subscription (Claude, ChatGPT)".to_string(),
            ),
            (Item::ApiKey, "Add an API key".to_string()),
            (
                Item::DefaultModel,
                format!(
                    "Choose the default model (now: {})",
                    default.as_deref().unwrap_or("none")
                ),
            ),
            (
                Item::Roles,
                "Models for the lead and each agent (coder, scout, reviewer, ...)".to_string(),
            ),
        ];
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
                Some(name) => format!("{name} ({})", p.id),
                None => format!("{} ({})", p.name, p.id),
            })
            .collect();
        let Ok(index) = ui.0.select("Sign in with which account?", &labels).await else {
            return Ok(None);
        };
        let provider = candidates[index];
        let Some(settings) = &provider.oauth else {
            return Ok(None);
        };
        let line = sign_in_from_ui(&self.credentials, provider, settings, ui.clone()).await?;
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
                format!("{} ({}){state}", p.id, p.api_key_env.join(" or "))
            })
            .collect();
        let Ok(index) = ui.0.select("Store an API key for which provider?", &labels).await else {
            return Ok(None);
        };
        let provider = candidates[index];
        let prompt = format!("API key for {} (stored in ~/.azazel/auth.json)", provider.id);
        match ui.0.secret(&prompt).await {
            Ok(key) => store_key(&self.credentials, provider, &key)
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
                        false => format!("{} (local server)", p.id),
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
        let Ok(index) = ui.0.select(&format!("Model for {role} ({provider})"), &ids).await else {
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
                format!("{role:<11} {model}{via} - {what}")
            })
            .collect();
        let Ok(index) = ui.0.select("Which role?", &labels).await else {
            return Ok(None);
        };
        let role = ROLES[index].0;
        let fallback = Config::default().roles.get(role).cloned();
        let mut actions = vec!["Choose a model".to_string()];
        if let Some(chain) = &fallback {
            actions.push(format!("Use the fallback ({})", chain_label(chain)));
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
}

impl Extension for SettingsExt {
    fn id(&self) -> &str {
        "settings"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(Command {
            name: "settings".to_string(),
            description: "Sign in, add API keys and choose the default model".to_string(),
            argument_hint: None,
            handler: Arc::new(Settings {
                providers: self.providers.clone(),
                credentials: self.credentials.clone(),
                home_config: self.home_config.clone(),
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
}
