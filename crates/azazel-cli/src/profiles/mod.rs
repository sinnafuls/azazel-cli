//! Profiles: named sets of role chains that switch the whole team between, say, a cheap and a
//! thorough set of models. The built-in ones are in [`builtin`]; `[profiles.<name>.roles]` in a
//! config file adds more, or replaces a built-in of the same name whole.

use std::collections::BTreeMap;

use azazel_agent::AgentHandle;
use azazel_ai::providers::presets::ProviderConfig;
use azazel_ai::{Credentials, ModelSpec, Router};
use azazel_core::config::{Layer, LayerKind, ProfileConfig};
use azazel_core::{Config, EntryKind, SessionStore};

pub mod builtin;
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

/// Whether the logins a profile needs are there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usability {
    pub usable: bool,
    /// Providers the profile names that have no credentials, sorted.
    pub missing_logins: Vec<String>,
}

/// No profile has the name asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownProfile {
    pub name: String,
    pub known: Vec<String>,
}

impl std::fmt::Display for UnknownProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown profile `{}`; known: {}",
            self.name,
            self.known.join(", ")
        )
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

/// Every profile: the built-in ones in their order, a config entry of the same name replacing a
/// built-in whole, then the other config entries by name. Every config entry counts as the
/// user's; [`all_in`] tells a project's from the user's.
pub fn all(config: &Config) -> Vec<Profile> {
    all_in(&config.profiles.entries, &[])
}

/// [`all`] over `entries`, with the config `layers` they were read from to tell a project's
/// profile from the user's.
pub fn all_in(entries: &BTreeMap<String, ProfileConfig>, layers: &[Layer]) -> Vec<Profile> {
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
    resolve_in(&config.profiles.entries, name)
}

/// [`resolve`] over `entries`.
pub fn resolve_in(entries: &BTreeMap<String, ProfileConfig>, name: &str) -> Result<Profile, UnknownProfile> {
    let mut profiles = all_in(entries, &[]);
    match profiles.iter().position(|profile| profile.name == name) {
        Some(index) => Ok(profiles.swap_remove(index)),
        None => Err(UnknownProfile {
            name: name.to_string(),
            known: profiles.into_iter().map(|profile| profile.name).collect(),
        }),
    }
}

/// Which providers `profile` names that `logged_in` says have no credentials.
pub fn usability(profile: &Profile, logged_in: &dyn Fn(&str) -> bool) -> Usability {
    let mut missing: Vec<String> = profile
        .roles
        .values()
        .flatten()
        .filter_map(|spec| match spec {
            ModelSpec::Model(model) => Some(model.provider.clone()),
            ModelSpec::Role(_) => None,
        })
        .filter(|provider| !logged_in(provider))
        .collect();
    missing.sort();
    missing.dedup();
    Usability {
        usable: missing.is_empty(),
        missing_logins: missing,
    }
}

/// The check `azazel models` and the model picker use for "has a credential" (`models::usable`),
/// as the function [`usability`] takes. A provider that is not an HTTP endpoint (an extension's)
/// finds its own credentials and counts as available.
pub fn logged_in<'a>(
    providers: &'a [ProviderConfig],
    credentials: &'a Credentials,
) -> impl Fn(&str) -> bool + 'a {
    move |id| {
        providers
            .iter()
            .find(|provider| provider.id == id)
            .is_none_or(|provider| crate::models::usable(credentials, provider))
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
/// profile does not name stay as they are. Returns what was done, in a line.
pub(crate) fn switch(
    profile: &Profile,
    router: &Router,
    lead: Option<(&AgentHandle, Option<&SessionStore>)>,
    reason: Reason,
) -> String {
    let session = lead.and_then(|(_, session)| session);
    let done = crate::routing::apply_roles(router, &profile.roles, lead);
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
        let discovered = ["openai-codex"];
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
                    let known = catalog.contains(&model.provider, &model.model)
                        || (discovered.contains(&model.provider.as_str())
                            && catalog.contains("openai", &model.model));
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
        let sources: Vec<_> = all_in(&entries, &layers)
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
        let only_anthropic = |id: &str| id == "anthropic";
        assert_eq!(
            usability(&gpt, &only_anthropic),
            Usability {
                usable: false,
                missing_logins: vec!["openai-codex".to_string()],
            }
        );
        assert!(usability(&claude, &only_anthropic).usable);
        let nobody = |_: &str| false;
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
