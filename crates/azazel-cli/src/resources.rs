//! What the command line adds to one run: `--extension`, `--skill` and `--mcp-config`.
//!
//! The paths are the user's own word, so what they name loads like the user's own files:
//! whether or not the project is trusted, and with the `--no-*` switches on.

use std::path::{Path, PathBuf};

use azazel_core::agents::{self, DefSource};
use azazel_core::commands;
use azazel_core::packages::{self, Loaded};
use azazel_core::paths::canonical;
use azazel_core::skills::discover;
use azazel_core::{Paths, SkillRoot, SkillSource};

use crate::cli::Cli;

/// The resources named for this run. They come after everything found in the usual places, so
/// where names collide they win.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunResources {
    pub skill_roots: Vec<SkillRoot>,
    pub agent_roots: Vec<PathBuf>,
    pub command_roots: Vec<PathBuf>,
    /// `(script id, file)` of the Luau scripts.
    pub scripts: Vec<(String, PathBuf)>,
    pub mcp_files: Vec<PathBuf>,
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl RunResources {
    /// Reads the flags of `cli`; relative paths are below `cwd`. `Err` says which flag names
    /// something that cannot be used, and why.
    pub fn from_cli(cli: &Cli, cwd: &Path) -> Result<RunResources, String> {
        let mut run = RunResources::default();
        for given in &cli.extension {
            run.extension(&cwd.join(given))
                .map_err(|why| format!("--extension {}: {why}", given.display()))?;
        }
        for given in &cli.skill {
            run.skill(&cwd.join(given))
                .map_err(|why| format!("--skill {}: {why}", given.display()))?;
        }
        for given in &cli.mcp_config {
            run.mcp_config(&cwd.join(given))
                .map_err(|why| format!("--mcp-config {}: {why}", given.display()))?;
        }
        Ok(run)
    }

    /// A Luau file is one script. A directory is read like a package (`azazel.toml`, or the
    /// directories `skills`, `agents` and `commands`) with its code enabled, and an `init.luau`
    /// in it is a script too.
    fn extension(&mut self, path: &Path) -> Result<(), String> {
        if path.is_file() {
            if path.extension().is_none_or(|extension| extension != "luau") {
                return Err("not a Luau script (`.luau`) or a directory".to_string());
            }
            let path = canonical(path);
            // `<name>/init.luau` goes by the name of its directory, as in an extensions directory.
            let id = match stem_of(&path).as_str() {
                "init" => path.parent().map(name_of).unwrap_or_default(),
                stem => stem.to_string(),
            };
            self.script(id, path);
            return Ok(());
        }
        if !path.is_dir() {
            return Err("no such file or directory".to_string());
        }
        let root = canonical(path);
        let name = name_of(&root);
        let found = packages::contributions(&root);
        let init = root.join("init.luau");
        let nothing = found.skills.is_empty()
            && found.agents.is_empty()
            && found.commands.is_empty()
            && found.extensions.is_empty()
            && found.mcp.is_none()
            && !init.is_file();
        if nothing {
            return Err(
                "nothing to load in it: no azazel.toml, init.luau, skills, agents or commands".to_string(),
            );
        }
        for path in found.skills {
            self.skill_roots.push(SkillRoot {
                single: path.join("SKILL.md").is_file(),
                ..SkillRoot::new(path, SkillSource::Package(name.clone()))
            });
        }
        self.agent_roots.extend(found.agents);
        self.command_roots.extend(found.commands);
        for script in found.extensions {
            self.script(format!("{name}-{}", stem_of(&script)), script);
        }
        if init.is_file() && !self.scripts.iter().any(|(_, script)| *script == init) {
            self.script(name, init);
        }
        self.mcp_files.extend(found.mcp);
        Ok(())
    }

    fn script(&mut self, id: String, file: PathBuf) {
        self.scripts.retain(|(known, _)| *known != id);
        self.scripts.push((id, file));
    }

    /// One skill (its directory, or its `SKILL.md`) or a directory of skills.
    fn skill(&mut self, path: &Path) -> Result<(), String> {
        let dir = match path.is_file() {
            true if path.file_name().is_some_and(|name| name == "SKILL.md") => {
                path.parent().unwrap_or(path).to_path_buf()
            }
            true => return Err("not a SKILL.md or a directory".to_string()),
            false if path.is_dir() => path.to_path_buf(),
            false => return Err("no such file or directory".to_string()),
        };
        let root = SkillRoot {
            single: dir.join("SKILL.md").is_file(),
            ..SkillRoot::new(canonical(&dir), SkillSource::CommandLine)
        };
        let found = discover(std::slice::from_ref(&root));
        if let Some(invalid) = found.invalid.first() {
            return Err(format!("{}: {}", invalid.path.display(), invalid.error));
        }
        if found.skills.is_empty() {
            return Err("no skill in it: neither a SKILL.md nor directories that have one".to_string());
        }
        self.skill_roots.push(root);
        Ok(())
    }

    fn mcp_config(&mut self, file: &Path) -> Result<(), String> {
        let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
        let servers = serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| format!("not valid JSON: {e}"))?
            .get("mcpServers")
            .is_some_and(serde_json::Value::is_object);
        if !servers {
            return Err("it has no `mcpServers` object".to_string());
        }
        self.mcp_files.push(canonical(file));
        Ok(())
    }

    /// Whether nothing was named.
    pub fn is_empty(&self) -> bool {
        *self == RunResources::default()
    }

    /// The directories custom commands are read from, lowest precedence first.
    pub fn command_roots(&self, paths: &Paths, claude_compat: bool, packages: &Loaded) -> Vec<PathBuf> {
        let mut roots = commands::command_roots(paths, claude_compat, packages);
        roots.extend(self.command_roots.iter().cloned());
        roots
    }

    /// The directories agent definitions are read from, lowest precedence first.
    pub fn def_roots(
        &self,
        paths: &Paths,
        claude_compat: bool,
        packages: &Loaded,
    ) -> Vec<(PathBuf, DefSource)> {
        let mut roots = agents::def_roots(paths, claude_compat, packages);
        roots.extend(
            self.agent_roots
                .iter()
                .map(|dir| (dir.clone(), DefSource::Package)),
        );
        roots
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use clap::Parser;

    use super::*;

    const SKILL: &str = "---\nname: NAME\ndescription: Use when testing the flags.\n---\nBody.\n";

    fn write(root: &Path, file: &str, text: &str) {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn run(dir: &Path, args: &[&str]) -> Result<RunResources, String> {
        let cli = Cli::try_parse_from(["azazel"].into_iter().chain(args.iter().copied())).unwrap();
        RunResources::from_cli(&cli, dir)
    }

    fn skill_names(run: &RunResources) -> Vec<String> {
        discover(&run.skill_roots).skills.into_keys().collect()
    }

    #[test]
    fn a_script_file_and_a_script_directory_are_named_like_those_of_an_extensions_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        write(&root, "tools/audit.luau", "-- a");
        write(&root, "guard/init.luau", "-- b");
        let found = run(
            &root,
            &["-e", "tools/audit.luau", "--extension", "guard/init.luau"],
        )
        .unwrap();
        assert_eq!(
            found.scripts,
            [
                ("audit".to_string(), root.join("tools/audit.luau")),
                ("guard".to_string(), root.join("guard/init.luau")),
            ]
        );
        // The directory of an `init.luau` is the same script.
        assert_eq!(run(&root, &["-e", "guard"]).unwrap().scripts, found.scripts[1..]);
        assert!(run(&root, &[]).unwrap().is_empty());

        write(&root, "notes.txt", "x");
        std::fs::create_dir_all(root.join("empty")).unwrap();
        for (path, why) in [
            ("notes.txt", "not a Luau script"),
            ("absent.luau", "no such file or directory"),
            ("empty", "nothing to load in it"),
        ] {
            let error = run(&root, &["-e", path]).unwrap_err();
            assert!(
                error.starts_with(&format!("--extension {path}: ")) && error.contains(why),
                "{error}"
            );
        }
    }

    #[test]
    fn a_package_directory_brings_everything_it_offers_with_its_code_on() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        write(
            &root,
            "kit/azazel.toml",
            "skills = [\"skills\"]\nagents = [\"agents\"]\ncommands = [\"commands\"]\n\
             extensions = [\"ext/main.luau\"]\nmcp = \"mcp.json\"\n",
        );
        write(
            &root,
            "kit/skills/review/SKILL.md",
            &SKILL.replace("NAME", "review"),
        );
        write(&root, "kit/agents/helper.md", "x");
        write(&root, "kit/commands/ship.md", "Ship it.");
        write(&root, "kit/ext/main.luau", "-- script");
        write(&root, "kit/mcp.json", "{\"mcpServers\": {}}");
        let found = run(&root, &["-e", "kit"]).unwrap();
        assert_eq!(skill_names(&found), ["review"]);
        assert_eq!(found.skill_roots[0].source, SkillSource::Package("kit".into()));
        // Enabled code: the hooks of its skills count.
        assert!(found.skill_roots[0].hooks);
        assert_eq!(found.agent_roots, [root.join("kit/agents")]);
        assert_eq!(found.command_roots, [root.join("kit/commands")]);
        assert_eq!(
            found.scripts,
            [("kit-main".to_string(), root.join("kit/ext/main.luau"))]
        );
        assert_eq!(found.mcp_files, [root.join("kit/mcp.json")]);

        // They come after the usual places, so they win there.
        let paths = Paths::discover(&root, root.join("home").to_str());
        let commands = found.command_roots(&paths, false, &Loaded::default());
        assert_eq!(commands.last(), Some(&root.join("kit/commands")));
        let defs = found.def_roots(&paths, false, &Loaded::default());
        assert_eq!(defs.last(), Some(&(root.join("kit/agents"), DefSource::Package)));
    }

    #[test]
    fn a_skill_is_given_as_its_directory_its_file_or_a_directory_of_skills() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        write(&root, "one/SKILL.md", &SKILL.replace("NAME", "one"));
        write(&root, "many/a/SKILL.md", &SKILL.replace("NAME", "a"));
        write(&root, "many/b/SKILL.md", &SKILL.replace("NAME", "b"));
        write(&root, "broken/SKILL.md", "no front matter");
        write(&root, "plain/readme.md", "x");

        let found = run(
            &root,
            &["--skill", "one", "--skill", "many", "--skill", "one/SKILL.md"],
        )
        .unwrap();
        assert_eq!(skill_names(&found), ["a", "b", "one"]);
        assert!(
            found
                .skill_roots
                .iter()
                .all(|root| root.source == SkillSource::CommandLine)
        );
        for (path, why) in [
            ("absent", "no such file or directory"),
            ("plain", "no skill in it"),
            ("plain/readme.md", "not a SKILL.md or a directory"),
            ("broken", "SKILL.md"),
        ] {
            let error = run(&root, &["--skill", path]).unwrap_err();
            assert!(
                error.starts_with(&format!("--skill {path}: ")) && error.contains(why),
                "{error}"
            );
        }
    }

    #[test]
    fn an_mcp_file_must_be_there_and_define_servers() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        write(
            &root,
            "servers.json",
            "{\"mcpServers\": {\"echo\": {\"command\": \"echo\"}}}",
        );
        write(&root, "other.json", "{\"servers\": {}}");
        write(&root, "broken.json", "{");
        let found = run(&root, &["--mcp-config", "servers.json"]).unwrap();
        assert_eq!(found.mcp_files, [root.join("servers.json")]);
        for (path, why) in [
            ("absent.json", "--mcp-config absent.json: "),
            ("other.json", "it has no `mcpServers` object"),
            ("broken.json", "not valid JSON"),
        ] {
            let error = run(&root, &["--mcp-config", path]).unwrap_err();
            assert!(error.contains(why), "{error}");
        }
    }
}
