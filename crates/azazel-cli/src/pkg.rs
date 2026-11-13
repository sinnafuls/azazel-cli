//! `azazel pkg ...` and `azazel skills ...`: installing, listing, updating and removing packages.

use std::io::{BufRead, Write};
use std::path::Path;

use azazel_core::packages::{Added, CODE_PROMPT, Contributions, Packages, registry, resolve};
use azazel_core::skills::{discover, listing, standard_roots};
use azazel_core::{Config, LoadOptions, Paths};

use crate::cli::{PkgAction, SkillsAction};
use crate::print::{EXIT_FAILED, EXIT_OK};

/// Skill names shown after an install before the rest is counted.
const MAX_NAMED_SKILLS: usize = 12;

/// A commit shortened the way git shows one; a version as it is.
fn short(revision: &str) -> &str {
    let commit = revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit());
    if commit { &revision[..7] } else { revision }
}

/// Says what in the package runs code and asks whether to let it.
fn ask_to_enable_code(found: &Contributions) -> bool {
    let mut kinds = Vec::new();
    if !found.extensions.is_empty() {
        kinds.push(format!("{} extension script(s)", found.extensions.len()));
    }
    if found.mcp.is_some() {
        kinds.push("MCP servers".to_string());
    }
    if kinds.is_empty() {
        kinds.push("skills with hooks".to_string());
    }
    eprintln!("It contains {}.", kinds.join(" and "));
    eprint!("{CODE_PROMPT} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn installed_text(added: &Added) -> String {
    let package = &added.package;
    let mut out = format!("installed {} from {}", package.name, package.source);
    if let Some(revision) = package.commit.as_ref().or(package.version.as_ref()) {
        out.push_str(&format!(" ({})", short(revision)));
    }
    let found = &added.contributions;
    let skills = found.skill_names();
    if !skills.is_empty() {
        let shown = if skills.len() <= MAX_NAMED_SKILLS {
            skills.join(", ")
        } else {
            format!("{} skills", skills.len())
        };
        out.push_str(&format!("\n  skills: {shown}"));
        if package.model_invocable.as_ref().is_some_and(Vec::is_empty) {
            out.push_str(&format!(
                "\n  none is offered to the model yet: add one with `azazel skills add {}/<skill>`; \
                 each can be used as /skill:<skill>",
                package.name
            ));
        }
    }
    for (what, count) in [("agent", found.agents.len()), ("command", found.commands.len())] {
        if count > 0 {
            out.push_str(&format!("\n  {what} directories: {count}"));
        }
    }
    if found.has_code() {
        out.push_str(match package.code_enabled {
            true => "\n  code: enabled",
            false => "\n  code: not enabled (its extensions, MCP servers and skill hooks stay off)",
        });
    }
    if skills.is_empty() && found.agents.is_empty() && found.commands.is_empty() && !found.has_code() {
        out.push_str("\n  nothing azazel can use was found in it");
    }
    if added.no_licence {
        out.push_str(&format!(
            "\nnote: {} has no licence file; installed for your local use only",
            package.source
        ));
    }
    out
}

fn add(store: &Packages, source: &str, name: Option<&str>, cwd: &Path) -> Result<String, String> {
    let added = store.add(resolve(source, cwd)?, name, &ask_to_enable_code)?;
    Ok(installed_text(&added))
}

fn list(store: &Packages) -> Result<String, String> {
    let all = store.list()?;
    if all.is_empty() {
        let names: Vec<String> = registry().into_keys().collect();
        return Ok(format!(
            "no packages are installed; `azazel pkg add <name>` knows: {}",
            names.join(", ")
        ));
    }
    let mut out = String::new();
    for package in all {
        let version = match (
            package.commit.as_ref().or(package.version.as_ref()),
            package.local,
        ) {
            (Some(revision), _) => short(revision),
            (None, true) => "local",
            (None, false) => "unknown commit",
        };
        let code = if package.code_enabled {
            ", code enabled"
        } else {
            ""
        };
        out.push_str(&format!(
            "{}  {} ({version}{code})  installed {}\n",
            package.name, package.source, package.installed_at
        ));
    }
    Ok(out.trim_end().to_string())
}

fn update(store: &Packages, name: Option<&str>) -> Result<String, String> {
    let done = store.update(name)?;
    if done.is_empty() {
        return Ok("no packages are installed".to_string());
    }
    fn shown(revision: &Option<String>) -> &str {
        revision.as_deref().map_or("?", short)
    }
    let lines: Vec<String> = done
        .iter()
        .map(|updated| match (&updated.before, &updated.after) {
            (None, None) => format!("{}: a local directory, used as it is", updated.name),
            (before, after) if before == after => format!("{}: up to date ({})", updated.name, shown(after)),
            (before, after) => format!("{}: {} -> {}", updated.name, shown(before), shown(after)),
        })
        .collect();
    Ok(lines.join("\n"))
}

fn report(done: Result<String, String>) -> u8 {
    match done {
        Ok(text) => {
            println!("{text}");
            EXIT_OK
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

/// `azazel pkg ...`.
pub fn run(action: &PkgAction, paths: &Paths) -> u8 {
    let store = Packages::new(paths);
    report(match action {
        PkgAction::Add { source, name } => add(&store, source, name.as_deref(), &paths.cwd),
        PkgAction::List => list(&store),
        PkgAction::Update { name } => update(&store, name.as_deref()),
        PkgAction::Remove { name } => store.remove(name).map(|()| format!("removed {name}")),
    })
}

/// `<package>/<skill>`, when `text` names one skill of an installed or registered package.
fn one_skill<'a>(store: &Packages, text: &'a str) -> Option<(&'a str, &'a str)> {
    let (package, skill) = text.split_once('/')?;
    let plain = |part: &str| !part.is_empty() && !part.contains(['/', ':', '\\', '.']);
    let known = registry().contains_key(package)
        || store
            .list()
            .is_ok_and(|all| all.iter().any(|installed| installed.name == package));
    (plain(package) && plain(skill) && known).then_some((package, skill))
}

fn add_skill(store: &Packages, source: &str, cwd: &Path) -> Result<String, String> {
    let Some((package, skill)) = one_skill(store, source) else {
        return add(store, source, None, cwd);
    };
    let mut out = String::new();
    if !store.list()?.iter().any(|installed| installed.name == package) {
        out = add(store, package, None, cwd)? + "\n";
    }
    out.push_str(&match store.offer_skill(package, skill)? {
        true => format!("skill `{skill}` of {package} is now offered to the model"),
        false => format!("skill `{skill}` of {package} is already offered to the model"),
    });
    Ok(out)
}

/// Every skill a session in this project would have.
fn list_skills(store: &Packages, paths: &Paths) -> Result<String, String> {
    let options = LoadOptions {
        trusted: false,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let compat = Config::load(paths, &options)
        .map_err(|e| e.to_string())?
        .config
        .compat
        .claude;
    let set = discover(&standard_roots(paths, compat, &store.load()));
    let skills: Vec<_> = set.skills.into_values().collect();
    Ok(listing(&skills, &set.invalid).trim_end().to_string())
}

/// `azazel skills ...`: the same store, addressed by what most packages are for.
pub fn run_skills(action: &SkillsAction, paths: &Paths) -> u8 {
    let store = Packages::new(paths);
    report(match action {
        SkillsAction::Add { source } => add_skill(&store, source, &paths.cwd),
        SkillsAction::List => list_skills(&store, paths),
        SkillsAction::Remove { name } => store.remove(name).map(|()| format!("removed {name}")),
    })
}
