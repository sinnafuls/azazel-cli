//! `azazel roles`: the model roles from the command line, listed and changed through
//! [`crate::roles`].

use azazel_ai::providers::presets::ProviderConfig;
use azazel_ai::{Catalog, Credentials, ModelSpec, RetryPolicy, Router, ThinkingLevel};
use azazel_core::{Config, LoadOptions, Paths};
use azazel_protocol::{RoleRow, RoleSource, Roles, SaveTo};

use crate::cli::{Cli, PresetAction, RolesAction};
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use crate::roles::{self, Place};

/// What `azazel roles` reads once and every action needs.
pub struct Setup {
    pub paths: Paths,
    pub load: LoadOptions,
    pub trusted: bool,
    pub config: Config,
    pub providers: Vec<ProviderConfig>,
    pub credentials: Credentials,
    /// `--thinking`, as typed.
    pub thinking: Option<String>,
}

impl Setup {
    pub fn new(cli: &Cli, paths: Paths, trusted: bool) -> Result<Setup, String> {
        let overrides = crate::app::config_overrides(cli, &paths).map_err(|e| format!("{e:#}"))?;
        let load = LoadOptions {
            trusted,
            overrides,
            env: std::env::vars().collect(),
        };
        let mut config = Config::load(&paths, &load).map_err(|e| format!("{e:#}"))?.config;
        if let Some(name) = &cli.profile {
            crate::profiles::apply(&mut config, name).map_err(|e| format!("--profile: {e}"))?;
        }
        let providers = crate::models::providers(&config)?;
        let credentials = crate::app::credentials(&paths, trusted).configured(&config.providers);
        Ok(Setup {
            paths,
            load,
            trusted,
            config,
            providers,
            credentials,
            thinking: cli.thinking.clone(),
        })
    }

    /// The roles as the config files set them: there is no session to change them.
    fn router(&self) -> Router {
        Router::new(
            Vec::new(),
            Catalog::default(),
            self.config.roles.clone(),
            RetryPolicy::default(),
        )
    }

    fn place<'a>(&'a self, router: &'a Router) -> Place<'a> {
        Place {
            paths: &self.paths,
            load: &self.load,
            trusted: self.trusted,
            router,
            lead: None,
        }
    }

    /// Every role, as `azazel roles` lists it.
    pub fn roles(&self) -> Vec<RoleRow> {
        roles::list(&self.place(&self.router())).roles
    }
}

/// A mistake in how the command was called (exit 2), or a change that could not be made (1).
enum Failure {
    Usage(String),
    Failed(String),
}

/// Runs `azazel roles [action]`.
pub fn run(setup: &Setup, action: Option<&RolesAction>, json: bool) -> u8 {
    let router = setup.router();
    let place = setup.place(&router);
    if setup.thinking.is_some() && !matches!(action, Some(RolesAction::Set { .. })) {
        eprintln!("azazel: --thinking goes with `azazel roles set`");
        return EXIT_USAGE;
    }
    let done = match action {
        None => {
            print_roles(&roles::list(&place).roles, json);
            Ok(())
        }
        Some(RolesAction::Set {
            role,
            models,
            project,
        }) => set(setup, &place, role, models, save_to(*project), json),
        Some(RolesAction::Clear { role, project }) => clear(&place, role, save_to(*project), json),
        Some(RolesAction::Preset {
            action: PresetAction::Save { name },
        }) => save_preset(&place, name, json),
        Some(RolesAction::Preset {
            action: PresetAction::Load { name, project },
        }) => load_preset(&place, name, save_to(*project), json),
    };
    match done {
        Ok(()) => EXIT_OK,
        Err(Failure::Usage(why)) => {
            eprintln!("azazel: {why}");
            EXIT_USAGE
        }
        Err(Failure::Failed(why)) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

fn save_to(project: bool) -> SaveTo {
    match project {
        true => SaveTo::Project,
        false => SaveTo::Home,
    }
}

fn file_name(save: SaveTo) -> &'static str {
    match save {
        SaveTo::Project => "the project config",
        SaveTo::Home | SaveTo::Session => "the home config",
    }
}

fn set(
    setup: &Setup,
    place: &Place<'_>,
    role: &str,
    models: &[String],
    save: SaveTo,
    json: bool,
) -> Result<(), Failure> {
    let ids: Vec<String> = setup.providers.iter().map(|p| p.id.clone()).collect();
    let (known, usable) = crate::app::known_and_usable(ids, &setup.providers, &setup.credentials, false);
    let catalog = Catalog::load(Some(&setup.paths.models_cache()), &setup.config.models);
    // The names are this command's words, not `--model`'s.
    let mut chain = crate::models::resolve_specs(models, &catalog, &known, &usable)
        .map_err(|why| Failure::Usage(why.strip_prefix("--model ").unwrap_or(&why).to_string()))?;
    if let Some(text) = &setup.thinking {
        let level: ThinkingLevel = text
            .parse()
            .map_err(|e| Failure::Usage(format!("--thinking: {e}")))?;
        match chain.first_mut() {
            Some(ModelSpec::Model(model)) => model.thinking = Some(level),
            _ => {
                return Err(Failure::Usage(format!(
                    "--thinking: {} is a role, which has the level of its own models",
                    models[0]
                )));
            }
        }
    }
    roles::check(place, role, Some(&chain), save).map_err(Failure::Usage)?;
    let roles = roles::set(place, role, Some(chain), save).map_err(Failure::Failed)?;
    report(&roles, role, json, |row| {
        format!("{role} → {} ({})", row.chain.join(", "), file_name(save))
    });
    Ok(())
}

fn clear(place: &Place<'_>, role: &str, save: SaveTo, json: bool) -> Result<(), Failure> {
    roles::check(place, role, None, save).map_err(Failure::Usage)?;
    let roles = roles::set(place, role, None, save).map_err(Failure::Failed)?;
    report(&roles, role, json, |row| match row.source {
        RoleSource::Unset => format!("{role} taken out of {}; it is not set now", file_name(save)),
        source => format!(
            "{role} taken out of {}; it is now {} ({})",
            file_name(save),
            row.chain.join(", "),
            source_name(source)
        ),
    });
    Ok(())
}

/// What a change made of `role`: its row, as a line or as JSON, and a note when a file that
/// is read later sets the role too and so wins over the one written.
fn report(roles: &Roles, role: &str, json: bool, line: impl Fn(&RoleRow) -> String) {
    let row = roles.roles.iter().find(|row| row.role == role);
    match (row, json) {
        (Some(row), true) => println!("{}", serde_json::to_string(row).unwrap_or_default()),
        (Some(row), false) => {
            println!("{}", line(row));
            if matches!(row.source, RoleSource::Project | RoleSource::ProjectLocal) {
                println!(
                    "note: in this project {role} is {} from {}, which wins",
                    row.chain.join(", "),
                    source_name(row.source)
                );
            }
        }
        // A custom role that was cleared is gone altogether.
        (None, true) => println!("{}", serde_json::json!({ "role": role, "source": "unset" })),
        (None, false) => println!(
            "{}",
            line(&RoleRow {
                role: role.to_string(),
                chain: Vec::new(),
                source: RoleSource::Unset,
                builtin: Vec::new(),
                resolved: Vec::new(),
                thinking: None,
            })
        ),
    }
}

fn save_preset(place: &Place<'_>, name: &str, json: bool) -> Result<(), Failure> {
    crate::routing::profile_name(name).map_err(Failure::Usage)?;
    roles::save_preset(place, name).map_err(Failure::Failed)?;
    let saved: Vec<String> = roles::presets(place)
        .remove(name)
        .unwrap_or_default()
        .into_iter()
        .map(|(role, _)| role)
        .collect();
    match json {
        true => println!(
            "{}",
            serde_json::json!({
                "preset": name,
                "roles": saved,
                "file": place.paths.home_config(),
            })
        ),
        false => println!(
            "saved preset {name} ({} roles) in {}",
            saved.len(),
            place.paths.home_config().display()
        ),
    }
    Ok(())
}

/// Writes every role of preset `name` into the file `save` names: a run of the command line
/// has no session to keep them in.
fn load_preset(place: &Place<'_>, name: &str, save: SaveTo, json: bool) -> Result<(), Failure> {
    let mut presets = roles::presets(place);
    let Some(preset) = presets.remove(name) else {
        return Err(Failure::Usage(match presets.is_empty() {
            true => format!("no preset `{name}`: none is saved yet"),
            false => format!(
                "no preset `{name}`; there are: {}",
                presets.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        }));
    };
    let mut chains = Vec::new();
    for (role, chain) in &preset {
        let chain = roles::parse_chain(chain).map_err(Failure::Usage)?;
        roles::check(place, role, Some(&chain), save).map_err(Failure::Usage)?;
        chains.push((role, chain));
    }
    let mut roles = None;
    for (role, chain) in chains {
        roles = Some(roles::set(place, role, Some(chain), save).map_err(Failure::Failed)?);
    }
    let rows: Vec<&RoleRow> = roles
        .as_ref()
        .map(|roles| {
            roles
                .roles
                .iter()
                .filter(|row| preset.iter().any(|(role, _)| *role == row.role))
                .collect()
        })
        .unwrap_or_default();
    match json {
        true => {
            for row in rows {
                println!("{}", serde_json::to_string(row).unwrap_or_default());
            }
        }
        false => {
            println!(
                "preset {name}: {} roles written to {}",
                rows.len(),
                file_name(save)
            );
            print_roles(&rows.into_iter().cloned().collect::<Vec<_>>(), false);
        }
    }
    Ok(())
}

/// How a source reads in a list.
fn source_name(source: RoleSource) -> &'static str {
    match source {
        RoleSource::Builtin => "built-in",
        RoleSource::Home => "home",
        RoleSource::Project => "project",
        RoleSource::ProjectLocal => "local",
        RoleSource::Session => "session",
        RoleSource::Unset => "unset",
    }
}

/// The model a role answers with first, without its level, and that level.
fn first_model(row: &RoleRow) -> (String, String) {
    match row
        .resolved
        .first()
        .and_then(|text| text.parse::<ModelSpec>().ok())
    {
        Some(ModelSpec::Model(model)) => (
            model.key(),
            model
                .thinking
                .map_or_else(|| "-".to_string(), |level| level.to_string()),
        ),
        _ => ("-".to_string(), "-".to_string()),
    }
}

fn print_roles(rows: &[RoleRow], json: bool) {
    if json {
        for row in rows {
            println!("{}", serde_json::to_string(row).unwrap_or_default());
        }
        return;
    }
    print!("{}", table(rows));
}

/// The roles as aligned columns under a heading: role, chain, the model it resolves to, its
/// thinking level and where it is set.
fn table(rows: &[RoleRow]) -> String {
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|row| {
            let (model, thinking) = first_model(row);
            let chain = match row.chain.is_empty() {
                true => "-".to_string(),
                false => row.chain.join(", "),
            };
            [
                row.role.clone(),
                chain,
                model,
                thinking,
                source_name(row.source).to_string(),
            ]
        })
        .collect();
    let head = ["role", "chain", "model", "thinking", "source"].map(str::to_string);
    let mut widths = [0; 5];
    for line in std::iter::once(&head).chain(&cells) {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for line in std::iter::once(&head).chain(&cells) {
        let text: Vec<String> = line
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out.push_str(text.join("  ").trim_end());
        out.push('\n');
    }
    out
}
