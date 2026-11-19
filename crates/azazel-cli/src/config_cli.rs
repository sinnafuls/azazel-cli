//! `azazel config get` and `azazel config set`: one setting at a time, through the store the
//! settings screen uses, so a value is checked and written the same way.

use azazel_core::config_keys;
use azazel_core::settings::{Kind, NOT_EDITABLE, setting};
use azazel_core::{LoadOptions, Paths, config};
use toml::Value;

use crate::cli::{Cli, ConfigAction};
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use crate::settings::store::{Changes, Store, literal, parse_value, validate};

/// Runs `azazel config …`.
pub fn run(action: &ConfigAction, cli: &Cli, paths: &Paths, trusted: bool) -> u8 {
    let mut store = match open(cli, paths, trusted) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    match action {
        ConfigAction::Get { key } => get(&store, key),
        ConfigAction::Set { key, value, project } => set(&mut store, key, value, *project),
    }
}

/// The configuration as this run has it: the files, the environment and `--set`.
fn open(cli: &Cli, paths: &Paths, trusted: bool) -> Result<Store, String> {
    let overrides = crate::app::config_overrides(cli, paths).map_err(|e| format!("{e:#}"))?;
    let options = LoadOptions {
        trusted,
        overrides,
        env: std::env::vars().collect(),
    };
    Store::open(paths.clone(), options, None, None, Changes::default())
}

/// Whether `path` is a key a configuration file may set, or a table that holds some. An entry of
/// a named table (`lsp.servers.*`) matches whatever name it is given.
fn is_known(path: &str) -> bool {
    let wanted: Vec<&str> = path.split('.').collect();
    config_keys::keys().iter().any(|(key, _)| {
        let key: Vec<&str> = key.split('.').collect();
        // `tools.format.rs` is under the key `tools.format`; `retry` holds `retry.max_attempts`.
        key.iter().zip(&wanted).all(|(a, b)| a == b || *a == "*")
    })
}

fn get(store: &Store, key: &str) -> u8 {
    if !is_known(key) {
        eprintln!(
            "azazel: `{key}` is not a setting; they are written as table.key, as in `azazel config get permissions.mode`"
        );
        return EXIT_USAGE;
    }
    match store.value(key) {
        Some(value) => println!("{}", literal(value)),
        None => println!("(not set)"),
    }
    match store.sources(key).first() {
        Some(source) => eprintln!("({})", source.phrase()),
        None => eprintln!("(the default)"),
    }
    EXIT_OK
}

fn set(store: &mut Store, key: &str, typed: &str, project: bool) -> u8 {
    if project {
        if !store.can_project() {
            eprintln!(
                "azazel: changes go to the project's config only in a trusted project: pass --trust, or trust it with /trust in the UI"
            );
            return EXIT_USAGE;
        }
        if let Err(why) = store.toggle_scope() {
            eprintln!("azazel: {why}");
            return EXIT_USAGE;
        }
    }
    let written = match target(key, typed) {
        Ok(Target::Setting(row, value)) => {
            if let Err(why) = validate(row.path, &value) {
                eprintln!("azazel: {}: {why}", row.path);
                return EXIT_USAGE;
            }
            store.save(row, value)
        }
        Ok(Target::Entry(row, name, value)) => store.set_entry(row, name, value),
        Err(why) => {
            eprintln!("azazel: {why}");
            return EXIT_USAGE;
        }
    };
    match written {
        Ok(line) => {
            println!("{line}");
            EXIT_OK
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

/// What a `set` writes.
enum Target<'a> {
    /// A setting of its own, with the value typed for it.
    Setting(&'static azazel_core::settings::Setting, Value),
    /// One entry of a table setting.
    Entry(&'static azazel_core::settings::Setting, &'a str, Value),
}

/// The setting `key` names and the value `typed` stands for there, or why not.
fn target<'a>(key: &'a str, typed: &str) -> Result<Target<'a>, String> {
    if let Some(row) = setting(key) {
        let value = match row.kind {
            Kind::Role => {
                return Err(format!(
                    "{key} is a model role: set it with `azazel roles set {} <model>`",
                    crate::settings::store::role_name(row)
                ));
            }
            Kind::Table => {
                return Err(format!(
                    "{key} is a table, set one entry at a time: `azazel config set {key}.<name> <value>`"
                ));
            }
            // A list is also written as TOML, as the docs do for lists of users.
            Kind::List if typed.trim_start().starts_with('[') => match config::parse_literal(typed.trim()) {
                array @ Value::Array(_) => array,
                _ => return Err(format!("{key}: expected a TOML array such as [\"a\", \"b\"]")),
            },
            kind => parse_value(kind, typed, row.default_value().as_ref())
                .map_err(|why| format!("{key}: {why}"))?,
        };
        return Ok(Target::Setting(row, value));
    }
    if let Some((parent, name)) = key.rsplit_once('.')
        && let Some(row) = setting(parent)
        && row.kind == Kind::Table
        && !name.is_empty()
    {
        return Ok(Target::Entry(row, name, config::parse_literal(typed.trim())));
    }
    if let Some((_, why)) = NOT_EDITABLE
        .iter()
        .find(|(path, _)| key == *path || key.starts_with(&format!("{path}.")))
    {
        return Err(format!("{key} is not set from the command line: {why}"));
    }
    Err(format!(
        "`{key}` is not a setting that can be set here; the settings are written as table.key (see /settings)"
    ))
}
