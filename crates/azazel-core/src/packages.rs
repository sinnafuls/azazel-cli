//! Packages: bundles of skills, agent definitions, commands, Luau extensions and MCP servers,
//! installed under `~/.azazel/packages` and recorded in `~/.azazel/packages.toml`.
//!
//! Skills, agent definitions and commands of a package always load. What can run code (its
//! extensions, its MCP servers and the hooks its skills carry) loads only when the user enabled
//! the package's code.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod npm;

use crate::frontmatter;
use crate::paths::Paths;
use crate::skills::{SkillRoot, SkillSource};

/// Asked when a package that can run code is installed.
pub const CODE_PROMPT: &str = "This package can run code on your machine. Enable its code?";

const REGISTRY: &str = include_str!("../registry.toml");
const MANIFEST: &str = "azazel.toml";

/// One entry of the built-in registry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEntry {
    pub source: String,
    /// The skills offered to the model when the package is installed by this name.
    pub model_invocable: Option<Vec<String>>,
}

/// The names `pkg add` knows, with where each comes from.
pub fn registry() -> BTreeMap<String, RegistryEntry> {
    match toml::from_str(REGISTRY) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::error!("the built-in package registry is unreadable: {e}");
            BTreeMap::new()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Git {
        url: String,
        reference: Option<String>,
    },
    /// A directory used where it is; it is never copied or deleted.
    Local(PathBuf),
    /// A package of the npm registry; `wanted` is a version or a tag, `latest` when `None`.
    Npm {
        name: String,
        wanted: Option<String>,
    },
}

/// A source as the user named it, worked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub source: Source,
    /// The package name used unless the user gives one.
    pub name: String,
    pub model_invocable: Option<Vec<String>>,
}

/// A package name made from `text`: lower case, with anything but letters, digits, `-`, `_` and
/// `.` turned into `-`.
fn name_from(text: &str) -> String {
    let name: String = text
        .trim()
        .trim_end_matches(".git")
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') => c,
            _ => '-',
        })
        .collect();
    name.trim_matches(|c| c == '-' || c == '.').to_string()
}

fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name != name_from(name) {
        return Err(format!(
            "`{name}` cannot be a package name: use lower-case letters, digits, `-`, `_` and `.`"
        ));
    }
    Ok(())
}

/// Works out what `text` names: `github:<owner>/<repo>[@ref]`, `npm:<name>[@version]`, a git URL,
/// a registry name, or a directory (relative to `cwd`).
pub fn resolve(text: &str, cwd: &Path) -> Result<Resolved, String> {
    let text = text.trim();
    let last_segment = |address: &str| {
        let address = address.trim_end_matches('/');
        name_from(address.rsplit(['/', ':']).next().unwrap_or(address))
    };
    if let Some(rest) = text.strip_prefix("github:") {
        let (repo, reference) = match rest.split_once('@') {
            Some((repo, reference)) => (repo, Some(reference.to_string())),
            None => (rest, None),
        };
        let valid = repo.split('/').count() == 2 && repo.split('/').all(|part| !part.is_empty());
        if !valid || reference.as_deref() == Some("") {
            return Err(format!("`{text}` is not of the form github:<owner>/<repo>[@ref]"));
        }
        return Ok(Resolved {
            source: Source::Git {
                url: format!("https://github.com/{repo}"),
                reference,
            },
            name: last_segment(repo),
            model_invocable: None,
        });
    }
    if let Some(rest) = text.strip_prefix("npm:") {
        let (name, wanted) = npm::parse(rest)?;
        return Ok(Resolved {
            name: last_segment(&name),
            source: Source::Npm { name, wanted },
            model_invocable: None,
        });
    }
    if text.contains("://") || text.starts_with("git@") {
        return Ok(Resolved {
            source: Source::Git {
                url: text.to_string(),
                reference: None,
            },
            name: last_segment(text),
            model_invocable: None,
        });
    }
    let known = registry();
    if let Some(entry) = known.get(text) {
        return Ok(Resolved {
            source: Source::Git {
                url: entry.source.clone(),
                reference: None,
            },
            name: text.to_string(),
            model_invocable: entry.model_invocable.clone(),
        });
    }
    let dir = cwd.join(text);
    if !text.is_empty() && dir.is_dir() {
        let dir = dunce::canonicalize(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = dir
            .file_name()
            .map(|name| name_from(&name.to_string_lossy()))
            .unwrap_or_default();
        return Ok(Resolved {
            source: Source::Local(dir),
            name,
            model_invocable: None,
        });
    }
    let names: Vec<&str> = known.keys().map(String::as_str).collect();
    Err(format!(
        "`{text}` is not a git URL, `github:<owner>/<repo>`, `npm:<name>`, a directory or a registry name ({})",
        names.join(", ")
    ))
}

/// One installed package, as `packages.toml` records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub name: String,
    /// The git URL it was cloned from, the directory it lives in, or `npm:<name>`.
    pub source: String,
    /// The git branch or tag, or the npm version or tag, it was asked for by.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The version of an npm package that is installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// RFC 3339, UTC.
    pub installed_at: String,
    /// Whether its extensions, MCP servers and skill hooks load.
    #[serde(default)]
    pub code_enabled: bool,
    /// When present, only these of its skills are offered to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_invocable: Option<Vec<String>>,
    /// The package is the directory `source`, not a clone under the packages directory.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct PackagesFile {
    #[serde(default)]
    package: Vec<Installed>,
}

/// What a package directory offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contributions {
    /// Each is one skill's directory or a directory of such directories.
    pub skills: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
    pub commands: Vec<PathBuf>,
    /// Luau script files.
    pub extensions: Vec<PathBuf>,
    /// An `mcp.json`.
    pub mcp: Option<PathBuf>,
}

/// `relative` below `root`, or `None` when it would leave the package.
fn inside(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative.trim());
    let stays = relative
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    stays.then(|| {
        root.join(
            relative
                .components()
                .filter(|part| *part != Component::CurDir)
                .collect::<PathBuf>(),
        )
    })
}

fn existing_dirs(root: &Path, relative: &[String]) -> Vec<PathBuf> {
    relative
        .iter()
        .filter_map(|path| inside(root, path))
        .filter(|path| path.is_dir())
        .collect()
}

/// A JSON member that is one path or a list of paths.
fn paths_of(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(path)) => vec![path.clone()],
        Some(Value::Array(paths)) => paths
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn read_json(file: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Manifest {
    skills: Vec<String>,
    agents: Vec<String>,
    commands: Vec<String>,
    extensions: Vec<String>,
    mcp: Option<String>,
}

/// What the package at `root` offers, from the first description of it that exists: `azazel.toml`;
/// a Claude plugin manifest (`.claude-plugin/marketplace.json`, then `plugin.json`); a Codex plugin
/// manifest (`.codex-plugin/plugin.json`); the directories `skills/`, `agents/`, `commands/` and a
/// `SKILL.md` at the root. The plugin manifests name skills only.
pub fn contributions(root: &Path) -> Contributions {
    if let Ok(text) = std::fs::read_to_string(root.join(MANIFEST)) {
        let manifest: Manifest = match toml::from_str(&text) {
            Ok(manifest) => manifest,
            Err(e) => {
                tracing::warn!("{}: {e}", root.join(MANIFEST).display());
                Manifest::default()
            }
        };
        return Contributions {
            skills: existing_dirs(root, &manifest.skills),
            agents: existing_dirs(root, &manifest.agents),
            commands: existing_dirs(root, &manifest.commands),
            extensions: manifest
                .extensions
                .iter()
                .filter_map(|path| inside(root, path))
                .filter(|path| path.is_file())
                .collect(),
            mcp: manifest
                .mcp
                .and_then(|path| inside(root, &path))
                .filter(|path| path.is_file()),
        };
    }

    let claude = root.join(".claude-plugin");
    if let Some(marketplace) = read_json(&claude.join("marketplace.json")) {
        let mut skills = Vec::new();
        for plugin in marketplace
            .get("plugins")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // A plugin kept elsewhere (`source` is not a path in this repository) offers nothing here.
            let base = match plugin.get("source") {
                Some(Value::String(base)) => base.as_str(),
                None => ".",
                Some(_) => continue,
            };
            let Some(base) = inside(root, base) else {
                continue;
            };
            skills.extend(existing_dirs(&base, &paths_of(plugin.get("skills"))));
        }
        return Contributions {
            skills,
            ..Contributions::default()
        };
    }
    if let Some(plugin) = read_json(&claude.join("plugin.json")) {
        return Contributions {
            skills: existing_dirs(root, &paths_of(plugin.get("skills"))),
            ..Contributions::default()
        };
    }
    if let Some(plugin) = read_json(&root.join(".codex-plugin").join("plugin.json")) {
        return Contributions {
            skills: existing_dirs(root, &paths_of(plugin.get("skills"))),
            ..Contributions::default()
        };
    }

    let dir = |name: &str| Some(root.join(name)).filter(|path| path.is_dir());
    let mut skills: Vec<PathBuf> = dir("skills").into_iter().collect();
    if root.join("SKILL.md").is_file() {
        skills.push(root.to_path_buf());
    }
    Contributions {
        skills,
        agents: dir("agents").into_iter().collect(),
        commands: dir("commands").into_iter().collect(),
        ..Contributions::default()
    }
}

/// Whether `path` is one skill's directory rather than a directory of them.
fn is_skill_dir(path: &Path) -> bool {
    path.join("SKILL.md").is_file()
}

impl Contributions {
    /// The `SKILL.md` files of the skills.
    fn skill_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for path in &self.skills {
            if is_skill_dir(path) {
                files.push(path.join("SKILL.md"));
                continue;
            }
            let mut found: Vec<PathBuf> = std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path().join("SKILL.md"))
                .filter(|file| file.is_file())
                .collect();
            found.sort();
            files.extend(found);
        }
        files
    }

    /// The names of the skills, read from their files; a skill without a readable name goes by
    /// its directory.
    pub fn skill_names(&self) -> Vec<String> {
        self.skill_files()
            .iter()
            .map(|file| {
                let named = std::fs::read_to_string(file)
                    .ok()
                    .and_then(|text| frontmatter::split(&text).map(|(yaml, _)| yaml.to_string()))
                    .and_then(|yaml| frontmatter::parse(&yaml).ok())
                    .and_then(|meta| meta.get("name").and_then(Value::as_str).map(str::to_string));
                named.unwrap_or_else(|| {
                    let dir = file.parent().and_then(Path::file_name);
                    dir.map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
            })
            .collect()
    }

    /// Whether anything in the package would run code: an extension, an MCP server, or a skill
    /// that carries hooks.
    pub fn has_code(&self) -> bool {
        !self.extensions.is_empty()
            || self.mcp.is_some()
            || self.skill_files().iter().any(|file| {
                std::fs::read_to_string(file)
                    .ok()
                    .and_then(|text| frontmatter::split(&text).map(|(yaml, _)| yaml.to_string()))
                    .and_then(|yaml| frontmatter::parse(&yaml).ok())
                    .is_some_and(|meta| meta.contains_key("hooks"))
            })
    }
}

/// What the installed packages add to a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub skill_roots: Vec<SkillRoot>,
    pub agent_roots: Vec<PathBuf>,
    pub command_roots: Vec<PathBuf>,
    /// `(script id, file)` of the Luau extensions of packages whose code is enabled.
    pub scripts: Vec<(String, PathBuf)>,
    /// The `mcp.json` files of packages whose code is enabled.
    pub mcp_files: Vec<PathBuf>,
    /// Why `packages.toml` could not be used, when it could not.
    pub problem: Option<String>,
}

/// One package brought up to date: the commit of a git package, or the version of an npm package,
/// before and after. A local package has neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated {
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// The result of installing a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub package: Installed,
    pub contributions: Contributions,
    /// The clone has no licence file at its root.
    pub no_licence: bool,
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    let output = command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        let why = why
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("git failed");
        return Err(why.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn has_licence(root: &Path) -> bool {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_uppercase();
            name.starts_with("LICENSE") || name.starts_with("LICENCE") || name.starts_with("COPYING")
        })
}

/// The package store of one azazel home.
pub struct Packages {
    file: PathBuf,
    dir: PathBuf,
    /// The npm registry `npm:` packages come from.
    npm_registry: String,
}

impl Packages {
    pub fn new(paths: &Paths) -> Self {
        Packages {
            file: paths.packages_file(),
            dir: paths.packages_dir(),
            npm_registry: npm::registry(&|name| std::env::var(name).ok()),
        }
    }

    /// The installed packages in the order they were added.
    pub fn list(&self) -> Result<Vec<Installed>, String> {
        let text = match std::fs::read_to_string(&self.file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", self.file.display())),
        };
        toml::from_str::<PackagesFile>(&text)
            .map(|file| file.package)
            .map_err(|e| format!("{}: {}", self.file.display(), e.message()))
    }

    fn save(&self, package: Vec<Installed>) -> Result<(), String> {
        let text = toml::to_string(&PackagesFile { package }).map_err(|e| e.to_string())?;
        let fail = |e: std::io::Error| format!("{}: {e}", self.file.display());
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(fail)?;
        }
        std::fs::write(&self.file, text).map_err(fail)
    }

    /// Where the files of `package` are.
    pub fn root(&self, package: &Installed) -> PathBuf {
        if package.local {
            PathBuf::from(&package.source)
        } else {
            self.dir.join(&package.name)
        }
    }

    fn find(&self, name: &str) -> Result<(Vec<Installed>, usize), String> {
        let all = self.list()?;
        match all.iter().position(|package| package.name == name) {
            Some(index) => Ok((all, index)),
            None => {
                let names: Vec<&str> = all.iter().map(|package| package.name.as_str()).collect();
                Err(match names.as_slice() {
                    [] => format!("no package named `{name}` is installed; none are"),
                    names => format!(
                        "no package named `{name}` is installed; there are: {}",
                        names.join(", ")
                    ),
                })
            }
        }
    }

    /// Installs `resolved` under `name` (its own name when `None`). `enable_code` is asked once,
    /// and only when the package contains something that runs code.
    pub fn add(
        &self,
        resolved: Resolved,
        name: Option<&str>,
        enable_code: &dyn Fn(&Contributions) -> bool,
    ) -> Result<Added, String> {
        let name = name.map(str::to_string).unwrap_or(resolved.name);
        check_name(&name)?;
        let mut all = self.list()?;
        if all.iter().any(|package| package.name == name) {
            return Err(format!(
                "a package named `{name}` is installed: remove it first, or install this one under another --name"
            ));
        }
        let mut version = None;
        let (root, source, reference, commit, local) = match resolved.source {
            Source::Local(dir) => {
                let source = dir.display().to_string();
                (dir, source, None, None, true)
            }
            Source::Npm {
                name: package,
                wanted,
            } => {
                let root = self.dir.join(&name);
                if root.exists() {
                    return Err(format!(
                        "{} is in the way; delete it and try again",
                        root.display()
                    ));
                }
                std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
                let release = npm::find(&self.npm_registry, &package, wanted.as_deref())
                    .and_then(|release| npm::install(&release, &root).map(|()| release))
                    .map_err(|why| format!("could not install {package} from npm: {why}"))?;
                version = Some(release.version);
                (root, format!("npm:{package}"), wanted, None, false)
            }
            Source::Git { url, reference } => {
                let root = self.dir.join(&name);
                if root.exists() {
                    return Err(format!(
                        "{} is in the way; delete it and try again",
                        root.display()
                    ));
                }
                std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
                let target = root.display().to_string();
                // The files are read, never edited: keep them byte for byte as committed.
                let mut args = vec!["clone", "--quiet", "--depth", "1", "-c", "core.autocrlf=false"];
                if let Some(reference) = &reference {
                    args.extend(["--branch", reference.as_str()]);
                }
                args.extend([url.as_str(), target.as_str()]);
                if let Err(why) = git(None, &args) {
                    let _ = std::fs::remove_dir_all(&root);
                    return Err(format!("could not clone {url}: {why}"));
                }
                let commit = git(Some(&root), &["rev-parse", "HEAD"]).ok();
                (root, url, reference, commit, false)
            }
        };
        let contributions = contributions(&root);
        let package = Installed {
            name,
            source,
            reference,
            commit,
            version,
            installed_at: jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
            code_enabled: contributions.has_code() && enable_code(&contributions),
            model_invocable: resolved.model_invocable,
            local,
        };
        all.push(package.clone());
        if let Err(why) = self.save(all) {
            if !local {
                let _ = std::fs::remove_dir_all(&root);
            }
            return Err(why);
        }
        Ok(Added {
            no_licence: !local && !has_licence(&root),
            package,
            contributions,
        })
    }

    /// Brings the clone of `name` (of every package when `None`) to the newest commit of what it
    /// was installed from, and an npm package to the version its tag or version names now.
    pub fn update(&self, name: Option<&str>) -> Result<Vec<Updated>, String> {
        let mut all = match name {
            Some(name) => self.find(name)?.0,
            None => self.list()?,
        };
        let mut done = Vec::new();
        for package in all
            .iter_mut()
            .filter(|package| name.is_none_or(|name| package.name == name))
        {
            if package.local {
                done.push(Updated {
                    name: package.name.clone(),
                    before: None,
                    after: None,
                });
                continue;
            }
            let root = self.dir.join(&package.name);
            let fail = |why: String| format!("could not update {}: {why}", package.name);
            if let Some(npm_name) = package.source.strip_prefix("npm:") {
                let release =
                    npm::find(&self.npm_registry, npm_name, package.reference.as_deref()).map_err(fail)?;
                if package.version.as_ref() != Some(&release.version) {
                    // The new files are complete before the old ones go.
                    let fresh = self.dir.join(format!(".{}.new", package.name));
                    let _ = std::fs::remove_dir_all(&fresh);
                    npm::install(&release, &fresh).map_err(fail)?;
                    let swapped = match std::fs::remove_dir_all(&root) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                        _ => std::fs::rename(&fresh, &root),
                    };
                    if let Err(e) = swapped {
                        let _ = std::fs::remove_dir_all(&fresh);
                        return Err(fail(format!("{}: {e}", root.display())));
                    }
                }
                done.push(Updated {
                    name: package.name.clone(),
                    before: package.version.replace(release.version.clone()),
                    after: Some(release.version),
                });
                continue;
            }
            let wanted = package.reference.as_deref().unwrap_or("HEAD");
            git(
                Some(&root),
                &["fetch", "--quiet", "--depth", "1", "origin", wanted],
            )
            .map_err(fail)?;
            git(Some(&root), &["reset", "--quiet", "--hard", "FETCH_HEAD"]).map_err(fail)?;
            let commit = git(Some(&root), &["rev-parse", "HEAD"]).ok();
            done.push(Updated {
                name: package.name.clone(),
                before: package.commit.clone(),
                after: commit.clone(),
            });
            package.commit = commit;
        }
        self.save(all)?;
        Ok(done)
    }

    /// Forgets `name` and deletes its clone. A local package's directory is left alone.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let (mut all, index) = self.find(name)?;
        let package = all.remove(index);
        if !package.local {
            let root = self.dir.join(&package.name);
            if root.exists() {
                std::fs::remove_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
            }
        }
        self.save(all)
    }

    /// Offers skill `skill` of package `name` to the model. Returns false when it already was.
    pub fn offer_skill(&self, name: &str, skill: &str) -> Result<bool, String> {
        let (mut all, index) = self.find(name)?;
        let root = self.root(&all[index]);
        let known = contributions(&root).skill_names();
        if !known.iter().any(|known| known == skill) {
            return Err(format!(
                "package `{name}` has no skill `{skill}`; it has: {}",
                known.join(", ")
            ));
        }
        let Some(offered) = &mut all[index].model_invocable else {
            // Every skill of the package is offered already.
            return Ok(false);
        };
        if offered.iter().any(|offered| offered == skill) {
            return Ok(false);
        }
        offered.push(skill.to_string());
        self.save(all)?;
        Ok(true)
    }

    /// What the installed packages add to a session.
    pub fn load(&self) -> Loaded {
        let mut loaded = Loaded::default();
        let all = match self.list() {
            Ok(all) => all,
            Err(why) => {
                loaded.problem = Some(why);
                return loaded;
            }
        };
        for package in all {
            let found = contributions(&self.root(&package));
            for path in found.skills {
                loaded.skill_roots.push(SkillRoot {
                    single: is_skill_dir(&path),
                    model_invocable: package.model_invocable.clone(),
                    hooks: package.code_enabled,
                    ..SkillRoot::new(path, SkillSource::Package(package.name.clone()))
                });
            }
            loaded.agent_roots.extend(found.agents);
            loaded.command_roots.extend(found.commands);
            if package.code_enabled {
                for script in found.extensions {
                    let stem = script.file_stem().map(|stem| stem.to_string_lossy().into_owned());
                    let id = format!("{}-{}", package.name, stem.unwrap_or_default());
                    loaded.scripts.push((id, script));
                }
                loaded.mcp_files.extend(found.mcp);
            }
        }
        loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::discover;

    const SKILL: &str = "---\nname: NAME\ndescription: Use when testing packages.\n---\nBody.\n";
    const HOOKED: &str = "---\nname: NAME\ndescription: Use when testing packages.\nhooks:\n  Stop:\n    - hooks:\n        - type: command\n          command: echo stop\n---\nBody.\n";

    fn write(root: &Path, file: &str, text: &str) {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn skill(root: &Path, dir: &str, name: &str) {
        write(root, &format!("{dir}/SKILL.md"), &SKILL.replace("NAME", name));
    }

    fn store(home: &Path) -> Packages {
        Packages {
            file: home.join("packages.toml"),
            dir: home.join("packages"),
            npm_registry: npm::DEFAULT_REGISTRY.to_string(),
        }
    }

    /// A git repository holding `files`, with one commit.
    fn repository(dir: &Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        for (file, text) in files {
            write(dir, file, text);
        }
        let run = |args: &[&str]| {
            let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@example.invalid"];
            all.extend(args);
            git(Some(dir), &all).unwrap();
        };
        run(&["init", "--quiet", "--initial-branch", "main"]);
        run(&["add", "--all"]);
        run(&["commit", "--quiet", "--message", "one"]);
    }

    fn names(loaded: &Loaded) -> Vec<String> {
        discover(&loaded.skill_roots).skills.into_keys().collect()
    }

    /// The `file:` address git clones the repository at `dir` from.
    fn file_url(dir: &Path) -> String {
        let path = dir.display().to_string().replace('\\', "/");
        format!("file:///{}", path.trim_start_matches('/'))
    }

    #[test]
    fn the_manifest_names_every_kind_of_contribution_and_cannot_point_outside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            MANIFEST,
            "name = \"my-pack\"\nskills = [\"skills\", \"../elsewhere\", \"missing\"]\nagents = [\"./agents\"]\n\
             commands = [\"commands\"]\nextensions = [\"ext/main.luau\", \"/etc/passwd\"]\nmcp = \"mcp.json\"\n",
        );
        skill(root, "skills/alpha", "alpha");
        // The conventional layout is not consulted once a manifest exists.
        write(root, "SKILL.md", &SKILL.replace("NAME", "root-skill"));
        write(root, "agents/a.md", "x");
        write(root, "commands/c.md", "x");
        write(root, "ext/main.luau", "-- script");
        write(root, "mcp.json", "{}");

        let found = contributions(root);
        assert_eq!(
            found,
            Contributions {
                skills: vec![root.join("skills")],
                agents: vec![root.join("agents")],
                commands: vec![root.join("commands")],
                extensions: vec![root.join("ext").join("main.luau")],
                mcp: Some(root.join("mcp.json")),
            }
        );
        assert_eq!(found.skill_names(), ["alpha"]);
        assert!(found.has_code());
    }

    #[test]
    fn plugin_manifests_and_the_conventional_layout_are_understood() {
        let dir = tempfile::tempdir().unwrap();

        // A Claude marketplace lists single skill directories under each plugin's source.
        let marketplace = dir.path().join("marketplace");
        write(
            &marketplace,
            ".claude-plugin/marketplace.json",
            r#"{"plugins":[
                {"name":"a","source":"./","skills":["./skills/taste"]},
                {"name":"b","source":"plugins/b","skills":["skills/inner","../../skills/taste/../../.."]},
                {"name":"c","source":{"source":"url","url":"https://example.invalid/c.git"},"skills":["skills/taste"]}
            ]}"#,
        );
        skill(&marketplace, "skills/taste", "taste");
        skill(&marketplace, "skills/unlisted", "unlisted");
        skill(&marketplace, "plugins/b/skills/inner", "inner");
        let found = contributions(&marketplace);
        assert_eq!(
            found.skills,
            [
                marketplace.join("skills").join("taste"),
                marketplace.join("plugins/b").join("skills/inner")
            ]
        );
        assert_eq!(found.skill_names(), ["taste", "inner"]);
        assert!(!found.has_code());

        // A Claude plugin manifest names skill directories as a string or a list.
        let plugin = dir.path().join("plugin");
        write(
            &plugin,
            ".claude-plugin/plugin.json",
            r#"{"name":"p","skills":["./more"]}"#,
        );
        skill(&plugin, "more/one", "one");
        skill(&plugin, "skills/ignored", "ignored");
        assert_eq!(contributions(&plugin).skill_names(), ["one"]);

        // A Codex plugin manifest names one directory of skills.
        let codex = dir.path().join("codex");
        write(
            &codex,
            ".codex-plugin/plugin.json",
            r#"{"name":"p","skills":"./skills/"}"#,
        );
        skill(&codex, "skills/b", "b");
        skill(&codex, "skills/a", "a");
        assert_eq!(contributions(&codex).skill_names(), ["a", "b"]);

        // Without any manifest the directories speak for themselves.
        let plain = dir.path().join("plain");
        skill(&plain, "skills/nested", "nested");
        write(&plain, "SKILL.md", &HOOKED.replace("NAME", "whole"));
        write(&plain, "agents/a.md", "x");
        write(&plain, "ext/main.luau", "-- not announced, so not loaded");
        let found = contributions(&plain);
        assert_eq!(found.skill_names(), ["nested", "whole"]);
        assert_eq!((found.agents.len(), found.commands.len()), (1, 0));
        assert_eq!((found.extensions.len(), &found.mcp), (0, &None));
        // A skill that carries hooks is code.
        assert!(found.has_code());
    }

    #[test]
    fn sources_are_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("My Pack")).unwrap();
        let git = |url: &str, reference: Option<&str>| Source::Git {
            url: url.to_string(),
            reference: reference.map(str::to_string),
        };
        let of = |text: &str| resolve(text, dir.path());

        let github = of("github:Acme/Tool.Kit@v2").unwrap();
        assert_eq!(github.source, git("https://github.com/Acme/Tool.Kit", Some("v2")));
        assert_eq!(github.name, "tool.kit");
        assert_eq!(of("https://example.com/x/pack.git/").unwrap().name, "pack");
        assert_eq!(
            of("git@example.com:x/Pack.git").unwrap(),
            Resolved {
                source: git("git@example.com:x/Pack.git", None),
                name: "pack".into(),
                model_invocable: None
            }
        );

        let taste = of("taste").unwrap();
        assert_eq!(taste.source, git("https://github.com/Leonxlnx/taste-skill", None));
        assert_eq!((taste.name.as_str(), &taste.model_invocable), ("taste", &None));
        assert_eq!(of("design-styles").unwrap().model_invocable, Some(Vec::new()));

        let scoped = of("npm:@acme/review-kit@2.1.0").unwrap();
        assert_eq!(
            scoped.source,
            Source::Npm {
                name: "@acme/review-kit".into(),
                wanted: Some("2.1.0".into())
            }
        );
        assert_eq!(scoped.name, "review-kit");
        assert_eq!(
            of("npm:left-pad").unwrap().source,
            Source::Npm {
                name: "left-pad".into(),
                wanted: None
            }
        );
        assert!(of("npm:Not A Name").is_err());

        let local = of("My Pack").unwrap();
        assert_eq!(
            local.source,
            Source::Local(dunce::canonicalize(dir.path().join("My Pack")).unwrap())
        );
        assert_eq!(local.name, "my-pack");

        for bad in ["github:just-a-name", "github:a/b@", "nothing-like-this", ""] {
            assert!(of(bad).is_err(), "{bad:?}");
        }
        assert!(
            of("nothing-like-this")
                .unwrap_err()
                .contains("components, design-styles, taste")
        );
    }

    #[test]
    fn a_package_is_cloned_loaded_updated_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream");
        repository(
            &upstream,
            &[
                (
                    MANIFEST,
                    "skills = [\"skills\"]\nextensions = [\"ext/main.luau\"]\nmcp = \"mcp.json\"\n",
                ),
                ("skills/one/SKILL.md", &HOOKED.replace("NAME", "one")),
                ("skills/two/SKILL.md", &SKILL.replace("NAME", "two")),
                ("ext/main.luau", "-- script"),
                ("mcp.json", "{\"mcpServers\":{}}"),
                ("LICENSE", "text"),
            ],
        );
        let url = file_url(&upstream);
        let store = store(&dir.path().join("home"));
        let resolved = || resolve(&url, dir.path()).unwrap();
        let asked = std::cell::Cell::new(0);
        let decline = |_: &Contributions| {
            asked.set(asked.get() + 1);
            false
        };

        // Declined: the skills load, without their hooks; nothing that runs code does.
        let added = store.add(resolved(), Some("pack"), &decline).unwrap();
        assert_eq!(asked.get(), 1);
        assert!(!added.no_licence && !added.package.code_enabled && !added.package.local);
        assert_eq!(added.package.commit.as_deref().map(str::len), Some(40));
        assert_eq!(added.contributions.skill_names(), ["one", "two"]);
        let loaded = store.load();
        assert_eq!(names(&loaded), ["one", "two"]);
        let skills = discover(&loaded.skill_roots).skills;
        assert_eq!(skills["one"].hooks, None);
        assert_eq!(skills["one"].source, SkillSource::Package("pack".into()));
        assert_eq!((loaded.scripts.len(), loaded.mcp_files.len()), (0, 0));

        // The same name cannot be installed twice; another name can, with its code enabled.
        let again = store.add(resolved(), Some("pack"), &decline).unwrap_err();
        assert!(
            again.starts_with("a package named `pack` is installed"),
            "{again}"
        );
        store.add(resolved(), Some("trusted"), &|_| true).unwrap();
        let loaded = store.load();
        let root = dir.path().join("home").join("packages").join("trusted");
        assert_eq!(
            loaded.scripts,
            [("trusted-main".to_string(), root.join("ext").join("main.luau"))]
        );
        assert_eq!(loaded.mcp_files, [root.join("mcp.json")]);
        // The later package's skills replace the earlier one's, hooks included.
        assert!(discover(&loaded.skill_roots).skills["one"].hooks.is_some());

        // An update follows the upstream branch.
        write(
            &upstream,
            "skills/three/SKILL.md",
            &SKILL.replace("NAME", "three"),
        );
        git(Some(&upstream), &["add", "--all"]).unwrap();
        git(
            Some(&upstream),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "--quiet",
                "--message",
                "two",
            ],
        )
        .unwrap();
        let updated = store.update(Some("pack")).unwrap();
        assert_eq!(updated.len(), 1);
        let Updated { name, before, after } = &updated[0];
        assert_eq!((name.as_str(), before), ("pack", &added.package.commit));
        assert_ne!(after, before);
        assert_eq!(store.list().unwrap()[0].commit, *after);
        assert!(names(&store.load()).contains(&"three".to_string()));

        store.remove("pack").unwrap();
        assert!(!dir.path().join("home").join("packages").join("pack").exists());
        let missing = store.remove("pack").unwrap_err();
        assert_eq!(
            missing,
            "no package named `pack` is installed; there are: trusted"
        );
        assert!(store.update(Some("pack")).is_err());
    }

    #[tokio::test]
    async fn an_npm_package_is_downloaded_checked_unpacked_updated_and_removed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let files = |version: &str| {
            npm::testing::tarball(&[
                (
                    "azazel.toml",
                    "skills = [\"skills\"]\nextensions = [\"ext/main.luau\"]\n",
                ),
                ("skills/review/SKILL.md", &SKILL.replace("NAME", "review")),
                ("ext/main.luau", &format!("-- {version}")),
                ("LICENSE", "MIT"),
                ("package.json", "{}"),
            ])
        };
        // What the registry says about the package: one version at first, a newer one later.
        let describe = |version: &'static str| {
            let url = format!("{}/tarballs/kit-{version}.tgz", server.uri());
            npm::testing::packument("@acme/review-kit", version, &url, &files(version))
        };
        for version in ["1.0.0", "1.1.0"] {
            Mock::given(method("GET"))
                .and(path(format!("/tarballs/kit-{version}.tgz")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(files(version)))
                .mount(&server)
                .await;
        }
        let first = Mock::given(method("GET"))
            .and(path("/@acme%2Freview-kit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(describe("1.0.0")))
            .mount_as_scoped(&server)
            .await;

        let home = tempfile::tempdir().unwrap();
        let store = Packages {
            npm_registry: server.uri(),
            ..store(home.path())
        };
        let resolved = resolve("npm:@acme/review-kit", home.path()).unwrap();
        let added = store.add(resolved.clone(), None, &|_| true).unwrap();
        assert_eq!(added.package.name, "review-kit");
        assert_eq!(added.package.source, "npm:@acme/review-kit");
        assert_eq!(added.package.version.as_deref(), Some("1.0.0"));
        assert_eq!((&added.package.reference, &added.package.commit), (&None, &None));
        assert!(added.package.code_enabled && !added.no_licence);
        let root = home.path().join("packages/review-kit");
        assert_eq!(store.root(&added.package), root);
        // The files sit at the root of the package, not in the `package` directory of the tarball.
        assert_eq!(
            std::fs::read_to_string(root.join("ext/main.luau")).unwrap(),
            "-- 1.0.0"
        );

        let loaded = store.load();
        assert_eq!(names(&loaded), ["review"]);
        assert_eq!(
            loaded.scripts,
            [("review-kit-main".to_string(), root.join("ext/main.luau"))]
        );
        let again = store.add(resolved, None, &|_| true).unwrap_err();
        assert!(
            again.contains("a package named `review-kit` is installed"),
            "{again}"
        );

        // Nothing newer: nothing is downloaded again.
        std::fs::write(root.join("mine.txt"), "kept").unwrap();
        let same = store.update(Some("review-kit")).unwrap();
        assert_eq!(
            (same[0].before.as_deref(), same[0].after.as_deref()),
            (Some("1.0.0"), Some("1.0.0"))
        );
        assert!(root.join("mine.txt").exists());

        drop(first);
        Mock::given(method("GET"))
            .and(path("/@acme%2Freview-kit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(describe("1.1.0")))
            .mount(&server)
            .await;
        let newer = store.update(None).unwrap();
        assert_eq!(
            (newer[0].before.as_deref(), newer[0].after.as_deref()),
            (Some("1.0.0"), Some("1.1.0"))
        );
        assert_eq!(
            std::fs::read_to_string(root.join("ext/main.luau")).unwrap(),
            "-- 1.1.0"
        );
        // The old files are gone, not mixed with the new.
        assert!(!root.join("mine.txt").exists());
        assert_eq!(store.list().unwrap()[0].version.as_deref(), Some("1.1.0"));

        store.remove("review-kit").unwrap();
        assert!(!root.exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_npm_package_that_cannot_be_checked_leaves_nothing_behind() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let described = npm::testing::tarball(&[("SKILL.md", "as published")]);
        let url = format!("{}/kit.tgz", server.uri());
        Mock::given(method("GET"))
            .and(path("/kit"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(npm::testing::packument("kit", "3.0.0", &url, &described)),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/kit.tgz"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(npm::testing::tarball(&[("SKILL.md", "swapped")])),
            )
            .mount(&server)
            .await;

        let home = tempfile::tempdir().unwrap();
        let store = Packages {
            npm_registry: server.uri(),
            ..store(home.path())
        };
        let of = |text: &str| resolve(text, home.path()).unwrap();
        let why = store.add(of("npm:kit"), None, &|_| true).unwrap_err();
        assert!(
            why.starts_with("could not install kit from npm:") && why.contains("SHA-512 differs"),
            "{why}"
        );
        let why = store.add(of("npm:kit@9.9.9"), None, &|_| true).unwrap_err();
        assert!(why.contains("kit has no version or tag `9.9.9`"), "{why}");
        let why = store.add(of("npm:nothing-here"), None, &|_| true).unwrap_err();
        assert!(
            why.contains("the registry has no package named nothing-here"),
            "{why}"
        );
        assert!(!home.path().join("packages/kit").exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_local_package_is_used_in_place_and_its_skills_can_be_offered_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("styles");
        skill(&local, "skills/bold", "bold");
        skill(&local, "skills/calm", "calm");
        let store = store(&dir.path().join("home"));
        let mut resolved = resolve("styles", dir.path()).unwrap();
        // As a registry entry would: nothing is offered to the model at first.
        resolved.model_invocable = Some(Vec::new());

        let added = store
            .add(resolved, None, &|_| panic!("nothing here runs code"))
            .unwrap();
        assert!(added.package.local && !added.no_licence && added.package.commit.is_none());
        let offered = |store: &Packages| -> Vec<String> {
            discover(&store.load().skill_roots)
                .skills
                .into_values()
                .filter(|skill| skill.model_invocable)
                .map(|skill| skill.name)
                .collect()
        };
        assert_eq!(names(&store.load()), ["bold", "calm"]);
        assert_eq!(offered(&store), Vec::<String>::new());

        assert_eq!(store.offer_skill("styles", "calm"), Ok(true));
        assert_eq!(store.offer_skill("styles", "calm"), Ok(false));
        assert_eq!(offered(&store), ["calm"]);
        let unknown = store.offer_skill("styles", "loud").unwrap_err();
        assert_eq!(
            unknown,
            "package `styles` has no skill `loud`; it has: bold, calm"
        );

        let untouched = Updated {
            name: "styles".into(),
            before: None,
            after: None,
        };
        assert_eq!(store.update(None).unwrap(), [untouched]);
        store.remove("styles").unwrap();
        assert!(local.join("skills").join("bold").join("SKILL.md").is_file());
        assert_eq!(store.load(), Loaded::default());
    }

    #[test]
    fn a_missing_licence_and_a_failed_clone_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("bare-bones");
        repository(&upstream, &[("SKILL.md", &SKILL.replace("NAME", "whole"))]);
        let url = file_url(&upstream);
        let store = store(&dir.path().join("home"));

        let added = store
            .add(resolve(&url, dir.path()).unwrap(), None, &|_| true)
            .unwrap();
        assert!(added.no_licence);
        assert_eq!(added.package.name, "bare-bones");
        // Nothing in it runs code, so nothing was enabled.
        assert!(!added.package.code_enabled);
        // The package is one skill: its root is the skill's directory.
        assert_eq!(names(&store.load()), ["whole"]);

        let gone = resolve("file:///nowhere/at/all.git", dir.path()).unwrap();
        let failed = store.add(gone, None, &|_| true).unwrap_err();
        assert!(
            failed.starts_with("could not clone file:///nowhere/at/all.git: "),
            "{failed}"
        );
        assert!(!dir.path().join("home").join("packages").join("all").exists());
        assert_eq!(store.list().unwrap().len(), 1);

        std::fs::write(dir.path().join("home").join("packages.toml"), "package = 3").unwrap();
        assert!(store.load().problem.is_some());
    }

    #[test]
    fn the_registry_parses_and_names_are_checked() {
        let known = registry();
        assert_eq!(
            known.keys().collect::<Vec<_>>(),
            ["components", "design-styles", "taste"]
        );
        assert!(
            known
                .values()
                .all(|entry| entry.source.starts_with("https://github.com/"))
        );
        assert!(check_name("my-pack_2.0").is_ok());
        for bad in ["", "My Pack", "../x", ".hidden", "a/b"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }
}
