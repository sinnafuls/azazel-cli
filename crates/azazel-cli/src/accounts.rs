//! `/account`: the stored accounts of a provider, what each has left of its usage windows, and
//! which one this session works on.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_ai::credentials::{check_label, now_ms};
use azazel_ai::{
    Account, Credentials, DEFAULT_ACCOUNT, LimitBlock, LimitStore, ProviderConfig, Router, Window,
};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, EntryKind, ExtCtx, ExtError, Extension, Level, Registrar,
    SessionStore, Ui,
};
use serde_json::json;

use crate::login::{forget_account, sign_in_from_ui, store_key};

/// The `custom` session entry that records the account a session works on.
const ACCOUNT_ENTRY: &str = "account";

/// A time as a person reads it off a clock: `14:05` today, `Tue 14:05` on another day.
pub fn clock(unix_secs: u64) -> String {
    let zone = jiff::tz::TimeZone::system();
    let Ok(at) = jiff::Timestamp::from_second(i64::try_from(unix_secs).unwrap_or(i64::MAX)) else {
        return "later".to_string();
    };
    let at = at.to_zoned(zone.clone());
    let format = match at.date() == jiff::Timestamp::now().to_zoned(zone).date() {
        true => "%H:%M",
        false => "%a %H:%M",
    };
    at.strftime(format).to_string()
}

/// What the transcript says when the router moves a provider to another account.
pub fn switch_line(provider: &str, from: &str, to: &str, until: u64, limit: &str) -> String {
    format!(
        "{provider}: account {from} reached its {limit} (resets {}); now using {to}",
        clock(until)
    )
}

/// Selects the accounts a resumed session worked on.
pub fn restore(session: &SessionStore, credentials: &Credentials) {
    for entry in session.branch(None) {
        if let EntryKind::Custom { custom_type, data } = &entry.kind
            && custom_type == ACCOUNT_ENTRY
            && let (Some(provider), Some(account)) = (data["provider"].as_str(), data["account"].as_str())
        {
            credentials.select(provider, account);
        }
    }
}

/// One window as `5h 42% (resets 14:05)`; extra usage as what the provider says about it.
fn window_text(window: &Window, now: u64) -> String {
    if window.name == "overage" {
        return format!("extra usage {}", window.status);
    }
    let percent = (window.used * 100.0).round().clamp(0.0, 999.0) as u32;
    let name = crate::usage::window_name(window);
    match window.resets_at.filter(|at| *at > now) {
        Some(at) => format!("{name} {percent}% (resets {})", clock(at)),
        None => format!("{name} {percent}%"),
    }
}

/// One row of the account list. `who` is the account as [`crate::usage::describe`] shows it.
fn account_row(
    who: String,
    in_use: bool,
    is_default: bool,
    windows: &[Window],
    blocks: &[LimitBlock],
    now: u64,
) -> String {
    let mut parts = vec![who];
    if in_use {
        parts.push("in use".to_string());
    }
    if is_default {
        parts.push("default".to_string());
    }
    for block in blocks {
        parts.push(match &block.scope {
            Some(scope) => format!("out of allowance for {scope} until {}", clock(block.until)),
            None => format!("out of allowance until {}", clock(block.until)),
        });
    }
    parts.extend(windows.iter().map(|window| window_text(window, now)));
    if windows.is_empty() && blocks.is_empty() {
        parts.push("no usage seen yet".to_string());
    }
    parts.join(" · ")
}

struct AccountCommand {
    providers: Vec<ProviderConfig>,
    credentials: Arc<Credentials>,
    home_config: PathBuf,
    /// `[usage] redact_email`: e-mail addresses are shown as `j***@example.com`.
    redact_email: bool,
}

/// What can be done with one account, in menu order.
#[derive(Clone, Copy)]
enum Action {
    Use,
    MakeDefault,
    SignOut,
}

impl AccountCommand {
    /// The provider `/account` is about: the one named, the only one with a stored account, or
    /// the one the user picks.
    async fn provider(&self, named: &str, ui: &Ui) -> Result<Option<&ProviderConfig>, String> {
        if !named.is_empty() {
            return self
                .providers
                .iter()
                .find(|provider| provider.id == named)
                .map(Some)
                .ok_or_else(|| format!("`{named}` is not a provider that takes a key or a login"));
        }
        let with_accounts: Vec<&ProviderConfig> = self
            .providers
            .iter()
            .filter(|provider| !self.credentials.accounts(&provider.id).is_empty())
            .collect();
        match with_accounts.as_slice() {
            [] => Err("no account is stored yet: /login <provider> signs in or stores a key".to_string()),
            [only] => Ok(Some(*only)),
            many => {
                let labels: Vec<String> = many
                    .iter()
                    .map(|provider| {
                        let count = self.credentials.accounts(&provider.id).len();
                        match count {
                            1 => format!("{} (1 account)", provider.id),
                            count => format!("{} ({count} accounts)", provider.id),
                        }
                    })
                    .collect();
                match ui.0.select("Accounts of which provider?", &labels).await {
                    Ok(index) => Ok(Some(many[index])),
                    Err(_) => Ok(None),
                }
            }
        }
    }

    fn rows(&self, provider: &ProviderConfig, limits: &LimitStore) -> (String, Vec<Account>, Vec<String>) {
        let accounts = self.credentials.accounts(&provider.id);
        let in_use = self.credentials.selected(&provider.id);
        let default = self.credentials.default_account(&provider.id);
        let now = now_ms() / 1000;
        let rows = accounts
            .iter()
            .map(|account| {
                account_row(
                    crate::usage::describe(account, self.redact_email),
                    in_use.as_deref() == Some(&account.label),
                    default.as_deref() == Some(&account.label),
                    &limits.windows(&provider.id, &account.label),
                    &limits.blocks(&provider.id, &account.label),
                    now,
                )
            })
            .collect();
        let mut heading = format!("Accounts of {}", provider.id);
        // A key from elsewhere answers whatever is chosen here; say so before anything is chosen.
        match self.credentials.source(provider) {
            Some(source) if !source.starts_with("auth.json") => {
                heading.push_str(&format!(" · the key from {source} is used instead of these"));
            }
            _ => {}
        }
        (heading, accounts, rows)
    }

    async fn add(&self, provider: &ProviderConfig, ui: &Arc<Ui>) -> Result<Option<String>, String> {
        let taken = self.credentials.accounts(&provider.id);
        // The first account of a provider is its default one and needs no label.
        let label = match taken.iter().any(|account| account.label == DEFAULT_ACCOUNT) {
            false => None,
            true => {
                let Ok(label) = ui.0.input("Label for the new account (for example: work)").await else {
                    return Ok(None);
                };
                let label = label.trim().to_string();
                check_label(&label)?;
                if taken.iter().any(|account| account.label == label) {
                    return Err(format!(
                        "{} already has an account `{label}`; sign it out first, or choose another label",
                        provider.id
                    ));
                }
                Some(label)
            }
        };
        let label = label.as_deref();
        let by_key = match (&provider.oauth, provider.api_key_env.is_empty()) {
            (None, _) => true,
            (Some(_), true) => false,
            (Some(_), false) => {
                let ways = [
                    "Sign in with a subscription".to_string(),
                    "Store an API key".to_string(),
                ];
                match ui.0.select("Add the account how?", &ways).await {
                    Ok(index) => index == 1,
                    Err(_) => return Ok(None),
                }
            }
        };
        match (&provider.oauth, by_key) {
            (Some(settings), false) => {
                sign_in_from_ui(
                    &self.credentials,
                    provider,
                    settings,
                    &Default::default(),
                    ui.clone(),
                    label,
                )
                .await
            }
            _ => match ui.0.secret(&format!("API key for {}", provider.id)).await {
                Ok(key) => store_key(&self.credentials, provider, &key, label).map(Some),
                Err(_) => Ok(None),
            },
        }
    }

    fn act(
        &self,
        provider: &ProviderConfig,
        account: &Account,
        action: Action,
        ctx: &ExtCtx,
    ) -> Result<String, String> {
        let (id, label) = (&provider.id, &account.label);
        match action {
            Action::Use => {
                self.credentials.select(id, label);
                // Chosen knowingly: the account is asked even if it was out of allowance.
                self.credentials.limits().unblock(id, label);
                if let Some(session) = ctx.services.get::<SessionStore>() {
                    session.append(EntryKind::Custom {
                        custom_type: ACCOUNT_ENTRY.to_string(),
                        data: json!({ "provider": id, "account": label }),
                    });
                }
                Ok(format!(
                    "this session now uses account {label} of {id}; resuming it selects the account again"
                ))
            }
            Action::MakeDefault => {
                // The default account is what an unset `account` means, so it is not written.
                let value = (label != DEFAULT_ACCOUNT).then(|| toml_edit::value(label.as_str()));
                crate::init::set_config_value(&self.home_config, &["providers", id, "account"], value)
                    .map_err(|e| format!("{e:#}"))?;
                self.credentials
                    .prefer(id, (label != DEFAULT_ACCOUNT).then_some(label.as_str()));
                let mut line = format!(
                    "{label} is now the default account of {id} (saved in {})",
                    self.home_config.display()
                );
                match self.credentials.selected(id) {
                    Some(in_use) if in_use != *label => {
                        line.push_str(&format!("; this session stays on {in_use}"));
                    }
                    _ => {}
                }
                Ok(line)
            }
            Action::SignOut => forget_account(&self.credentials, id, Some(label)),
        }
    }
}

#[async_trait]
impl CommandHandler for AccountCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let Some(ui) = ctx.services.get::<Ui>() else {
            return CommandOutcome::Error("/account needs the interactive UI".to_string());
        };
        let provider = match self.provider(args.trim(), &ui).await {
            Ok(Some(provider)) => provider,
            Ok(None) => return CommandOutcome::Done,
            Err(why) => return CommandOutcome::Error(why),
        };
        let limits = match ctx.services.get::<Router>() {
            Some(router) => router.limits(),
            None => self.credentials.limits().clone(),
        };
        let mut done: Vec<String> = Vec::new();
        loop {
            let (heading, accounts, mut options) = self.rows(provider, &limits);
            options.push("Add another account".to_string());
            options.push("Done".to_string());
            let Ok(chosen) = ui.0.select(&heading, &options).await else {
                break;
            };
            let outcome = match accounts.get(chosen) {
                Some(account) => {
                    let actions = [
                        ("Use for this session", Action::Use),
                        ("Make default", Action::MakeDefault),
                        ("Sign out", Action::SignOut),
                    ];
                    let mut names: Vec<String> = actions.iter().map(|(name, _)| name.to_string()).collect();
                    names.push("Back".to_string());
                    let title = format!(
                        "{} · {}",
                        provider.id,
                        crate::usage::describe(account, self.redact_email)
                    );
                    match ui.0.select(&title, &names).await {
                        Ok(index) => match actions.get(index) {
                            Some((_, action)) => self.act(provider, account, *action, ctx).map(Some),
                            None => Ok(None),
                        },
                        Err(_) => Ok(None),
                    }
                }
                None if chosen == accounts.len() => self.add(provider, &ui).await,
                None => break,
            };
            match outcome {
                Ok(Some(line)) => {
                    ui.0.notify(Level::Info, &line);
                    done.push(line);
                }
                Ok(None) => {}
                Err(why) => ui.0.notify(Level::Error, &why),
            }
        }
        match done.is_empty() {
            true => CommandOutcome::Done,
            false => CommandOutcome::Show(format!("accounts changed:\n- {}", done.join("\n- "))),
        }
    }
}

/// Registers `/account`.
pub struct AccountsExt {
    /// The providers that take a key or have a login.
    pub providers: Vec<ProviderConfig>,
    pub credentials: Arc<Credentials>,
    pub home_config: PathBuf,
    /// `[usage] redact_email`.
    pub redact_email: bool,
}

impl Extension for AccountsExt {
    fn id(&self) -> &str {
        "accounts"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(Command {
            name: "account".to_string(),
            description: "The accounts of a provider: usage, switch, make default, sign out, add".to_string(),
            argument_hint: Some("[provider]".to_string()),
            handler: Arc::new(AccountCommand {
                providers: self.providers.clone(),
                credentials: self.credentials.clone(),
                home_config: self.home_config.clone(),
                redact_email: self.redact_email,
            }),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_agent::Services;
    use azazel_ai::Credential;
    use azazel_ai::credentials::account_key;

    use super::*;

    fn command(dir: &std::path::Path) -> AccountCommand {
        let credentials = Arc::new(Credentials::with_env(
            dir.join("auth.json"),
            Arc::new(|_: &str| None),
        ));
        for label in ["default", "work"] {
            let key = format!("k-{label}");
            credentials
                .store(&account_key("anthropic", label), Credential::ApiKey { key })
                .unwrap();
        }
        AccountCommand {
            providers: crate::login::key_providers(&azazel_core::Config::default()).unwrap(),
            credentials,
            home_config: dir.join("config.toml"),
            redact_email: true,
        }
    }

    fn context(session: Option<Arc<SessionStore>>) -> ExtCtx {
        let services = Arc::new(Services::new());
        if let Some(session) = session {
            services.insert(session);
        }
        ExtCtx {
            services,
            cwd: PathBuf::from("."),
            session_id: String::new(),
        }
    }

    #[test]
    fn a_row_says_who_the_account_is_and_what_it_has_left() {
        let now = now_ms() / 1000;
        let account = Account {
            label: "work".into(),
            login: true,
            email: Some("wanda@example.com".into()),
            org: None,
        };
        let windows = [
            Window::new("5h", 0.424, Some(now - 5), "allowed"),
            Window::new("7d-fable", 0.9, None, "allowed").scoped("Fable"),
            Window::new("overage", 0.0, None, "rejected (org_level_disabled)"),
        ];
        // The address is hidden but for its first letter and its domain unless the setting
        // says otherwise.
        assert_eq!(
            account_row(
                crate::usage::describe(&account, true),
                true,
                false,
                &windows,
                &[],
                now
            ),
            "work (w***@example.com) · in use · 5h 42% · 7d Fable 90% · extra usage rejected \
             (org_level_disabled)"
        );
        assert_eq!(
            crate::usage::describe(&account, false),
            "work (wanda@example.com)"
        );
        let key = Account {
            label: "default".into(),
            login: false,
            email: None,
            org: None,
        };
        assert_eq!(
            account_row(crate::usage::describe(&key, true), false, true, &[], &[], now),
            "default (API key) · default · no usage seen yet"
        );
        let block = |scope: Option<&str>| LimitBlock {
            until: now + 3600,
            since: now,
            window: None,
            scope: scope.map(str::to_string),
        };
        let blocked = account_row("default".into(), false, false, &[], &[block(None)], now);
        assert!(
            blocked.starts_with("default · out of allowance until "),
            "{blocked}"
        );
        let fable = account_row("default".into(), false, false, &[], &[block(Some("Fable"))], now);
        assert!(
            fable.starts_with("default · out of allowance for Fable until "),
            "{fable}"
        );
        let line = switch_line("anthropic", "work", "personal", now + 60, "5-hour limit");
        assert!(
            line.starts_with("anthropic: account work reached its 5-hour limit (resets ")
                && line.ends_with("); now using personal"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn using_an_account_is_recorded_in_the_session_and_selected_again_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let command = command(dir.path());
        let anthropic = command
            .providers
            .iter()
            .find(|provider| provider.id == "anthropic")
            .unwrap();
        let session = Arc::new(SessionStore::create(&dir.path().join("sessions"), dir.path(), None).unwrap());
        let work = command.credentials.accounts("anthropic").remove(1);
        // The account ran out earlier; picking it knowingly has it asked again.
        let limits = command.credentials.limits();
        limits.block("anthropic", "work", now_ms() / 1000 + 600);
        limits.record(
            "anthropic",
            "work",
            vec![Window::new("5h", 1.0, Some(now_ms() / 1000 + 600), "rejected")],
        );
        assert!(!limits.open("anthropic", "work", "claude-opus-5-5"));

        let line = command
            .act(anthropic, &work, Action::Use, &context(Some(session.clone())))
            .unwrap();
        assert!(
            line.starts_with("this session now uses account work of anthropic"),
            "{line}"
        );
        assert_eq!(command.credentials.selected("anthropic").as_deref(), Some("work"));
        assert!(limits.open("anthropic", "work", "claude-opus-5-5"));

        // Another process resuming the session ends up on the same account.
        let reopened = SessionStore::open(&session.path()).unwrap();
        let later = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_: &str| None));
        assert_eq!(later.selected("anthropic").as_deref(), Some("default"));
        restore(&reopened, &later);
        assert_eq!(later.selected("anthropic").as_deref(), Some("work"));
        assert_eq!(
            later.resolve(anthropic).await.unwrap(),
            azazel_ai::Auth::ApiKey("k-work".into())
        );
    }

    #[test]
    fn the_default_account_is_saved_in_the_configuration_and_sign_out_forgets_one_account() {
        let dir = tempfile::tempdir().unwrap();
        let command = command(dir.path());
        let anthropic = command
            .providers
            .iter()
            .find(|provider| provider.id == "anthropic")
            .unwrap();
        let accounts = command.credentials.accounts("anthropic");
        let ctx = context(None);

        let line = command
            .act(anthropic, &accounts[1], Action::MakeDefault, &ctx)
            .unwrap();
        assert!(
            line.starts_with("work is now the default account of anthropic"),
            "{line}"
        );
        let written = std::fs::read_to_string(&command.home_config).unwrap();
        let config: toml::Table = written.parse().unwrap();
        assert_eq!(config["providers"]["anthropic"]["account"].as_str(), Some("work"));
        assert_eq!(command.credentials.selected("anthropic").as_deref(), Some("work"));
        // The file loads as configuration.
        let settings: azazel_ai::ProviderSettings =
            config["providers"]["anthropic"].clone().try_into().unwrap();
        assert_eq!(settings.account.as_deref(), Some("work"));

        // Going back to the default account removes the setting.
        command
            .act(anthropic, &accounts[0], Action::MakeDefault, &ctx)
            .unwrap();
        let written = std::fs::read_to_string(&command.home_config).unwrap();
        assert!(!written.contains("account"), "{written}");
        assert_eq!(
            command.credentials.default_account("anthropic").as_deref(),
            Some("default")
        );

        assert_eq!(
            command
                .act(anthropic, &accounts[1], Action::SignOut, &ctx)
                .unwrap(),
            "forgot the stored anthropic key of account `work`"
        );
        assert_eq!(command.credentials.accounts("anthropic").len(), 1);
    }
}
