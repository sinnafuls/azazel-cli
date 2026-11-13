//! `azazel login` and `azazel logout`, and their slash commands: API keys and the logins of
//! providers with an authorisation server, kept in `auth.json`.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;

use async_trait::async_trait;
use azazel_ai::credentials::{account_key, check_label};
use azazel_ai::providers::http::http_client;
use azazel_ai::providers::presets;
use azazel_ai::{Credential, Credentials, DEFAULT_ACCOUNT, LoginUi, OAuthSettings, ProviderConfig};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Config, ExtCtx, ExtError, Extension, Level, LoadOptions, Paths,
    Registrar, Ui,
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio_util::sync::CancellationToken;

use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};

/// The providers that take a key or have a login, sorted by id.
pub fn key_providers(config: &Config) -> Result<Vec<ProviderConfig>, String> {
    Ok(presets::resolve(&config.providers)?
        .into_iter()
        .filter(|provider| provider.requires_key)
        .collect())
}

fn find<'a>(providers: &'a [ProviderConfig], id: &str) -> Result<&'a ProviderConfig, String> {
    providers
        .iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| {
            let known: Vec<&str> = providers.iter().map(|provider| provider.id.as_str()).collect();
            format!(
                "`{id}` is not a provider that takes a key or a login; there are: {}",
                known.join(", ")
            )
        })
}

/// Opens `url` in the user's browser when a person is at the terminal.
pub(crate) fn open_in_browser(url: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    let quiet = std::process::Stdio::null;
    let _ = command
        .arg(url)
        .stdin(quiet())
        .stdout(quiet())
        .stderr(quiet())
        .spawn();
}

/// The account a command line or a slash command names: `None` is the default account.
pub(crate) fn account_label(label: Option<&str>) -> Result<&str, String> {
    match label {
        None | Some(DEFAULT_ACCOUNT) => Ok(DEFAULT_ACCOUNT),
        Some(label) => check_label(label).map(|()| label),
    }
}

/// Signs in with the provider's authorisation server and keeps the login as account `label`
/// (the default account when `None`). A login for an account that is already stored, under
/// whatever label, renews that account instead.
pub(crate) async fn sign_in(
    credentials: &Credentials,
    provider: &ProviderConfig,
    settings: &OAuthSettings,
    ui: &dyn LoginUi,
    cancel: &CancellationToken,
    label: Option<&str>,
) -> Result<String, String> {
    let wanted = account_label(label)?;
    let login = azazel_ai::oauth::login(&http_client(), settings, ui, cancel)
        .await
        .map_err(|e| format!("login to {} failed: {e}", provider.id))?;
    let file = credentials.auth_file().display().to_string();
    let known = credentials.same_account(&provider.id, &login);
    let label = known.as_deref().unwrap_or(wanted);
    credentials
        .store(&account_key(&provider.id, label), login)
        .map_err(|e| format!("could not write {file}: {e}"))?;
    Ok(match (label, known.is_some() && label != wanted) {
        (_, true) => format!(
            "this is the {} account already stored as `{label}`; its login is renewed in {file}",
            provider.id
        ),
        (DEFAULT_ACCOUNT, _) => format!("signed in to {}; the login is kept in {file}", provider.id),
        _ => format!(
            "signed in to {} as account `{label}`; the login is kept in {file}",
            provider.id
        ),
    })
}

/// The person at the terminal.
struct Terminal;

#[async_trait]
impl LoginUi for Terminal {
    fn show(&self, text: &str) {
        eprintln!("{text}");
    }

    fn open(&self, url: &str) {
        open_in_browser(url);
    }

    async fn ask(&self, prompt: &str) -> Option<String> {
        eprintln!("{prompt}");
        let read = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).map(|_| line)
        });
        let line = read.await.ok()?.ok()?;
        Some(line.trim().to_string()).filter(|line| !line.is_empty())
    }
}

/// The person at the terminal UI.
pub(crate) struct Overlay(pub(crate) Arc<Ui>);

#[async_trait]
impl LoginUi for Overlay {
    fn show(&self, text: &str) {
        self.0.0.notify(Level::Info, text);
    }

    fn open(&self, url: &str) {
        open_in_browser(url);
    }

    async fn ask(&self, prompt: &str) -> Option<String> {
        self.0.0.input(prompt).await.ok()
    }
}

/// Signs in from the terminal UI. While the browser is open a small overlay waits, and choosing
/// it (or pressing esc) cancels the sign-in; the overlay goes away by itself when the sign-in
/// ends first. `Ok(None)` is a cancelled sign-in.
pub(crate) async fn sign_in_from_ui(
    credentials: &Credentials,
    provider: &ProviderConfig,
    settings: &OAuthSettings,
    ui: Arc<Ui>,
    label: Option<&str>,
) -> Result<Option<String>, String> {
    let cancel = CancellationToken::new();
    let login = Overlay(ui.clone());
    let signing_in = sign_in(credentials, provider, settings, &login, &cancel, label);
    tokio::pin!(signing_in);
    let cancel_option = ["Cancel the sign-in".to_string()];
    let waiting =
        ui.0.select("Waiting for the sign-in to finish in the browser", &cancel_option);
    // The sign-in goes first, so a question it asks at once (paste the address, when no local
    // port could be opened) is shown before the waiting overlay.
    tokio::select! {
        biased;
        done = &mut signing_in => return done.map(Some),
        _ = waiting => cancel.cancel(),
    }
    match signing_in.await {
        Ok(line) => Ok(Some(line)),
        Err(_) if cancel.is_cancelled() => Ok(None),
        Err(why) => Err(why),
    }
}

/// Stores `key` for `provider` as account `label` (the default account when `None`) and returns
/// the line to show. A key from the environment or a key command still wins over the stored
/// one, which the line says.
pub fn store_key(
    credentials: &Credentials,
    provider: &ProviderConfig,
    key: &str,
    label: Option<&str>,
) -> Result<String, String> {
    let label = account_label(label)?;
    let key = key.trim();
    if key.is_empty() {
        return Err("no key given".to_string());
    }
    let file = credentials.auth_file().display().to_string();
    credentials
        .store(
            &account_key(&provider.id, label),
            Credential::ApiKey { key: key.to_string() },
        )
        .map_err(|e| format!("could not write {file}: {e}"))?;
    let mut line = match label {
        DEFAULT_ACCOUNT => format!("stored the {} key in {file}", provider.id),
        label => format!("stored the {} key of account `{label}` in {file}", provider.id),
    };
    match credentials.source(provider) {
        Some(source) if !source.starts_with("auth.json") => {
            line.push_str(&format!("\nnote: the key from {source} takes precedence over it"));
        }
        _ => {}
    }
    Ok(line)
}

/// Forgets the stored key or login under `entry`: a provider id for its default account, or
/// `provider#label` for another one.
pub fn forget_key(credentials: &Credentials, entry: &str) -> Result<String, String> {
    let what = match credentials.load().get(entry) {
        Some(Credential::OAuth { .. }) => "login",
        _ => "key",
    };
    let (provider, account) = match entry.split_once('#') {
        Some((provider, label)) => (provider, format!(" of account `{label}`")),
        None => (entry, String::new()),
    };
    match credentials.remove(entry) {
        Ok(true) => Ok(format!("forgot the stored {provider} {what}{account}")),
        Ok(false) => Err(format!(
            "there is no stored key or login for `{provider}`{account}"
        )),
        Err(e) => Err(format!(
            "could not update {}: {e}",
            credentials.auth_file().display()
        )),
    }
}

/// Forgets account `label` of `provider` (the default account when `None`).
pub fn forget_account(
    credentials: &Credentials,
    provider: &str,
    label: Option<&str>,
) -> Result<String, String> {
    forget_key(credentials, &account_key(provider, account_label(label)?))
}

/// Reads a line from the terminal without showing it; from a pipe, reads the line as it is.
pub(crate) fn read_secret(prompt: &str) -> std::io::Result<String> {
    let mut out = std::io::stderr();
    write!(out, "{prompt}")?;
    out.flush()?;
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim().to_string());
    }
    crossterm::terminal::enable_raw_mode()?;
    let mut secret = String::new();
    let read = loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret.clear();
                    break Ok(());
                }
                KeyCode::Esc => {
                    secret.clear();
                    break Ok(());
                }
                KeyCode::Backspace => {
                    secret.pop();
                }
                KeyCode::Char(c) => secret.push(c),
                _ => {}
            },
            // A pasted key arrives as one event when the terminal supports bracketed paste.
            Ok(Event::Paste(text)) => secret.push_str(&text),
            Ok(_) => {}
            Err(e) => break Err(e),
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    writeln!(out)?;
    read.map(|()| secret.trim().to_string())
}

fn load(paths: &Paths) -> Result<(Vec<ProviderConfig>, Credentials), String> {
    let options = LoadOptions {
        // Project files may not redirect where a key is sent.
        trusted: false,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let config = Config::load(paths, &options).map_err(|e| e.to_string())?.config;
    Ok((key_providers(&config)?, Credentials::new(paths.auth_file())))
}

/// `azazel login [provider] [--key] [--as label]`: asks for the provider when none is given, then
/// signs in with its authorisation server or asks for its key, and keeps the result as account
/// `label`.
pub async fn login(provider: Option<&str>, key: bool, label: Option<&str>, paths: &Paths) -> u8 {
    if let Err(why) = account_label(label) {
        eprintln!("azazel: --as: {why}");
        return EXIT_USAGE;
    }
    let chosen = load(paths).and_then(|(providers, credentials)| {
        let id = match provider {
            Some(id) => id.to_string(),
            None => {
                for (number, provider) in providers.iter().enumerate() {
                    eprintln!("{:>2}. {}", number + 1, provider.id);
                }
                eprint!("provider (name or number): ");
                let mut line = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut line)
                    .map_err(|e| e.to_string())?;
                let answer = line.trim();
                match answer.parse::<usize>() {
                    Ok(number) => providers
                        .get(number.wrapping_sub(1))
                        .map(|provider| provider.id.clone())
                        .ok_or_else(|| format!("there is no provider number {number}"))?,
                    Err(_) => answer.to_string(),
                }
            }
        };
        Ok((find(&providers, &id)?.clone(), credentials))
    });
    let done = match chosen {
        Err(why) => Err(why),
        Ok((provider, credentials)) => match provider.oauth.as_ref().filter(|_| !key) {
            Some(settings) => {
                let cancel = CancellationToken::new();
                let interrupted = cancel.clone();
                tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        interrupted.cancel();
                    }
                });
                sign_in(&credentials, &provider, settings, &Terminal, &cancel, label).await
            }
            None => read_secret(&format!("API key for {}: ", provider.id))
                .map_err(|e| e.to_string())
                .and_then(|key| store_key(&credentials, &provider, &key, label)),
        },
    };
    report(done)
}

/// `azazel logout <provider> [--account label]`.
pub fn logout(provider: &str, account: Option<&str>, paths: &Paths) -> u8 {
    report(forget_account(
        &Credentials::new(paths.auth_file()),
        provider,
        account,
    ))
}

fn report(done: Result<String, String>) -> u8 {
    match done {
        Ok(line) => {
            println!("{line}");
            EXIT_OK
        }
        Err(why) if why == "no key given" => {
            eprintln!("azazel: {why}");
            EXIT_USAGE
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

struct LoginCommand {
    providers: Vec<ProviderConfig>,
    credentials: Arc<Credentials>,
}

#[async_trait]
impl CommandHandler for LoginCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let Some(ui) = ctx.services.get::<Ui>() else {
            return CommandOutcome::Error("there is nobody to ask for the key".to_string());
        };
        // `/login <provider> [label]`: the label keeps the login beside the provider's other
        // accounts.
        let mut words = args.split_whitespace();
        let (first, label) = (words.next(), words.next());
        let id = match first {
            None => {
                let names: Vec<String> = self
                    .providers
                    .iter()
                    .map(|provider| provider.id.clone())
                    .collect();
                match ui.0.select("Log in to which provider?", &names).await {
                    Ok(index) => names[index].clone(),
                    Err(_) => return CommandOutcome::Done,
                }
            }
            Some(id) => id.to_string(),
        };
        let provider = match find(&self.providers, &id) {
            Ok(provider) => provider,
            Err(why) => return CommandOutcome::Error(why),
        };
        if let Some(settings) = &provider.oauth {
            return match sign_in_from_ui(&self.credentials, provider, settings, ui, label).await {
                Ok(Some(line)) => CommandOutcome::Show(line),
                Ok(None) => CommandOutcome::Show("sign-in cancelled".to_string()),
                Err(why) => CommandOutcome::Error(why),
            };
        }
        let prompt = format!("API key for {} (shown while you type)", provider.id);
        match ui.0.input(&prompt).await {
            Ok(key) => match store_key(&self.credentials, provider, &key, label) {
                Ok(line) => CommandOutcome::Show(line),
                Err(why) => CommandOutcome::Error(why),
            },
            Err(_) => CommandOutcome::Done,
        }
    }
}

struct LogoutCommand(Arc<Credentials>);

#[async_trait]
impl CommandHandler for LogoutCommand {
    async fn run(&self, args: &str, _: &ExtCtx) -> CommandOutcome {
        let mut words = args.split_whitespace();
        match (words.next(), words.next()) {
            (None, _) => CommandOutcome::Error("usage: /logout <provider> [account]".to_string()),
            (Some(provider), account) => match forget_account(&self.0, provider, account) {
                Ok(line) => CommandOutcome::Show(line),
                Err(why) => CommandOutcome::Error(why),
            },
        }
    }
}

/// Registers `/login` and `/logout`.
pub struct LoginExt {
    pub providers: Vec<ProviderConfig>,
    pub credentials: Arc<Credentials>,
}

impl Extension for LoginExt {
    fn id(&self) -> &str {
        "login"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(Command {
            name: "login".to_string(),
            description: "Sign in to a provider or store its API key".to_string(),
            argument_hint: Some("[provider] [account]".to_string()),
            handler: Arc::new(LoginCommand {
                providers: self.providers.clone(),
                credentials: self.credentials.clone(),
            }),
        });
        r.command(Command {
            name: "logout".to_string(),
            description: "Forget the stored key or login of a provider".to_string(),
            argument_hint: Some("<provider> [account]".to_string()),
            handler: Arc::new(LogoutCommand(self.credentials.clone())),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn credentials(dir: &std::path::Path, env_key: Option<&'static str>) -> Credentials {
        Credentials::with_env(
            dir.join("auth.json"),
            Arc::new(move |name| (name == "ANTHROPIC_API_KEY").then_some(env_key?.to_string())),
        )
    }

    #[tokio::test]
    async fn a_stored_key_is_used_until_it_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let providers = key_providers(&Config::default()).unwrap();
        let anthropic = find(&providers, "anthropic").unwrap();
        let store = credentials(dir.path(), None);

        assert_eq!(
            store_key(&store, anthropic, "  ", None).unwrap_err(),
            "no key given"
        );
        let line = store_key(&store, anthropic, " sk-stored \n", None).unwrap();
        assert!(
            line.starts_with("stored the anthropic key in ") && !line.contains("note:"),
            "{line}"
        );
        assert_eq!(
            store.resolve(anthropic).await.unwrap(),
            azazel_ai::Auth::ApiKey("sk-stored".into())
        );

        assert_eq!(
            forget_key(&store, "anthropic").unwrap(),
            "forgot the stored anthropic key"
        );
        assert!(
            forget_key(&store, "anthropic")
                .unwrap_err()
                .contains("no stored key")
        );
        assert!(store.resolve(anthropic).await.is_err());
    }

    #[test]
    fn storing_a_key_says_when_the_environment_overrides_it() {
        let dir = tempfile::tempdir().unwrap();
        let providers = key_providers(&Config::default()).unwrap();
        let anthropic = find(&providers, "anthropic").unwrap();
        let line = store_key(
            &credentials(dir.path(), Some("sk-env")),
            anthropic,
            "sk-stored",
            None,
        )
        .unwrap();
        assert!(
            line.ends_with("note: the key from env ANTHROPIC_API_KEY takes precedence over it"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn a_second_account_is_kept_beside_the_first_and_forgotten_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let providers = key_providers(&Config::default()).unwrap();
        let anthropic = find(&providers, "anthropic").unwrap();
        let store = credentials(dir.path(), None);

        store_key(&store, anthropic, "sk-first", None).unwrap();
        let line = store_key(&store, anthropic, "sk-work", Some("work")).unwrap();
        assert!(
            line.starts_with("stored the anthropic key of account `work` in "),
            "{line}"
        );
        assert!(store_key(&store, anthropic, "sk", Some("no spaces")).is_err());
        let labels: Vec<String> = store
            .accounts("anthropic")
            .into_iter()
            .map(|account| account.label)
            .collect();
        assert_eq!(labels, ["default", "work"]);
        // The default account answers until another one is selected.
        assert_eq!(
            store.resolve(anthropic).await.unwrap(),
            azazel_ai::Auth::ApiKey("sk-first".into())
        );

        assert_eq!(
            forget_account(&store, "anthropic", Some("work")).unwrap(),
            "forgot the stored anthropic key of account `work`"
        );
        assert!(
            forget_account(&store, "anthropic", Some("work"))
                .unwrap_err()
                .contains("no stored key or login for `anthropic` of account `work`")
        );
        assert_eq!(store.accounts("anthropic").len(), 1);
        assert_eq!(
            forget_account(&store, "anthropic", Some("default")).unwrap(),
            "forgot the stored anthropic key"
        );
    }

    #[test]
    fn only_providers_that_take_a_key_can_be_logged_in_to() {
        let providers = key_providers(&Config::default()).unwrap();
        assert!(find(&providers, "openai").is_ok());
        // Local servers need no key.
        let unknown = find(&providers, "ollama").unwrap_err();
        assert!(
            unknown.contains("`ollama` is not a provider that takes a key or a login")
                && unknown.contains("anthropic")
        );
    }
}
