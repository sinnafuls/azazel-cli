//! Credential lookup and the `auth.json` store.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::AiError;
use crate::limits::LimitStore;
use crate::providers::presets::{ProviderConfig, ProviderSettings};

/// The label of the account kept under the plain provider key of `auth.json`.
pub const DEFAULT_ACCOUNT: &str = "default";

/// The `auth.json` key of `provider`'s account `label`: the provider id for the default account,
/// `provider#label` for the others.
pub fn account_key(provider: &str, label: &str) -> String {
    match label {
        DEFAULT_ACCOUNT => provider.to_string(),
        label => format!("{provider}#{label}"),
    }
}

/// Whether `label` can name an account: letters, digits, `-`, `_` and `.`, and not `default`.
pub fn check_label(label: &str) -> Result<(), String> {
    let plain = !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    match (plain, label) {
        (false, _) => Err(format!(
            "`{label}` cannot name an account: use letters, digits, `-`, `_` and `.`"
        )),
        (true, DEFAULT_ACCOUNT) => Err(format!(
            "`{DEFAULT_ACCOUNT}` is the account without a label; choose another name"
        )),
        (true, _) => Ok(()),
    }
}

/// One stored account of a provider, without its secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub label: String,
    /// A sign-in, as opposed to a stored API key.
    pub login: bool,
    pub email: Option<String>,
    /// The organisation the login belongs to, as the provider identifies it.
    pub org: Option<String>,
}

impl Account {
    fn of(label: &str, credential: &Credential) -> Account {
        let field = |name: &str| match credential {
            Credential::OAuth { extra, .. } => extra
                .get(name)
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            Credential::ApiKey { .. } => None,
        };
        Account {
            label: label.to_string(),
            login: matches!(credential, Credential::OAuth { .. }),
            email: field("email").or_else(|| field("account_email")),
            org: field("org_id"),
        }
    }

    /// The label with the e-mail behind it when the login has one.
    pub fn describe(&self) -> String {
        match &self.email {
            Some(email) => format!("{} ({email})", self.label),
            None if self.login => self.label.clone(),
            None => format!("{} (API key)", self.label),
        }
    }
}

/// What a wire module puts on a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    None,
    ApiKey(String),
    OAuth {
        access: String,
        /// Account fields of the login (`account_id`, `email`, `org_id`, …), for the headers a
        /// subscription endpoint wants beside the bearer token.
        fields: BTreeMap<String, String>,
    },
}

impl Auth {
    /// The provider headers only a login can fill, such as the account a ChatGPT subscription
    /// request is billed to. `{field}` holes are taken from the login; a header with a hole the
    /// login cannot fill is left off, because a provider that asks for one usually refuses the
    /// request when it is empty.
    pub fn login_headers(&self, provider: &ProviderConfig) -> Vec<(String, String)> {
        let Auth::OAuth { fields, .. } = self else {
            return Vec::new();
        };
        provider
            .oauth_headers
            .iter()
            .filter_map(|(name, template)| {
                let mut value = template.clone();
                for (field, found) in fields {
                    value = value.replace(&format!("{{{field}}}"), found);
                }
                (!value.contains('{')).then(|| (name.clone(), value))
            })
            .collect()
    }
}

/// One `auth.json` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey { key: String },
    #[serde(rename = "oauth")]
    OAuth {
        access: String,
        refresh: String,
        /// Unix milliseconds.
        expires: u64,
        /// Provider-specific fields (account id, enterprise domain).
        #[serde(flatten)]
        extra: BTreeMap<String, serde_json::Value>,
    },
}

type EnvFn = dyn Fn(&str) -> Option<String> + Send + Sync;

/// `NAME=value` lines of a `.env` file: `export` prefixes, quotes and `#` comments are
/// understood; anything else on a line is ignored.
fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let valid = !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            continue;
        }
        let value = value.trim();
        let value = if let Some(rest) = value.strip_prefix('"') {
            match rest.split_once('"') {
                Some((quoted, _)) => quoted.replace("\\n", "\n"),
                None => continue,
            }
        } else if let Some(rest) = value.strip_prefix('\'') {
            match rest.split_once('\'') {
                Some((quoted, _)) => quoted.to_string(),
                None => continue,
            }
        } else {
            // An unquoted value ends at a comment.
            value.split(" #").next().unwrap_or_default().trim().to_string()
        };
        if !value.is_empty() {
            vars.insert(name.to_string(), value);
        }
    }
    vars
}

pub struct Credentials {
    auth_file: PathBuf,
    env: Arc<EnvFn>,
    /// `.env` files keys are also looked up in, in order.
    env_files: Vec<PathBuf>,
    /// What those files hold, read when a key is first looked for.
    env_file_vars: OnceLock<Vec<(PathBuf, HashMap<String, String>)>>,
    flag_keys: Mutex<HashMap<String, String>>,
    command_keys: Mutex<HashMap<String, String>>,
    /// Held by the one task that is refreshing a login.
    refreshing: tokio::sync::Mutex<()>,
    http: OnceLock<reqwest::Client>,
    /// The account chosen for this process, per provider: by the user for the session, or by the
    /// router when the one in use ran out of allowance.
    pinned: Mutex<HashMap<String, String>>,
    /// The account `[providers.<id>] account` names, per provider.
    preferred: Mutex<HashMap<String, String>>,
    /// Providers whose last credential came from `auth.json`: only there can an account be
    /// switched.
    stored: Mutex<HashSet<String>>,
    limits: Arc<LimitStore>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Credentials {
    pub fn new(auth_file: PathBuf) -> Self {
        Self::with_env(
            auth_file,
            Arc::new(|name| std::env::var(name).ok().filter(|v| !v.is_empty())),
        )
    }

    /// `env` replaces the process environment (tests).
    pub fn with_env(auth_file: PathBuf, env: Arc<EnvFn>) -> Self {
        Credentials {
            auth_file,
            env,
            env_files: Vec::new(),
            env_file_vars: OnceLock::new(),
            flag_keys: Mutex::default(),
            command_keys: Mutex::default(),
            refreshing: tokio::sync::Mutex::default(),
            http: OnceLock::new(),
            pinned: Mutex::default(),
            preferred: Mutex::default(),
            stored: Mutex::default(),
            limits: Arc::default(),
        }
    }

    /// What the providers reported about each account's usage windows.
    pub fn limits(&self) -> &Arc<LimitStore> {
        &self.limits
    }

    /// The stored accounts of `provider`: the default one first, then the labelled ones by name.
    pub fn accounts(&self, provider: &str) -> Vec<Account> {
        accounts_in(&self.load(), provider)
    }

    /// Uses account `label` of `provider` for the rest of the process. A label that names no
    /// stored account is ignored when a credential is looked up.
    pub fn select(&self, provider: &str, label: &str) {
        locked(&self.pinned).insert(provider.to_string(), label.to_string());
    }

    /// Takes the account each `[providers.<id>]` table names as that provider's preferred one.
    pub fn configured(self, providers: &BTreeMap<String, ProviderSettings>) -> Self {
        for (provider, settings) in providers {
            self.prefer(provider, settings.account.as_deref());
        }
        self
    }

    /// The account `[providers.<id>] account` names; `None` goes back to the default account.
    pub fn prefer(&self, provider: &str, label: Option<&str>) {
        let mut preferred = locked(&self.preferred);
        match label {
            Some(label) => preferred.insert(provider.to_string(), label.to_string()),
            None => preferred.remove(provider),
        };
    }

    /// The account a request to `provider` uses: the one selected for this process, else the
    /// configured one, else the default one, else the first there is. `None` when `auth.json`
    /// holds nothing for the provider.
    pub fn selected(&self, provider: &str) -> Option<String> {
        self.selected_in(&self.load(), provider)
    }

    /// The account a new session starts on: the configured one, else the default one, else the
    /// first there is.
    pub fn default_account(&self, provider: &str) -> Option<String> {
        self.default_in(&self.load(), provider)
    }

    fn default_in(&self, entries: &BTreeMap<String, Credential>, provider: &str) -> Option<String> {
        let exists = |label: &String| entries.contains_key(&account_key(provider, label));
        let preferred = locked(&self.preferred).get(provider).cloned();
        preferred
            .filter(exists)
            .or_else(|| Some(DEFAULT_ACCOUNT.to_string()).filter(exists))
            .or_else(|| accounts_in(entries, provider).into_iter().next().map(|a| a.label))
    }

    fn selected_in(&self, entries: &BTreeMap<String, Credential>, provider: &str) -> Option<String> {
        let pinned = locked(&self.pinned).get(provider).cloned();
        pinned
            .filter(|label| entries.contains_key(&account_key(provider, label)))
            .or_else(|| self.default_in(entries, provider))
    }

    /// Whether the last credential handed out for `provider` came from `auth.json`. A key from
    /// the environment or the configuration is the same whichever account is selected.
    pub fn uses_store(&self, provider: &str) -> bool {
        locked(&self.stored).contains(provider)
    }

    /// The label of the stored login of `provider` that is the same account as `credential`:
    /// same e-mail and same organisation.
    pub fn same_account(&self, provider: &str, credential: &Credential) -> Option<String> {
        let new = Account::of("", credential);
        new.email.as_ref()?;
        self.accounts(provider)
            .into_iter()
            .find(|known| known.login && known.email == new.email && known.org == new.org)
            .map(|known| known.label)
    }

    /// Also looks keys up in these `.env` files, in order, after the real environment. Only the
    /// variables a provider names as its key are ever taken from them, and nothing in them is
    /// put into the environment of this process or its children.
    pub fn with_env_files(mut self, files: Vec<PathBuf>) -> Self {
        self.env_files = files;
        self
    }

    /// The first of the provider's key variables that a `.env` file sets: its name, the file and
    /// the value.
    fn env_file_key<'a>(&'a self, provider: &'a ProviderConfig) -> Option<(&'a str, &'a Path, &'a str)> {
        let files = self.env_file_vars.get_or_init(|| {
            self.env_files
                .iter()
                .filter_map(|file| {
                    let text = std::fs::read_to_string(file).ok()?;
                    Some((file.clone(), parse_env_file(&text)))
                })
                .collect()
        });
        files.iter().find_map(|(file, vars)| {
            provider.api_key_env.iter().find_map(|name| {
                vars.get(name)
                    .map(|value| (name.as_str(), file.as_path(), value.as_str()))
            })
        })
    }

    pub fn auth_file(&self) -> &Path {
        &self.auth_file
    }

    /// A key given on the command line; lives for the process only.
    pub fn set_flag_key(&self, provider: &str, key: String) {
        locked(&self.flag_keys).insert(provider.to_string(), key);
    }

    /// Order: command-line key, environment, `.env` files, `api_key_command`, a key written in
    /// the configuration, `auth.json`.
    pub async fn resolve(&self, provider: &ProviderConfig) -> Result<Auth, AiError> {
        self.resolve_account(provider).await.map(|(auth, _)| auth)
    }

    /// Like [`Credentials::resolve`], also saying which stored account the credential belongs
    /// to; `None` when it did not come from `auth.json`.
    pub async fn resolve_account(
        &self,
        provider: &ProviderConfig,
    ) -> Result<(Auth, Option<String>), AiError> {
        match self.outside_store(provider).await? {
            Some(auth) => {
                locked(&self.stored).remove(&provider.id);
                return Ok((auth, None));
            }
            None => locked(&self.stored).insert(provider.id.clone()),
        };
        let mut entries = self.load();
        let label = self.selected_in(&entries, &provider.id);
        let key = account_key(&provider.id, label.as_deref().unwrap_or(DEFAULT_ACCOUNT));
        match entries.remove(&key) {
            Some(Credential::ApiKey { key }) => Ok((Auth::ApiKey(key), label)),
            Some(Credential::OAuth { .. }) => {
                let (access, fields) = crate::oauth::refresh::fresh_login(self, provider, &key).await?;
                Ok((Auth::OAuth { access, fields }, label))
            }
            None if !provider.requires_key => Ok((Auth::None, None)),
            None => Err(AiError::Auth(match provider.api_key_env.first() {
                Some(env) => {
                    format!(
                        "no credentials for {id}: set {env} or run `azazel login {id}`",
                        id = provider.id
                    )
                }
                None => format!(
                    "no credentials for {id}: run `azazel login {id}`",
                    id = provider.id
                ),
            })),
        }
    }

    /// The credential that does not depend on a stored account, when there is one.
    async fn outside_store(&self, provider: &ProviderConfig) -> Result<Option<Auth>, AiError> {
        if let Some(key) = locked(&self.flag_keys).get(&provider.id) {
            return Ok(Some(Auth::ApiKey(key.clone())));
        }
        if let Some(key) = provider.api_key_env.iter().find_map(|name| (self.env)(name)) {
            return Ok(Some(Auth::ApiKey(key)));
        }
        if let Some((_, _, key)) = self.env_file_key(provider) {
            return Ok(Some(Auth::ApiKey(key.to_string())));
        }
        if let Some(command) = &provider.api_key_command {
            let cached = locked(&self.command_keys).get(&provider.id).cloned();
            let key = match cached {
                Some(key) => key,
                None => {
                    let key = run_key_command(&provider.id, command).await?;
                    locked(&self.command_keys).insert(provider.id.clone(), key.clone());
                    key
                }
            };
            return Ok(Some(Auth::ApiKey(key)));
        }
        Ok(provider.api_key.clone().map(Auth::ApiKey))
    }

    /// Forget the cached `api_key_command` result; call after the provider answers 401.
    pub fn invalidate(&self, provider: &str) {
        locked(&self.command_keys).remove(provider);
    }

    /// Where the credential would come from, without revealing it.
    pub fn source(&self, provider: &ProviderConfig) -> Option<String> {
        if locked(&self.flag_keys).contains_key(&provider.id) {
            return Some("--api-key".to_string());
        }
        if let Some(name) = provider
            .api_key_env
            .iter()
            .find(|name| (self.env)(name).is_some())
        {
            return Some(format!("env {name}"));
        }
        if let Some((name, file, _)) = self.env_file_key(provider) {
            return Some(format!("{name} in {}", file.display()));
        }
        if provider.api_key_command.is_some() {
            return Some("api_key_command".to_string());
        }
        if provider.api_key.is_some() {
            return Some("api_key in the configuration".to_string());
        }
        let entries = self.load();
        let label = self.selected_in(&entries, &provider.id)?;
        let kind = match entries.get(&account_key(&provider.id, &label))? {
            Credential::ApiKey { .. } => "api key",
            Credential::OAuth { .. } => "login",
        };
        Some(match label.as_str() {
            DEFAULT_ACCOUNT => format!("auth.json ({kind})"),
            label => format!("auth.json ({kind}, account {label})"),
        })
    }

    /// All entries; empty when the file is missing or unreadable.
    pub fn load(&self) -> BTreeMap<String, Credential> {
        read_auth_file(&self.auth_file)
    }

    /// Keeps `field` beside the stored login under `key` (see [`account_key`]), where the next
    /// request finds it among the login's account fields. Nothing happens when there is no login.
    pub fn set_login_field(&self, key: &str, field: &str, value: &str) -> io::Result<()> {
        self.modify(|entries| {
            if let Some(Credential::OAuth { extra, .. }) = entries.get_mut(key) {
                extra.insert(field.to_string(), serde_json::Value::String(value.to_string()));
            }
        })
    }

    pub fn store(&self, id: &str, credential: Credential) -> io::Result<()> {
        self.modify(|entries| {
            entries.insert(id.to_string(), credential);
        })
    }

    /// Returns whether an entry existed.
    pub fn remove(&self, id: &str) -> io::Result<bool> {
        let mut existed = false;
        self.modify(|entries| existed = entries.remove(id).is_some())?;
        Ok(existed)
    }

    /// Read-modify-write under the advisory lock, so concurrent processes never lose an update.
    pub fn modify(&self, f: impl FnOnce(&mut BTreeMap<String, Credential>)) -> io::Result<()> {
        let lock = open_lock(&self.auth_file)?;
        let mut entries = read_auth_file(&self.auth_file);
        f(&mut entries);
        let result = self.write_entries(&entries);
        lock.unlock()?;
        result
    }

    /// The advisory lock of `auth.json`, taken; it is released when the file is dropped. Waiting
    /// for another process happens off the async threads.
    pub(crate) async fn lock_file(&self) -> io::Result<std::fs::File> {
        let auth_file = self.auth_file.clone();
        tokio::task::spawn_blocking(move || open_lock(&auth_file))
            .await
            .map_err(io::Error::other)?
    }

    /// Replaces the file. The caller holds the lock.
    pub(crate) fn write_entries(&self, entries: &BTreeMap<String, Credential>) -> io::Result<()> {
        write_private(&self.auth_file, &serde_json::to_vec_pretty(entries)?)
    }

    pub(crate) fn refreshing(&self) -> &tokio::sync::Mutex<()> {
        &self.refreshing
    }

    /// The client token endpoints are reached with, built when a login first needs refreshing.
    pub(crate) fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(|| {
            crate::tls::client_builder()
                .connect_timeout(Duration::from_secs(30))
                .user_agent(concat!("azazel/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default()
        })
    }
}

/// The accounts of `provider` among `entries`: the default one first, then the others by label.
fn accounts_in(entries: &BTreeMap<String, Credential>, provider: &str) -> Vec<Account> {
    let default = entries
        .get(provider)
        .map(|credential| Account::of(DEFAULT_ACCOUNT, credential));
    let labelled = entries.iter().filter_map(|(key, credential)| {
        let label = key.strip_prefix(provider)?.strip_prefix('#')?;
        Some(Account::of(label, credential))
    });
    default.into_iter().chain(labelled).collect()
}

/// The advisory lock of the credential file `auth_file` (`<name>.json.lock` beside it), taken.
pub(crate) fn open_lock(auth_file: &Path) -> io::Result<std::fs::File> {
    if let Some(dir) = auth_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(auth_file.with_extension("json.lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn read_auth_file(path: &Path) -> BTreeMap<String, Credential> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str(&text) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", path.display());
            BTreeMap::new()
        }
    }
}

/// Temp file in the same directory, then rename; mode 0600 on Unix.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

async fn run_key_command(provider: &str, command: &str) -> Result<String, AiError> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    let fail = |why: String| AiError::Auth(format!("api_key_command for {provider} failed: {why}"));
    let output = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| fail(e.to_string()))?;
    if !output.status.success() {
        return Err(fail(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if key.is_empty() {
        return Err(fail("it printed nothing".to_string()));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WireApi;

    fn provider(requires_key: bool) -> ProviderConfig {
        ProviderConfig {
            api_key_env: vec!["ACME_KEY".into(), "ACME_ALT".into()],
            requires_key,
            ..ProviderConfig::new("acme", WireApi::OpenAiChat, "http://x")
        }
    }

    fn creds(dir: &Path, env: &'static [(&'static str, &'static str)]) -> Credentials {
        Credentials::with_env(
            dir.join("auth.json"),
            Arc::new(move |name| env.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())),
        )
    }

    #[tokio::test]
    async fn keys_come_from_env_files_after_the_environment_and_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home.env");
        let project = dir.path().join("project.env");
        std::fs::write(
            &home,
            "# keys\nexport ACME_ALT=\"from home\"  # the spare\nOTHER=1\nnot a line\n1BAD=x\n",
        )
        .unwrap();
        std::fs::write(
            &project,
            "ACME_KEY='from project'\nACME_ALT=ignored # comment\nEMPTY=\n",
        )
        .unwrap();
        let files = vec![home.clone(), project.clone(), dir.path().join("missing.env")];

        // The home file is asked first, whichever of the provider's variables it sets.
        let from_files = creds(dir.path(), &[]).with_env_files(files.clone());
        assert_eq!(
            from_files.resolve(&provider(true)).await.unwrap(),
            Auth::ApiKey("from home".into())
        );
        assert_eq!(
            from_files.source(&provider(true)),
            Some(format!("ACME_ALT in {}", home.display()))
        );
        let project_only = creds(dir.path(), &[]).with_env_files(vec![project.clone()]);
        assert_eq!(
            project_only.resolve(&provider(true)).await.unwrap(),
            Auth::ApiKey("from project".into())
        );

        // The real environment wins; a key command and a stored key come after the files.
        let with_env = creds(dir.path(), &[("ACME_KEY", "from-env")]).with_env_files(files.clone());
        assert_eq!(
            with_env.resolve(&provider(true)).await.unwrap(),
            Auth::ApiKey("from-env".into())
        );
        assert_eq!(with_env.source(&provider(true)).as_deref(), Some("env ACME_KEY"));
        let mut with_command = provider(true);
        with_command.api_key_command = Some("echo from-command".into());
        assert_eq!(
            from_files.resolve(&with_command).await.unwrap(),
            Auth::ApiKey("from home".into())
        );

        // A provider that names none of the file's variables gets nothing from it.
        let other = ProviderConfig {
            api_key_env: vec!["UNRELATED".into()],
            requires_key: true,
            ..ProviderConfig::new("other", WireApi::OpenAiChat, "http://x")
        };
        assert!(from_files.resolve(&other).await.is_err());
        assert_eq!(from_files.source(&other), None);

        let vars =
            parse_env_file("A=1\n B = two words \nC=\"q # not a comment\"\nD='x' trailing\nE=\"open\n");
        assert_eq!(vars.len(), 4);
        assert_eq!(vars["B"], "two words");
        assert_eq!(vars["C"], "q # not a comment");
        assert_eq!(vars["D"], "x");
    }

    #[tokio::test]
    async fn resolution_order_is_flag_env_command_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = creds(dir.path(), &[("ACME_ALT", "from-env")]);
        c.store(
            "acme",
            Credential::ApiKey {
                key: "from-file".into(),
            },
        )
        .unwrap();

        let mut with_command = provider(true);
        with_command.api_key_command = Some("echo from-command".into());

        assert_eq!(
            c.resolve(&with_command).await.unwrap(),
            Auth::ApiKey("from-env".into())
        );
        c.set_flag_key("acme", "from-flag".into());
        assert_eq!(
            c.resolve(&with_command).await.unwrap(),
            Auth::ApiKey("from-flag".into())
        );

        let no_env = creds(dir.path(), &[]);
        assert_eq!(
            no_env.resolve(&with_command).await.unwrap(),
            Auth::ApiKey("from-command".into())
        );
        assert_eq!(
            no_env.resolve(&provider(true)).await.unwrap(),
            Auth::ApiKey("from-file".into())
        );
        assert_eq!(
            no_env.source(&provider(true)).as_deref(),
            Some("auth.json (api key)")
        );
    }

    #[tokio::test]
    async fn missing_credentials_error_names_env_var_unless_key_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let c = creds(dir.path(), &[]);
        match c.resolve(&provider(true)).await {
            Err(AiError::Auth(msg)) => {
                assert_eq!(
                    msg,
                    "no credentials for acme: set ACME_KEY or run `azazel login acme`"
                )
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(c.resolve(&provider(false)).await.unwrap(), Auth::None);
    }

    #[tokio::test]
    async fn oauth_entry_is_used_until_it_expires() {
        let dir = tempfile::tempdir().unwrap();
        let c = creds(dir.path(), &[]);
        let entry = |expires| Credential::OAuth {
            access: "tok".into(),
            refresh: "r".into(),
            expires,
            extra: BTreeMap::new(),
        };
        c.store("acme", entry(now_ms() + 60_000)).unwrap();
        assert_eq!(
            c.resolve(&provider(true)).await.unwrap(),
            Auth::OAuth {
                access: "tok".into(),
                fields: BTreeMap::new()
            }
        );
        c.store("acme", entry(1)).unwrap();
        assert!(matches!(c.resolve(&provider(true)).await, Err(AiError::Auth(m)) if m.contains("expired")));
    }

    #[test]
    fn store_round_trips_documented_json_shape_and_remove_reports_presence() {
        let dir = tempfile::tempdir().unwrap();
        let c = creds(dir.path(), &[]);
        c.store("a", Credential::ApiKey { key: "k".into() }).unwrap();
        c.store(
            "b",
            Credential::OAuth {
                access: "x".into(),
                refresh: "y".into(),
                expires: 5,
                extra: BTreeMap::new(),
            },
        )
        .unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(c.auth_file()).unwrap()).unwrap();
        assert_eq!(
            raw,
            serde_json::json!({
                "a": {"type": "api_key", "key": "k"},
                "b": {"type": "oauth", "access": "x", "refresh": "y", "expires": 5}
            })
        );
        assert!(c.remove("a").unwrap());
        assert!(!c.remove("a").unwrap());
        assert_eq!(c.load().len(), 1);
    }

    fn login(email: &str, org: &str) -> Credential {
        Credential::OAuth {
            access: format!("tok-{email}"),
            refresh: "r".into(),
            expires: now_ms() + 3_600_000,
            extra: BTreeMap::from([
                ("email".to_string(), serde_json::json!(email)),
                ("org_id".to_string(), serde_json::json!(org)),
            ]),
        }
    }

    #[tokio::test]
    async fn accounts_live_beside_the_default_one_and_old_files_load_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let c = creds(dir.path(), &[]);
        // A file written before accounts existed: one entry under the plain provider id.
        std::fs::write(
            c.auth_file(),
            r#"{"acme": {"type": "api_key", "key": "k-default"}, "acmeish": {"type": "api_key", "key": "other"}}"#,
        )
        .unwrap();
        assert_eq!(
            c.accounts("acme"),
            [Account {
                label: DEFAULT_ACCOUNT.into(),
                login: false,
                email: None,
                org: None
            }]
        );
        assert_eq!(
            c.resolve_account(&provider(true)).await.unwrap(),
            (
                Auth::ApiKey("k-default".into()),
                Some(DEFAULT_ACCOUNT.to_string())
            )
        );

        c.store(&account_key("acme", "work"), login("w@example.com", "org-1"))
            .unwrap();
        c.store(
            &account_key("acme", "alt"),
            Credential::ApiKey { key: "k-alt".into() },
        )
        .unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(c.auth_file()).unwrap()).unwrap();
        assert_eq!(
            raw["acme"],
            serde_json::json!({"type": "api_key", "key": "k-default"})
        );
        assert_eq!(raw["acme#work"]["type"], "oauth");
        assert_eq!(raw["acme#alt"]["key"], "k-alt");

        let labels: Vec<String> = c.accounts("acme").into_iter().map(|a| a.label).collect();
        assert_eq!(labels, ["default", "alt", "work"]);
        assert_eq!(c.accounts("acme")[2].describe(), "work (w@example.com)");
        assert!(c.accounts("nobody").is_empty());

        // The same e-mail in the same organisation is the same account; another organisation
        // is not.
        assert_eq!(
            c.same_account("acme", &login("w@example.com", "org-1"))
                .as_deref(),
            Some("work")
        );
        assert_eq!(c.same_account("acme", &login("w@example.com", "org-2")), None);
        assert_eq!(
            c.same_account("acme", &Credential::ApiKey { key: "k-alt".into() }),
            None
        );
    }

    #[tokio::test]
    async fn the_account_is_the_selected_one_then_the_configured_one_then_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let c = creds(dir.path(), &[]);
        for (label, key) in [("default", "k-default"), ("work", "k-work"), ("home", "k-home")] {
            c.store(
                &account_key("acme", label),
                Credential::ApiKey { key: key.into() },
            )
            .unwrap();
        }
        let used = async |c: &Credentials| c.resolve_account(&provider(true)).await.unwrap();

        assert_eq!(c.selected("acme").as_deref(), Some("default"));
        assert_eq!(c.source(&provider(true)).as_deref(), Some("auth.json (api key)"));

        c.prefer("acme", Some("work"));
        assert_eq!(
            used(&c).await,
            (Auth::ApiKey("k-work".into()), Some("work".to_string()))
        );
        assert_eq!(
            c.source(&provider(true)).as_deref(),
            Some("auth.json (api key, account work)")
        );

        c.select("acme", "home");
        assert_eq!(used(&c).await.0, Auth::ApiKey("k-home".into()));
        assert!(c.uses_store("acme"));

        // A selection that names no stored account falls through to the next rule.
        c.select("acme", "gone");
        assert_eq!(c.selected("acme").as_deref(), Some("work"));
        c.prefer("acme", None);
        assert_eq!(c.selected("acme").as_deref(), Some("default"));

        // Without a default account the first labelled one answers.
        c.remove("acme").unwrap();
        assert_eq!(c.selected("acme").as_deref(), Some("home"));

        // A key from the environment is the same whatever is selected.
        let env = creds(dir.path(), &[("ACME_KEY", "from-env")]);
        assert_eq!(used(&env).await, (Auth::ApiKey("from-env".into()), None));
        assert!(!env.uses_store("acme"));
    }

    #[test]
    fn account_labels_are_plain_words() {
        assert_eq!(account_key("acme", DEFAULT_ACCOUNT), "acme");
        assert_eq!(account_key("acme", "work"), "acme#work");
        assert!(check_label("work-2.b_c").is_ok());
        for bad in ["", "a b", "a#b", "default", "é"] {
            assert!(check_label(bad).is_err(), "{bad}");
        }
    }
}
