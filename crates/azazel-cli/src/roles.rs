//! Model roles: each role's chain with where it comes from and what it resolves to, changing a
//! role for the session or in a config file, and presets. The `/models` screen, RPC and
//! `azazel roles` all go through here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use azazel_agent::AgentHandle;
use azazel_ai::{ModelSpec, Router};
use azazel_core::config::{Layer, LayerKind};
use azazel_core::{Config, LoadOptions, Paths, SessionStore};
use azazel_protocol::{RoleRow, RoleSource, Roles, SaveTo};

use crate::app::App;

/// What a change to the roles needs from the running process.
pub struct Place<'a> {
    pub paths: &'a Paths,
    pub load: &'a LoadOptions,
    /// Whether the project's config may be written.
    pub trusted: bool,
    pub router: &'a Router,
    /// The lead and its session, which follow `default` and `lead` when those change.
    pub lead: Option<(&'a AgentHandle, Option<&'a SessionStore>)>,
}

impl<'a> Place<'a> {
    pub fn of(app: &'a App, lead: Option<(&'a AgentHandle, Option<&'a SessionStore>)>) -> Place<'a> {
        Place {
            paths: &app.paths,
            load: &app.load_options,
            trusted: app.trusted,
            router: &app.router,
            lead,
        }
    }
}

/// The configuration as the files say it now, and the files. A configuration that no longer
/// loads gives the built-in roles and no files, so the roles can still be listed and fixed.
fn files(place: &Place<'_>) -> (Config, Vec<Layer>) {
    match Config::load(place.paths, place.load) {
        Ok(loaded) => (loaded.config, loaded.layers),
        Err(e) => {
            tracing::warn!("cannot read the configuration again: {e}");
            (Config::default(), Vec::new())
        }
    }
}

/// The chain the last file that sets `role` gives it, and which file that is.
fn from_files(layers: &[Layer], role: &str) -> Option<LayerKind> {
    layers
        .iter()
        .rev()
        .find(|layer| {
            layer
                .table
                .get("roles")
                .and_then(|roles| roles.get(role))
                .is_some()
        })
        .map(|layer| layer.kind)
}

fn strings(chain: &[ModelSpec]) -> Vec<String> {
    chain.iter().map(ToString::to_string).collect()
}

/// Every role the session knows, built-in ones first in a fixed order, then the others by name.
pub fn list(place: &Place<'_>) -> Roles {
    let (config, layers) = files(place);
    let builtin = Config::default().roles;
    let live = place.router.roles();
    let mut names: Vec<&String> = live.keys().chain(config.roles.keys()).collect();
    names.sort_by_key(|name| (order(name), name.as_str()));
    names.dedup();
    let roles = names
        .into_iter()
        .map(|role| {
            let chain = live.get(role).cloned().unwrap_or_default();
            let filed = config.roles.get(role);
            let source = match (from_files(&layers, role), filed) {
                _ if chain.is_empty() && !live.contains_key(role) => RoleSource::Unset,
                (Some(kind), Some(filed)) if *filed == chain => match kind {
                    LayerKind::Home => RoleSource::Home,
                    LayerKind::Project => RoleSource::Project,
                    LayerKind::ProjectLocal => RoleSource::ProjectLocal,
                },
                (None, _) if builtin.get(role) == Some(&chain) => RoleSource::Builtin,
                _ => RoleSource::Session,
            };
            let thinking = match chain.first() {
                Some(ModelSpec::Model(model)) => model.thinking.map(|level| level.to_string()),
                _ => None,
            };
            RoleRow {
                role: role.clone(),
                resolved: place
                    .router
                    .resolve(&chain)
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                chain: strings(&chain),
                source,
                builtin: builtin.get(role).map(|chain| strings(chain)).unwrap_or_default(),
                thinking,
            }
        })
        .collect();
    Roles {
        roles,
        presets: config.presets.keys().cloned().collect(),
        project: place.trusted,
    }
}

/// Where a role goes in the list: the general roles, then the lead and the built-in agents,
/// then the user's own roles.
fn order(role: &str) -> usize {
    const ORDER: [&str; 20] = [
        "default",
        "smol",
        "slow",
        "plan",
        "vision",
        "verify",
        "memory",
        "judge",
        "lead",
        "scout",
        "planner",
        "coder",
        "tester",
        "reviewer",
        "verifier",
        "researcher",
        "designer",
        "artist",
        "playtester",
        "advisor",
    ];
    ORDER
        .iter()
        .position(|known| *known == role)
        .unwrap_or(ORDER.len())
}

/// The config file `save` writes.
fn target(place: &Place<'_>, save: SaveTo) -> Result<Option<PathBuf>, String> {
    match save {
        SaveTo::Session => Ok(None),
        SaveTo::Home => Ok(Some(place.paths.home_config())),
        SaveTo::Project if place.trusted => Ok(Some(place.paths.project_config())),
        SaveTo::Project => Err(
            "this project is not trusted, so its config is not written: save to the home config, or trust the project (/trust yes)"
                .to_string(),
        ),
    }
}

/// Points `role` at `chain`, or with `None` takes away the value `save` names: the role then
/// falls back to the next file, or its built-in value. The running session follows at once,
/// and the lead goes back to following its role when `default` or `lead` changes.
pub fn set(
    place: &Place<'_>,
    role: &str,
    chain: Option<Vec<ModelSpec>>,
    save: SaveTo,
) -> Result<Roles, String> {
    if !crate::routing::plain_name(role) {
        return Err(format!(
            "`{role}` cannot name a role: use letters, digits, `-` and `_`, starting with a letter"
        ));
    }
    if let Some(chain) = &chain {
        if chain.is_empty() {
            return Err("give at least one model, or clear the role".to_string());
        }
        if chain
            .iter()
            .any(|spec| matches!(spec, ModelSpec::Role(name) if name == role))
        {
            return Err(format!("`{role}` cannot fall back to itself"));
        }
    }
    if let Some(file) = target(place, save)? {
        let value = chain
            .as_ref()
            .map(|chain| toml_edit::value(toml_edit::Array::from_iter(strings(chain))));
        crate::init::set_config_value(&file, &["roles", role], value).map_err(|e| format!("{e:#}"))?;
    }
    let now = match chain {
        Some(chain) => Some(chain),
        // What is left once the value is gone: the files as they are now, else the built-in.
        None => files(place).0.roles.remove(role),
    };
    match now {
        Some(chain) => place.router.set_role(role, chain),
        None => place.router.remove_role(role),
    }
    if matches!(role, "default" | "lead")
        && let Some((lead, session)) = place.lead
    {
        crate::routing::follow_role(lead, session);
    }
    Ok(list(place))
}

/// Writes every role as it is now as preset `name` in the home config.
pub fn save_preset(place: &Place<'_>, name: &str) -> Result<Roles, String> {
    crate::routing::save_roles(&place.paths.home_config(), name, &place.router.roles())?;
    Ok(list(place))
}

/// Points every role of the saved preset `name` at its models for this session.
pub fn apply_preset(place: &Place<'_>, name: &str) -> Result<Roles, String> {
    let presets = files(place).0.presets;
    let Some(preset) = presets.get(name) else {
        return Err(match presets.is_empty() {
            true => format!("no preset `{name}`: none is saved yet"),
            false => format!(
                "no preset `{name}`; there are: {}",
                presets.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        });
    };
    crate::routing::apply_roles(place.router, &preset.roles, place.lead);
    Ok(list(place))
}

/// The chain as written, in the canonical form, for `set`.
pub fn parse_chain(texts: &[String]) -> Result<Vec<ModelSpec>, String> {
    texts
        .iter()
        .map(|text| text.parse::<ModelSpec>().map_err(|e| format!("`{text}`: {e}")))
        .collect()
}

/// The presets the files keep, by name.
pub fn presets(place: &Place<'_>) -> BTreeMap<String, Vec<(String, Vec<String>)>> {
    files(place)
        .0
        .presets
        .into_iter()
        .map(|(name, preset)| {
            let roles = preset
                .roles
                .iter()
                .map(|(role, chain)| (role.clone(), strings(chain)))
                .collect();
            (name, roles)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_ai::{Catalog, RetryPolicy};

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: Paths,
        router: Router,
        load: LoadOptions,
    }

    fn fixture(home: &str, project: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home_dir = dir.path().join("home");
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(cwd.join(".azazel")).unwrap();
        std::fs::create_dir_all(&home_dir).unwrap();
        std::fs::write(home_dir.join("config.toml"), home).unwrap();
        std::fs::write(cwd.join(".azazel/config.toml"), project).unwrap();
        let paths = Paths::discover(&cwd, Some(home_dir.to_str().unwrap()));
        let load = LoadOptions {
            trusted: true,
            ..LoadOptions::default()
        };
        let config = Config::load(&paths, &load).unwrap().config;
        let router = Router::new(
            Vec::new(),
            Catalog::default(),
            config.roles,
            RetryPolicy::default(),
        );
        Fixture {
            _dir: dir,
            paths,
            router,
            load,
        }
    }

    fn place(f: &Fixture, trusted: bool) -> Place<'_> {
        Place {
            paths: &f.paths,
            load: &f.load,
            trusted,
            router: &f.router,
            lead: None,
        }
    }

    fn row<'a>(roles: &'a Roles, role: &str) -> &'a RoleRow {
        roles.roles.iter().find(|row| row.role == role).unwrap()
    }

    fn chain(specs: &[&str]) -> Vec<ModelSpec> {
        specs.iter().map(|spec| spec.parse().unwrap()).collect()
    }

    #[test]
    fn every_role_says_where_its_value_comes_from() {
        let f = fixture(
            "[roles]\ndefault = [\"a/one:high\"]\nmine = [\"a/two\"]\n",
            "[roles]\nslow = [\"b/three\"]\n",
        );
        f.router.set_role("smol", chain(&["a/four"]));
        let roles = list(&place(&f, true));
        let sources: Vec<(&str, RoleSource)> = ["default", "slow", "smol", "plan", "mine"]
            .into_iter()
            .map(|role| (role, row(&roles, role).source))
            .collect();
        assert_eq!(
            sources,
            [
                ("default", RoleSource::Home),
                ("slow", RoleSource::Project),
                ("smol", RoleSource::Session),
                ("plan", RoleSource::Builtin),
                ("mine", RoleSource::Home),
            ]
        );
        assert_eq!(row(&roles, "default").thinking.as_deref(), Some("high"));
        assert_eq!(row(&roles, "plan").builtin, ["@slow"]);
        // Built-in roles come first, in their order; the user's own after.
        let names: Vec<&str> = roles.roles.iter().map(|row| row.role.as_str()).collect();
        assert_eq!(names[..3], ["default", "smol", "slow"]);
        assert_eq!(names.last(), Some(&"mine"));
    }

    #[test]
    fn a_role_is_saved_where_asked_and_clearing_falls_back_a_layer() {
        let f = fixture("[roles]\nslow = [\"a/home\"]\n", "# project\n");
        let p = place(&f, true);
        let roles = set(
            &p,
            "slow",
            Some(chain(&["b/proj:low", "@default"])),
            SaveTo::Project,
        )
        .unwrap();
        assert_eq!(row(&roles, "slow").source, RoleSource::Project);
        assert_eq!(row(&roles, "slow").chain, ["b/proj:low", "@default"]);
        let written = std::fs::read_to_string(f.paths.project_config()).unwrap();
        // Comments are kept (toml_edit moves a lone trailing comment after the new table).
        assert!(written.contains("# project\n"), "{written}");
        assert!(
            written.contains("slow = [\"b/proj:low\", \"@default\"]"),
            "{written}"
        );

        // Taking the project's value away leaves the home one.
        let roles = set(&p, "slow", None, SaveTo::Project).unwrap();
        assert_eq!(
            (row(&roles, "slow").source, &row(&roles, "slow").chain[..]),
            (RoleSource::Home, &["a/home".to_string()][..])
        );
        // And the home one away leaves the built-in.
        let roles = set(&p, "slow", None, SaveTo::Home).unwrap();
        assert_eq!(row(&roles, "slow").source, RoleSource::Builtin);
        assert_eq!(f.router.roles()["slow"], chain(&["@default"]));
    }

    #[test]
    fn a_session_change_writes_nothing_and_a_custom_role_can_go() {
        let f = fixture("", "");
        let p = place(&f, true);
        let roles = set(&p, "mine", Some(chain(&["a/x"])), SaveTo::Session).unwrap();
        assert_eq!(row(&roles, "mine").source, RoleSource::Session);
        assert_eq!(std::fs::read_to_string(f.paths.home_config()).unwrap(), "");
        let roles = set(&p, "mine", None, SaveTo::Session).unwrap();
        assert!(roles.roles.iter().all(|row| row.role != "mine"));
        assert!(!f.router.roles().contains_key("mine"));
    }

    #[test]
    fn mistakes_are_refused() {
        let f = fixture("", "");
        let untrusted = place(&f, false);
        let refused = set(&untrusted, "slow", Some(chain(&["a/x"])), SaveTo::Project).unwrap_err();
        assert!(refused.contains("not trusted"), "{refused}");
        let p = place(&f, true);
        for (role, given, expected) in [
            ("bad name", Some(chain(&["a/x"])), "cannot name a role"),
            ("slow", Some(Vec::new()), "at least one model"),
            ("slow", Some(chain(&["@slow"])), "fall back to itself"),
        ] {
            let error = set(&p, role, given, SaveTo::Session).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn presets_are_saved_from_the_live_roles_and_applied() {
        let f = fixture("", "");
        let p = place(&f, true);
        set(&p, "default", Some(chain(&["a/one"])), SaveTo::Session).unwrap();
        let roles = save_preset(&p, "deep").unwrap();
        assert_eq!(roles.presets, ["deep"]);
        assert_eq!(
            presets(&p)["deep"]
                .iter()
                .find(|(role, _)| role == "default")
                .unwrap()
                .1,
            ["a/one"]
        );
        set(&p, "default", Some(chain(&["a/two"])), SaveTo::Session).unwrap();
        let roles = apply_preset(&p, "deep").unwrap();
        assert_eq!(row(&roles, "default").chain, ["a/one"]);
        assert!(apply_preset(&p, "none").unwrap_err().contains("there are: deep"));
    }
}
