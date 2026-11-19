//! Profiles: named sets of role chains that switch the whole team between, say, a cheap and a
//! thorough set of models. The built-in ones are in [`builtin`]; `[profiles.<name>.roles]` in a
//! config file adds more, or replaces a built-in of the same name whole.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use azazel_agent::AgentHandle;
use azazel_ai::providers::presets::ProviderConfig;
use azazel_ai::{Credentials, ModelSpec, Router};
use azazel_core::config::{Layer, LayerKind, ProfileConfig};
use azazel_core::{Config, EntryKind, SessionStore};
use azazel_team::Team;

pub mod builtin;
pub mod edit;
pub mod limit;

/// A named set of role chains.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub source: ProfileSource,
    pub roles: BTreeMap<String, Vec<ModelSpec>>,
}

/// Where a profile comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSource {
    /// Ships with azazel.
    Builtin,
    /// Written in the home config.
    User,
    /// Written in the project's config.
    Project,
}

impl ProfileSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileSource::Builtin => "builtin",
            ProfileSource::User => "user",
            ProfileSource::Project => "project",
        }
    }
}

/// Whether a provider a profile names can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// It has credentials, or finds its own.
    Ready,
    /// It is a provider of this process that has no credentials.
    NoLogin,
    /// No provider of this process has that id: a typo, or an extension that is not installed.
    Unknown,
}

/// What a provider id means to a profile, as [`usability`] asks it.
pub type AccessCheck = Arc<dyn Fn(&str) -> Access + Send + Sync>;

/// Whether the providers a profile names are there and logged in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usability {
    pub usable: bool,
    /// Providers the profile names that have no credentials, sorted.
    pub missing_logins: Vec<String>,
    /// Providers the profile names that no registered provider has, sorted.
    pub unknown_providers: Vec<String>,
}

impl Usability {
    /// What stops the profile from being used, as one sentence; `None` when it can be used.
    pub fn problem(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.unknown_providers.is_empty() {
            let names: Vec<String> = self
                .unknown_providers
                .iter()
                .map(|id| format!("`{id}`"))
                .collect();
            let noun = if names.len() == 1 { "provider" } else { "providers" };
            parts.push(format!("names an unknown {noun} {}", names.join(" and ")));
        }
        if let Some(login) = crate::routing::needs_login(&self.missing_logins) {
            parts.push(login);
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// No profile has the name asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownProfile {
    pub name: String,
    pub known: Vec<String>,
    /// The profile exists but `[profiles] disabled` switches it off.
    pub disabled: bool,
}

impl std::fmt::Display for UnknownProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.disabled {
            true => write!(
                f,
                "profile `{}` is disabled (profiles.disabled; enable it on the Profiles page of /models); known: {}",
                self.name,
                self.known.join(", ")
            ),
            false => write!(
                f,
                "unknown profile `{}`; known: {}",
                self.name,
                self.known.join(", ")
            ),
        }
    }
}

impl std::error::Error for UnknownProfile {}

/// Why a switch happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The user asked (`/profile`, an RPC command).
    User,
    /// The provider of the profile reported a usage limit.
    Limit,
    /// The limit window has reset.
    Reset,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::User => "user",
            Reason::Limit => "limit",
            Reason::Reset => "reset",
        }
    }
}

/// The built-in profiles.
pub fn builtin() -> Vec<Profile> {
    builtin::all()
}

/// The profiles that can be used: the built-in ones in their order, a config entry of the same
/// name replacing a built-in whole, then the other config entries by name, without those that
/// `[profiles] disabled` names. Every config entry counts as the user's; [`all_in`] tells a
/// project's from the user's.
pub fn all(config: &Config) -> Vec<Profile> {
    all_in(&config.profiles.entries, &config.profiles.disabled, &[])
}

/// [`all`] over `entries` without the `disabled` ones, with the config `layers` they were read
/// from to tell a project's profile from the user's.
pub fn all_in(entries: &BTreeMap<String, ProfileConfig>, disabled: &[String], layers: &[Layer]) -> Vec<Profile> {
    let mut profiles = every_in(entries, layers);
    profiles.retain(|profile| !disabled.contains(&profile.name));
    profiles
}

/// [`all_in`] with the disabled profiles too, for the page that switches them on again.
pub fn every_in(entries: &BTreeMap<String, ProfileConfig>, layers: &[Layer]) -> Vec<Profile> {
    let from_config = |name: &str, entry: &ProfileConfig| Profile {
        name: name.to_string(),
        source: source_of(layers, name),
        roles: entry.roles.clone(),
    };
    let mut out: Vec<Profile> = builtin()
        .into_iter()
        .map(|built| match entries.get(&built.name) {
            Some(entry) => from_config(&built.name, entry),
            None => built,
        })
        .collect();
    let shipped = out.len();
    let more: Vec<Profile> = entries
        .iter()
        .filter(|(name, _)| !out[..shipped].iter().any(|profile| &profile.name == *name))
        .map(|(name, entry)| from_config(name, entry))
        .collect();
    out.extend(more);
    out
}

/// The last config layer that has the profile (or its old name, the preset) decides.
fn source_of(layers: &[Layer], name: &str) -> ProfileSource {
    let has = |layer: &Layer| {
        ["profiles", "presets"].iter().any(|table| {
            layer
                .table
                .get(*table)
                .and_then(|entries| entries.get(name))
                .is_some_and(|entry| entry.is_table())
        })
    };
    match layers
        .iter()
        .rev()
        .find(|layer| has(layer))
        .map(|layer| layer.kind)
    {
        Some(LayerKind::Project | LayerKind::ProjectLocal) => ProfileSource::Project,
        _ => ProfileSource::User,
    }
}

/// The profile `name`, or what the known ones are.
pub fn resolve(config: &Config, name: &str) -> Result<Profile, UnknownProfile> {
    resolve_in(&config.profiles.entries, &config.profiles.disabled, name)
}

/// [`resolve`] over `entries` without the `disabled` ones.
pub fn resolve_in(
    entries: &BTreeMap<String, ProfileConfig>,
    disabled: &[String],
    name: &str,
) -> Result<Profile, UnknownProfile> {
    let every = every_in(entries, &[]);
    let known = || {
        every
            .iter()
            .filter(|profile| !disabled.contains(&profile.name))
            .map(|profile| profile.name.clone())
            .collect()
    };
    match every.iter().find(|profile| profile.name == name) {
        Some(profile) if !disabled.contains(&profile.name) => Ok(profile.clone()),
        found => Err(UnknownProfile {
            name: name.to_string(),
            known: known(),
            disabled: found.is_some(),
        }),
    }
}

/// Which providers `profile` names that `access` says have no credentials or do not exist.
pub fn usability(profile: &Profile, access: &dyn Fn(&str) -> Access) -> Usability {
    let mut named: Vec<&str> = profile
        .roles
        .values()
        .flatten()
        .filter_map(|spec| match spec {
            ModelSpec::Model(model) => Some(model.provider.as_str()),
            ModelSpec::Role(_) => None,
        })
        .collect();
    named.sort_unstable();
    named.dedup();
    let mut missing_logins = Vec::new();
    let mut unknown_providers = Vec::new();
    for id in named {
        match access(id) {
            Access::Ready => {}
            Access::NoLogin => missing_logins.push(id.to_string()),
            Access::Unknown => unknown_providers.push(id.to_string()),
        }
    }
    Usability {
        usable: missing_logins.is_empty() && unknown_providers.is_empty(),
        missing_logins,
        unknown_providers,
    }
}

/// The check `azazel models` and the model picker use for "has a credential" (`models::usable`),
/// as the function [`usability`] takes. `registered` says whether the router has a provider of
/// that id (`None` while it is not built yet). A registered provider that is not an HTTP
/// endpoint (an extension's, `mock`) finds its own credentials and counts as available; an id
/// no provider has is [`Access::Unknown`].
pub fn access(
    providers: Vec<ProviderConfig>,
    credentials: Arc<Credentials>,
    registered: impl Fn(&str) -> Option<bool> + Send + Sync + 'static,
) -> impl Fn(&str) -> Access + Send + Sync + 'static {
    move |id| {
        if registered(id) == Some(false) {
            return Access::Unknown;
        }
        match providers.iter().find(|provider| provider.id == id) {
            Some(provider) if !crate::models::usable(&credentials, provider) => Access::NoLogin,
            _ => Access::Ready,
        }
    }
}

/// `--profile`: the profile's roles replace the configured ones for this run.
pub fn apply(config: &mut Config, name: &str) -> Result<Profile, UnknownProfile> {
    let profile = resolve(config, name)?;
    config.roles.extend(profile.roles.clone());
    Ok(profile)
}

/// The one way a running session moves to a profile (`/profile`, RPC, an automatic switch): every
/// role of the profile points at its models, the lead follows its role again when the profile
/// sets it, and the switch is written into the session so that a resumed one shows it. Roles the
/// profile does not name stay as they are, and so do the models of sub-agents that are running.
/// Returns what was done, in a line.
pub(crate) fn switch(
    profile: &Profile,
    router: &Router,
    lead: Option<(&AgentHandle, Option<&SessionStore>)>,
    team: Option<&Team>,
    reason: Reason,
) -> String {
    let session = lead.and_then(|(_, session)| session);
    let done = crate::routing::apply_roles(router, &profile.roles, lead, team);
    if let Some(session) = session
        && session.last_profile(None).as_deref() != Some(profile.name.as_str())
    {
        session.append(EntryKind::ProfileChange {
            profile: profile.name.clone(),
        });
    }
    let because = match reason {
        Reason::User => "",
        Reason::Limit => " (usage limit reached)",
        Reason::Reset => " (the limit has reset)",
    };
    format!("profile {}{because}: {done}", profile.name)
}

/// A profile as the Profiles page shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub profile: Profile,
    pub usability: Usability,
    /// Not listed in `[profiles] disabled`.
    pub enabled: bool,
    /// One of the profiles that ship with azazel (its roles may be overridden).
    pub builtin: bool,
}

/// What an edit came to: the line to tell the user, and the profile to apply to the session
/// when the edit changed the one it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edited {
    pub line: String,
    pub reapply: Option<String>,
}

struct Held {
    /// Every profile, the disabled ones too.
    profiles: Vec<Profile>,
    disabled: Vec<String>,
}

/// The profiles of this process (the built-in ones, those of the configuration and those saved
/// since), and whether their providers have a login. A service: `/profile`, RPC and the
/// terminal's picker all switch through it, so that they agree on what can be used.
pub struct Book {
    held: Mutex<Held>,
    access: AccessCheck,
    /// The profile `--profile` named, which is the current one until a switch is recorded.
    launch: Option<String>,
    /// `[profiles] on_limit`, which an edit must leave usable.
    on_limit: String,
}

impl Book {
    /// `profiles` are all of them, the ones `disabled` names included.
    pub fn new(
        profiles: Vec<Profile>,
        disabled: Vec<String>,
        access: AccessCheck,
        launch: Option<String>,
    ) -> Book {
        Book {
            held: Mutex::new(Held { profiles, disabled }),
            access,
            launch,
            on_limit: String::new(),
        }
    }

    pub fn with_on_limit(mut self, on_limit: &str) -> Book {
        self.on_limit = on_limit.trim().to_string();
        self
    }

    fn lock(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The profiles that can be used: those not disabled.
    pub fn list(&self) -> Vec<Profile> {
        let held = self.lock();
        held.profiles
            .iter()
            .filter(|profile| !held.disabled.contains(&profile.name))
            .cloned()
            .collect()
    }

    /// Every profile with what the Profiles page shows of it, the disabled ones too.
    pub fn entries(&self) -> Vec<Entry> {
        let held = self.lock();
        held.profiles
            .iter()
            .map(|profile| Entry {
                usability: usability(profile, &*self.access),
                enabled: !held.disabled.contains(&profile.name),
                builtin: azazel_core::config::BUILTIN_PROFILES.contains(&profile.name.as_str()),
                profile: profile.clone(),
            })
            .collect()
    }

    pub fn find(&self, name: &str) -> Result<Profile, String> {
        let profiles = self.list();
        profiles
            .iter()
            .find(|profile| profile.name == name)
            .cloned()
            .ok_or_else(|| {
                let known: Vec<&str> = profiles.iter().map(|profile| profile.name.as_str()).collect();
                format!("no profile `{name}`; there are: {}", known.join(", "))
            })
    }

    /// Whether a provider exists and has a login, for the code that decides on its own (a usage
    /// limit).
    pub fn access_fn(&self) -> AccessCheck {
        self.access.clone()
    }

    pub fn usability(&self, profile: &Profile) -> Usability {
        usability(profile, &*self.access)
    }

    /// The profile the session is on: the last switch recorded in it, else the one `--profile`
    /// named.
    pub fn current(&self, session: &SessionStore) -> Option<String> {
        session.last_profile(None).or_else(|| self.launch.clone())
    }

    /// Every profile as the protocol shows it.
    pub fn infos(&self) -> Vec<azazel_protocol::ProfileInfo> {
        self.list()
            .into_iter()
            .map(|profile| {
                let usable = self.usability(&profile);
                azazel_protocol::ProfileInfo {
                    name: profile.name,
                    source: profile.source.as_str().to_string(),
                    usable: usable.usable,
                    missing_logins: usable.missing_logins,
                    unknown_providers: usable.unknown_providers,
                    roles: profile
                        .roles
                        .iter()
                        .filter_map(|(role, chain)| Some((role.clone(), chain.first()?.to_string())))
                        .collect(),
                }
            })
            .collect()
    }

    /// Writes `roles` as profile `name` in `home_config`, and makes it one of the profiles here,
    /// replacing one of that name.
    pub fn save(
        &self,
        home_config: &Path,
        name: &str,
        roles: &BTreeMap<String, Vec<ModelSpec>>,
    ) -> Result<(), String> {
        crate::routing::profile_name(name)?;
        let change = edit::Change::Write {
            name: name.to_string(),
            roles: roles.clone(),
        };
        let mut held = self.lock();
        self.write(&mut held, home_config, &change)
    }

    /// One atomic write of `change` into the home config, then into the profiles held here.
    fn write(&self, held: &mut Held, home_config: &Path, change: &edit::Change) -> Result<(), String> {
        crate::init::edit_config(home_config, |doc| {
            edit::write_doc(doc, change).map_err(|e| anyhow::anyhow!(e))
        })
        .map_err(|e| format!("{e:#}"))?;
        edit::write_state(&mut held.profiles, &mut held.disabled, change);
        Ok(())
    }

    /// Carries out `edit` on the home config in one atomic write, and on the profiles held here.
    /// `current` is the roles of the session (for a profile made from them) and `on` the profile
    /// the session is on, which comes back as [`Edited::reapply`] when the edit changed it.
    pub fn edit(
        &self,
        home_config: &Path,
        edit: &edit::Edit,
        current: &BTreeMap<String, Vec<ModelSpec>>,
        on: Option<&str>,
    ) -> Result<Edited, String> {
        let mut held = self.lock();
        let change = edit::plan(
            &edit::Known {
                profiles: &held.profiles,
                disabled: &held.disabled,
                on_limit: &self.on_limit,
                current,
            },
            edit,
        )?;
        self.write(&mut held, home_config, &change)?;
        let (line, touched) = edit::describe(&change);
        let reapply = touched
            .filter(|name| on == Some(name.as_str()))
            .map(|name| match &change {
                // The session's record names the new name from now on.
                edit::Change::Rename { to, .. } => to.clone(),
                _ => name,
            });
        Ok(Edited { line, reapply })
    }

    /// Moves the session to profile `name` with [`switch`], unless a provider it names has no
    /// login or does not exist. Returns the line [`switch`] gives.
    pub(crate) fn switch(
        &self,
        name: &str,
        router: &Router,
        lead: Option<(&AgentHandle, Option<&SessionStore>)>,
        team: Option<&Team>,
        reason: Reason,
    ) -> Result<String, String> {
        let profile = self.find(name)?;
        let usable = self.usability(&profile);
        if let Some(why) = usable.problem() {
            return Err(format!("profile {name} {why}"));
        }
        Ok(switch(&profile, router, lead, team, reason))
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_ai::Catalog;

    use super::*;

    fn chain(specs: &[&str]) -> Vec<ModelSpec> {
        specs.iter().map(|spec| spec.parse().unwrap()).collect()
    }

    fn entry(role: &str, model: &str) -> ProfileConfig {
        ProfileConfig {
            roles: BTreeMap::from([(role.to_string(), chain(&[model]))]),
        }
    }

    #[test]
    fn the_built_in_names_are_the_ones_the_configuration_accepts_for_on_limit() {
        let names: Vec<String> = builtin().into_iter().map(|profile| profile.name).collect();
        assert_eq!(names, azazel_core::config::BUILTIN_PROFILES);
    }

    #[test]
    fn every_built_in_role_names_a_real_role_and_model() {
        let catalog = Catalog::embedded();
        // The ChatGPT subscription lists its own models, so the catalogue holds them only under
        // `openai`; the router discovers them under `openai-codex`.
        for profile in builtin() {
            assert!(profile.roles.len() >= 8, "{}: {:?}", profile.name, profile.roles);
            for (role, chain) in &profile.roles {
                assert!(
                    crate::roles::is_known_role(role),
                    "{}: no role `{role}`",
                    profile.name
                );
                for spec in chain {
                    let ModelSpec::Model(model) = spec else {
                        panic!("{}: {role} names a role", profile.name)
                    };
                    let known = match model.provider.as_str() {
                        "anthropic" => catalog.contains("anthropic", &model.model),
                        "openai-codex" => catalog.contains("openai", &model.model),
                        other => panic!("{}: {role} names provider `{other}`", profile.name),
                    };
                    assert!(
                        known,
                        "{}: {role} = {model} is not in the catalogue",
                        profile.name
                    );
                }
            }
        }
    }

    #[test]
    fn the_built_ins_tier_their_models() {
        let profiles = all(&Config::default());
        let by = |name: &str| profiles.iter().find(|p| p.name == name).unwrap().clone();
        let claude = by("claude");
        assert_eq!(claude.source, ProfileSource::Builtin);
        assert_eq!(claude.roles["default"], chain(&["anthropic/claude-opus-5-5"]));
        assert_eq!(claude.roles["smol"], chain(&["anthropic/claude-haiku-5-5"]));
        let gpt = by("gpt");
        assert_eq!(gpt.roles["slow"], chain(&["openai-codex/gpt-6-astra"]));
        assert_eq!(gpt.roles["smol"], chain(&["openai-codex/gpt-6-luna"]));
        assert_eq!(
            by("research").roles["scout"],
            chain(&["anthropic/claude-opus-5-5"])
        );
        assert_eq!(
            by("frontend").roles["designer"],
            chain(&["anthropic/claude-fable-5-2"])
        );
    }

    #[test]
    fn a_configured_profile_replaces_a_built_in_whole_and_others_follow_by_name() {
        let mut config = Config::default();
        config
            .profiles
            .entries
            .insert("cheap".to_string(), entry("default", "acme/small"));
        config
            .profiles
            .entries
            .insert("mine".to_string(), entry("default", "acme/mine"));
        let profiles = all(&config);
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "claude", "gpt", "cheap", "thorough", "frontend", "research", "mine"
            ]
        );
        let cheap = resolve(&config, "cheap").unwrap();
        assert_eq!(cheap.source, ProfileSource::User);
        assert_eq!(cheap.roles.keys().collect::<Vec<_>>(), ["default"]);
        assert_eq!(resolve(&config, "claude").unwrap().source, ProfileSource::Builtin);

        let error = resolve(&config, "dear").unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown profile `dear`; known: claude, gpt, cheap, thorough, frontend, research, mine"
        );
    }

    #[test]
    fn a_project_layer_makes_a_project_profile() {
        let layer = |kind, text: &str| Layer {
            kind,
            path: "config.toml".into(),
            table: text.parse().unwrap(),
        };
        let layers = [
            layer(LayerKind::Home, "[profiles.a.roles]\ndefault = [\"acme/a\"]\n"),
            layer(LayerKind::Project, "[presets.b.roles]\ndefault = [\"acme/b\"]\n"),
        ];
        let entries = BTreeMap::from([
            ("a".to_string(), entry("default", "acme/a")),
            ("b".to_string(), entry("default", "acme/b")),
        ]);
        let sources: Vec<_> = all_in(&entries, &[], &layers)
            .into_iter()
            .skip(6)
            .map(|p| (p.name, p.source.as_str()))
            .collect();
        assert_eq!(sources, [("a".to_string(), "user"), ("b".to_string(), "project")]);
    }

    #[test]
    fn usability_names_the_providers_without_a_login() {
        let config = Config::default();
        let gpt = resolve(&config, "gpt").unwrap();
        let claude = resolve(&config, "claude").unwrap();
        let only_anthropic = |id: &str| match id {
            "anthropic" => Access::Ready,
            _ => Access::NoLogin,
        };
        assert_eq!(
            usability(&gpt, &only_anthropic),
            Usability {
                usable: false,
                missing_logins: vec!["openai-codex".to_string()],
                unknown_providers: Vec::new(),
            }
        );
        assert!(usability(&claude, &only_anthropic).usable);
        let nobody = |_: &str| Access::NoLogin;
        assert_eq!(usability(&claude, &nobody).missing_logins, ["anthropic"]);
        // Roles that name only other roles need nothing.
        let alias = Profile {
            name: "x".to_string(),
            source: ProfileSource::User,
            roles: BTreeMap::from([("smol".to_string(), chain(&["@default"]))]),
        };
        assert!(usability(&alias, &nobody).usable);
    }

    #[test]
    fn a_provider_nobody_registered_makes_a_profile_unusable() {
        let profile = Profile {
            name: "typo".to_string(),
            source: ProfileSource::User,
            roles: BTreeMap::from([("default".to_string(), chain(&["anthropc/claude-x", "acme/a"]))]),
        };
        let credentials = Arc::new(Credentials::new(std::path::PathBuf::from("auth.json")));
        // The router has `acme` (an extension's provider, no HTTP endpoint) and `mock`.
        let check = access(Vec::new(), credentials.clone(), |id| {
            Some(matches!(id, "acme" | "mock"))
        });
        let usable = usability(&profile, &check);
        assert!(!usable.usable);
        assert_eq!(usable.unknown_providers, ["anthropc"]);
        assert!(usable.missing_logins.is_empty());
        assert_eq!(usable.problem().unwrap(), "names an unknown provider `anthropc`");

        let fine = Profile {
            roles: BTreeMap::from([("default".to_string(), chain(&["acme/a", "mock/lead"]))]),
            ..profile
        };
        assert!(usability(&fine, &check).usable);
        // Before the router exists nothing is called unknown.
        let early = access(Vec::new(), credentials, |_| None);
        assert!(usability(&fine, &early).usable);
    }

    #[test]
    fn a_profile_named_at_launch_replaces_the_configured_roles() {
        let mut config = Config::default();
        config
            .profiles
            .entries
            .insert("small".to_string(), entry("default", "acme/small"));
        apply(&mut config, "small").unwrap();
        assert_eq!(config.roles["default"], chain(&["acme/small"]));
        // The roles it does not name keep what they had.
        assert_eq!(config.roles["smol"], chain(&["@default"]));
        apply(&mut config, "cheap").unwrap();
        assert_eq!(config.roles["smol"], chain(&["anthropic/claude-haiku-5-5"]));
        assert!(
            apply(&mut config, "dear")
                .unwrap_err()
                .to_string()
                .starts_with("unknown profile `dear`")
        );
    }
}
