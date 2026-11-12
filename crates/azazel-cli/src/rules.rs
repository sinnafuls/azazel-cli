//! `azazel rules …` and `/rules`: what rule files a project has, when each applies, and a check
//! of one rule against a path, a text or a tool.

use std::io::Read;
use std::path::Path;

use azazel_core::rules::{self, Applies, Loaded, Origin, Rules, Scope, Sources};
use azazel_core::{Config, LoadOptions, Paths};

use crate::app::App;
use crate::cli::RulesAction;
use crate::print::{EXIT_FAILED, EXIT_OK};

/// The rules as a list: name, where it comes from, when it applies, its file. `applied` says
/// whether a rule reached an agent in the open session; `None` leaves that out.
fn render(loaded: &Loaded, applied: Option<&dyn Fn(&str) -> bool>) -> String {
    let mut out = String::new();
    for rule in &loaded.rules {
        out.push_str(&format!(
            "{}  ({})  {}",
            rule.name,
            rule.origin.label(),
            rule.applies_text()
        ));
        if let Some(applied) = applied {
            out.push_str(if applied(&rule.name) {
                "  · applied in this session"
            } else {
                "  · not applied yet"
            });
        }
        out.push('\n');
        if let Some(description) = &rule.description {
            out.push_str(&format!("  {description}\n"));
        }
        if rule.origin != Origin::Config {
            out.push_str(&format!("  {}\n", rule.source.display()));
        }
    }
    for (file, problem) in &loaded.problems {
        out.push_str(&format!("{file}  not loaded: {problem}\n"));
    }
    if out.is_empty() {
        out.push_str(
            "this project has no rules; put Markdown files in .azazel/rules/ (see docs/rules.md)\n",
        );
    }
    out.trim_end().to_string()
}

/// `/rules`: the rules of the running session and whether each has been applied in it.
pub fn session_report(app: &App, session_id: &str) -> String {
    match app.registry.services.get::<Rules>() {
        Some(rules) => render(
            &rules.loaded(),
            Some(&|name: &str| rules.applied(session_id, name)),
        ),
        None => "rules are not loaded in this session".to_string(),
    }
}

fn sources(paths: &Paths, trusted: bool) -> Result<Sources, String> {
    let options = LoadOptions {
        trusted,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let config: Config = Config::load(paths, &options).map_err(|e| e.to_string())?.config;
    Ok(Sources {
        paths: paths.clone(),
        files: true,
        claude_compat: config.compat.claude,
        compat: config.rules.compat,
        stream_rules: config.stream_rules,
    })
}

/// What `azazel rules test` found: the lines to print, and whether everything asked about
/// applies.
fn test(
    paths: &Paths,
    file: &Path,
    text: Option<&str>,
    path: Option<&str>,
    tool: Option<&str>,
) -> Result<(Vec<String>, bool), String> {
    let file = paths.cwd.join(file);
    let raw = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let name = file
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let rule =
        rules::parse(&name, &file, Origin::guess(&file), &raw).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut lines = vec![format!("{}: {}", rule.name, rule.applies_text())];
    let mut all = true;

    if let Some(path) = path {
        let root = azazel_core::paths::canonical(&paths.project_root);
        let relative = rules::project_relative(&root, &paths.cwd, path);
        let (applies, why) = match rule.applies() {
            Applies::Paths if rule.matches_path(&relative) => (true, "its paths match".to_string()),
            Applies::Paths => (false, format!("none of {} matches", rule.paths.join(", "))),
            Applies::Always => (true, "the rule always applies".to_string()),
            Applies::Trigger => (false, "the rule has a trigger, not paths".to_string()),
            Applies::OnRequest => (false, "the rule is read on request, not by path".to_string()),
        };
        all &= applies;
        lines.push(format!(
            "path {relative}: {} ({why})",
            if applies { "applies" } else { "does not apply" }
        ));
    }
    let text = match text {
        Some("-") => {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .map_err(|e| format!("standard input: {e}"))?;
            Some(input)
        }
        Some(source) => {
            let source = paths.cwd.join(source);
            Some(std::fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?)
        }
        None => None,
    };
    if let Some(tool) = tool {
        let checks = matches!(&rule.scope, Scope::Tool(name) if name == tool) && rule.trigger.is_some();
        all &= checks;
        lines.push(if checks {
            format!("tool {tool}: the rule checks the arguments of its calls")
        } else {
            format!(
                "tool {tool}: the rule does not check its calls (scope: {})",
                rule.scope
            )
        });
    }
    if let Some(text) = &text {
        let what = match &rule.scope {
            Scope::Tool(_) => "arguments",
            _ => "text",
        };
        match (rule.trigger.is_some(), rule.trigger_match(text)) {
            (_, Some(found)) => lines.push(format!("{what}: the trigger matches `{found}`")),
            (true, None) => {
                all = false;
                lines.push(format!("{what}: the trigger does not match"));
            }
            (false, None) => {
                all = false;
                lines.push(format!("{what}: the rule has no trigger"));
            }
        }
    }
    if path.is_none() && text.is_none() && tool.is_none() {
        lines.push("give --path, --text or --tool to check the rule against something".to_string());
    }
    Ok((lines, all))
}

/// Runs `azazel rules …`. `test` exits 1 when something asked about does not apply.
pub fn run(action: &RulesAction, paths: &Paths, trusted: bool) -> u8 {
    let done = match action {
        RulesAction::List => {
            sources(paths, trusted).map(|sources| (vec![render(&rules::load(&sources), None)], true))
        }
        RulesAction::Test {
            rule,
            text,
            path,
            tool,
        } => test(paths, rule, text.as_deref(), path.as_deref(), tool.as_deref()),
    };
    match done {
        Ok((lines, all)) => {
            println!("{}", lines.join("\n"));
            if all { EXIT_OK } else { EXIT_FAILED }
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".azazel/rules")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let paths = Paths::discover(&root, Some(&tmp.path().join("home").to_string_lossy()));
        (tmp, paths)
    }

    #[test]
    fn a_rule_is_tested_against_a_path_a_text_and_a_tool() {
        let (_tmp, paths) = project();
        let rules_dir = paths.project_dir().join("rules");
        std::fs::write(rules_dir.join("rust.md"), "---\npaths: \"src/**/*.rs\"\n---\nuse ?").unwrap();
        std::fs::write(
            rules_dir.join("pnpm.md"),
            "---\ntrigger: \"npm (install|i)\\\\b\"\nscope: tool:shell\n---\nuse pnpm",
        )
        .unwrap();
        std::fs::write(paths.cwd.join("args.txt"), "npm install left-pad").unwrap();
        let rust = Path::new(".azazel/rules/rust.md");
        let pnpm = Path::new(".azazel/rules/pnpm.md");

        let (lines, all) = test(&paths, rust, None, Some("src/a/lib.rs"), None).unwrap();
        assert!(all);
        assert_eq!(
            lines,
            [
                "rust: when a file matching src/**/*.rs is touched",
                "path src/a/lib.rs: applies (its paths match)"
            ]
        );
        let (lines, all) = test(&paths, rust, None, Some("README.md"), None).unwrap();
        assert!(!all);
        assert_eq!(
            lines[1],
            "path README.md: does not apply (none of src/**/*.rs matches)"
        );

        let (lines, all) = test(&paths, pnpm, Some("args.txt"), None, Some("shell")).unwrap();
        assert!(all, "{lines:?}");
        assert_eq!(lines[1], "tool shell: the rule checks the arguments of its calls");
        assert_eq!(lines[2], "arguments: the trigger matches `npm install`");
        let (lines, all) = test(&paths, pnpm, None, None, Some("read")).unwrap();
        assert!(!all);
        assert!(lines[1].contains("does not check its calls (scope: tool:shell)"));
        let (lines, all) = test(&paths, rust, Some("args.txt"), None, None).unwrap();
        assert!(!all);
        assert_eq!(lines[1], "text: the rule has no trigger");

        assert!(test(&paths, Path::new("missing.md"), None, None, None).is_err());
    }

    #[test]
    fn the_list_names_each_rule_its_origin_and_whether_it_was_applied() {
        let (_tmp, paths) = project();
        std::fs::write(
            paths.project_dir().join("rules/release.md"),
            "---\ndescription: how to cut a release\n---\ntag it",
        )
        .unwrap();
        std::fs::write(paths.project_root.join(".cursorrules"), "be brief").unwrap();
        let loaded = rules::load(&sources(&paths, false).unwrap());
        let text = render(&loaded, Some(&|name: &str| name == "release"));
        assert!(
            text.contains("release  (project)  on request (rule://release)  · applied in this session"),
            "{text}"
        );
        assert!(text.contains("  how to cut a release\n"));
        assert!(text.contains("cursorrules  (cursor)  always  · not applied yet"), "{text}");
        assert!(render(&Loaded::default(), None).starts_with("this project has no rules"));
    }
}
