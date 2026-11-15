//! Installing language servers into `~/.azazel/lsp/`, never system-wide. A recipe says how a
//! server is installed; which program does it (`npm` or `bun`, `rustup`, `go`) is found when the
//! install is asked for, so a plan always names something that exists.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use azazel_core::config::LspServerConfig;

use super::servers::{BUILTINS, find_program_in, specs};

/// How long one install may take before it is given up.
pub(crate) const LIMIT: Duration = Duration::from_secs(600);

/// How a built-in server is installed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Recipe {
    /// rustup's own component, else the release of the rust-analyzer project for this platform.
    RustAnalyzer,
    /// These npm packages into `<lsp>/npm`.
    Npm(&'static [&'static str]),
    /// `go install` of this module, into `<lsp>/bin`.
    Go(&'static str),
    /// No recipe: what to do instead.
    Manual(&'static str),
}

/// Where programs are looked for: the folders azazel installs into, then the system's `PATH`.
#[derive(Clone, Debug)]
pub struct Places {
    /// `~/.azazel/lsp`.
    pub lsp_dir: PathBuf,
    /// The directories of `PATH`.
    pub path: Vec<PathBuf>,
}

impl Places {
    /// `lsp_dir` and the `PATH` of this process.
    pub fn system(lsp_dir: PathBuf) -> Places {
        let path = std::env::var_os("PATH").unwrap_or_default();
        Places {
            lsp_dir,
            path: std::env::split_paths(&path).collect(),
        }
    }

    /// The directories azazel installs programs into.
    fn managed(&self) -> [PathBuf; 2] {
        [
            self.lsp_dir.join("bin"),
            self.lsp_dir.join("npm").join("node_modules").join(".bin"),
        ]
    }

    /// A program an install needs (`npm`, `go`, `rustup`): on `PATH`.
    fn tool(&self, name: &str) -> Option<PathBuf> {
        find_program_in(&self.path, name)
    }

    /// A server's program: what azazel installed, else `PATH`. The `rust-analyzer` that rustup
    /// puts on `PATH` whether or not the component is installed does not count.
    pub(crate) fn find(&self, command: &str) -> Option<PathBuf> {
        let dirs: Vec<PathBuf> = self
            .managed()
            .into_iter()
            .chain(self.path.iter().cloned())
            .collect();
        let found = find_program_in(&dirs, command)?;
        if command == "rust-analyzer" && !found.starts_with(&self.lsp_dir) && !self.is_installed(&found) {
            return None;
        }
        Some(found)
    }

    /// Whether `found` is a real `rust-analyzer`. With rustup, `~/.cargo/bin/rust-analyzer` is a
    /// proxy that exists whether or not the component is installed and fails when it is not;
    /// `rustup which` tells the two apart.
    fn is_installed(&self, found: &Path) -> bool {
        let Some(rustup) = self.tool("rustup") else {
            return true;
        };
        if rustup.parent() != found.parent() {
            return true;
        }
        std::process::Command::new(rustup)
            .args(["which", "rust-analyzer"])
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success())
    }
}

enum Step {
    Run {
        program: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
        /// Folders the program expects to exist.
        make: Vec<PathBuf>,
    },
    Release {
        dir: PathBuf,
    },
}

/// What installing a server does, found for this machine.
pub(crate) struct Plan {
    /// The command line as a user would type it, for messages.
    pub shown: String,
    step: Step,
}

/// The plan for `recipe`, or what to do by hand.
pub(crate) fn plan(recipe: Recipe, places: &Places) -> Result<Plan, String> {
    let find = |name: &str| places.tool(name);
    let lsp_dir = &places.lsp_dir;
    match recipe {
        Recipe::Manual(how) => Err(how.to_string()),
        Recipe::RustAnalyzer => {
            if let Some(rustup) = find("rustup") {
                return Ok(Plan {
                    shown: "rustup component add rust-analyzer".to_string(),
                    step: Step::Run {
                        program: rustup,
                        args: ["component", "add", "rust-analyzer"].map(String::from).to_vec(),
                        env: Vec::new(),
                        make: Vec::new(),
                    },
                });
            }
            if release_asset().is_none() {
                return Err("install it with rustup (`rustup component add rust-analyzer`)".to_string());
            }
            let dir = lsp_dir.join("bin");
            Ok(Plan {
                shown: format!(
                    "download rust-analyzer from its GitHub release into {}",
                    dir.display()
                ),
                step: Step::Release { dir },
            })
        }
        Recipe::Npm(packages) => {
            let prefix = lsp_dir.join("npm");
            let prefix_text = prefix.to_string_lossy().into_owned();
            let packages: Vec<String> = packages.iter().map(|package| package.to_string()).collect();
            let (program, mut args, name) = if let Some(npm) = find("npm") {
                let args = [
                    "install",
                    "--prefix",
                    prefix_text.as_str(),
                    "--no-audit",
                    "--no-fund",
                ];
                (npm, args.map(String::from).to_vec(), "npm")
            } else if let Some(bun) = find("bun") {
                let args = ["add", "--cwd", prefix_text.as_str()];
                (bun, args.map(String::from).to_vec(), "bun")
            } else {
                return Err(format!(
                    "install Node.js (npm) or bun, then `npm install -g {}`",
                    packages.join(" ")
                ));
            };
            args.extend(packages);
            Ok(Plan {
                shown: format!("{name} {}", args.join(" ")),
                step: Step::Run {
                    program,
                    args,
                    env: Vec::new(),
                    make: vec![prefix],
                },
            })
        }
        Recipe::Go(module) => {
            let Some(go) = find("go") else {
                return Err(format!(
                    "install Go from https://go.dev/dl, then `go install {module}`"
                ));
            };
            let bin = lsp_dir.join("bin");
            Ok(Plan {
                shown: format!("GOBIN={} go install {module}", bin.display()),
                step: Step::Run {
                    program: go,
                    args: vec!["install".to_string(), module.to_string()],
                    env: vec![("GOBIN".to_string(), bin.to_string_lossy().into_owned())],
                    make: vec![bin],
                },
            })
        }
    }
}

/// One language server azazel knows about, for `azazel lsp list` and `azazel doctor`.
pub struct Known {
    pub name: String,
    pub command: String,
    /// Lower-case, without the dot.
    pub extensions: Vec<String>,
    /// Where the program is, when it is installed.
    pub found: Option<PathBuf>,
    /// The program is under `~/.azazel/lsp/`.
    pub managed: bool,
    /// A built-in default, which azazel can install; a configured server is the user's.
    pub builtin: bool,
    /// The command that installs it, or what to do instead; `None` for a configured server.
    pub install: Option<String>,
    /// `install` is a command `azazel lsp install` runs.
    pub installable: bool,
}

/// The servers of a configuration, with whether and where each is installed.
pub fn known(configured: &BTreeMap<String, LspServerConfig>, places: &Places) -> Vec<Known> {
    specs(configured)
        .into_iter()
        .map(|spec| {
            let found = places.find(&spec.command);
            let (install, installable) = match spec.recipe.map(|recipe| plan(recipe, places)) {
                Some(Ok(plan)) => (Some(plan.shown), true),
                Some(Err(how)) => (Some(how), false),
                None => (None, false),
            };
            Known {
                managed: found
                    .as_ref()
                    .is_some_and(|path| path.starts_with(&places.lsp_dir)),
                found,
                name: spec.name,
                command: spec.command,
                extensions: spec.extensions,
                builtin: spec.builtin,
                install,
                installable,
            }
        })
        .collect()
}

/// Installs the built-in server `name` and returns where its program is.
pub async fn install(name: &str, places: &Places) -> Result<PathBuf, String> {
    let Some(builtin) = BUILTINS.iter().find(|builtin| builtin.name == name) else {
        let names: Vec<&str> = BUILTINS.iter().map(|builtin| builtin.name).collect();
        return Err(format!(
            "`{name}` is not a server azazel knows how to install; they are: {}",
            names.join(", ")
        ));
    };
    if let Some(found) = places.find(builtin.command) {
        return Ok(found);
    }
    let plan = plan(builtin.recipe, places)
        .map_err(|how| format!("`{}` cannot be installed by azazel here: {how}", builtin.command))?;
    run(&plan).await?;
    places.find(builtin.command).ok_or_else(|| {
        format!(
            "`{}` ran, but `{}` is still not found",
            plan.shown, builtin.command
        )
    })
}

/// The extensions of the files under `root`, as far as a quick look goes: what the project is
/// written in.
pub fn extensions_in(root: &Path) -> BTreeSet<String> {
    const LOOKED_AT: usize = 5000;
    let mut found = BTreeSet::new();
    let walk = ignore::WalkBuilder::new(root).max_depth(Some(6)).build();
    for entry in walk.flatten().take(LOOKED_AT) {
        if let Some(extension) = entry.path().extension() {
            found.insert(extension.to_string_lossy().to_lowercase());
        }
    }
    found
}

/// The servers the user answered "never" to, kept in `<lsp_dir>/declined`, one name per line.
pub(crate) fn declined(lsp_dir: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(lsp_dir.join("declined"))
        .unwrap_or_default()
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

pub(crate) fn decline(lsp_dir: &Path, name: &str) {
    let mut names = declined(lsp_dir);
    names.insert(name.to_string());
    let text: String = names.iter().map(|name| format!("{name}\n")).collect();
    let written =
        std::fs::create_dir_all(lsp_dir).and_then(|()| std::fs::write(lsp_dir.join("declined"), text));
    if let Err(why) = written {
        tracing::debug!("could not remember that {name} is declined: {why}");
    }
}

/// The last lines of what a failed program printed.
fn tail(stdout: &[u8], stderr: &[u8]) -> String {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(stderr),
        String::from_utf8_lossy(stdout)
    );
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}

/// Carries `plan` out, within [`LIMIT`].
pub(crate) async fn run(plan: &Plan) -> Result<(), String> {
    let work = async {
        match &plan.step {
            Step::Run {
                program,
                args,
                env,
                make,
            } => {
                for dir in make {
                    std::fs::create_dir_all(dir)
                        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
                }
                let command = azazel_core::process::helper_command(program, |command| {
                    command
                        .args(args)
                        .envs(env.iter().map(|(key, value)| (key, value)))
                        .stdin(std::process::Stdio::null());
                });
                let output = azazel_core::process::helper_output(command)
                    .await
                    .map_err(|e| format!("could not run `{}`: {e}", plan.shown))?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "`{}` failed ({}): {}",
                        plan.shown,
                        output.status,
                        tail(&output.stdout, &output.stderr)
                    ))
                }
            }
            Step::Release { dir } => {
                let (asset, kind) = release_asset().ok_or("this platform has no rust-analyzer release")?;
                let url =
                    format!("https://github.com/rust-lang/rust-analyzer/releases/latest/download/{asset}");
                download(&url, kind, dir).await.map(|_| ())
            }
        }
    };
    match tokio::time::timeout(LIMIT, work).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "`{}` took longer than {} minutes",
            plan.shown,
            LIMIT.as_secs() / 60
        )),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Archive {
    /// One gzip-compressed executable.
    Gz,
    /// A zip holding `rust-analyzer.exe`.
    Zip,
}

/// The name of the rust-analyzer release file for this platform, and how it is packed.
fn release_asset() -> Option<(String, Archive)> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    match std::env::consts::OS {
        "linux" => Some((format!("rust-analyzer-{arch}-unknown-linux-gnu.gz"), Archive::Gz)),
        "macos" => Some((format!("rust-analyzer-{arch}-apple-darwin.gz"), Archive::Gz)),
        "windows" => Some((format!("rust-analyzer-{arch}-pc-windows-msvc.zip"), Archive::Zip)),
        _ => None,
    }
}

/// Fetches `url`, unpacks the executable it holds into `dir` and returns its path.
async fn download(url: &str, kind: Archive, dir: &Path) -> Result<PathBuf, String> {
    let bytes = crate::util::download(url).await?;
    unpack(&bytes, kind, dir)
}

/// Writes the executable in `bytes` as `rust-analyzer` into `dir`.
fn unpack(bytes: &[u8], kind: Archive, dir: &Path) -> Result<PathBuf, String> {
    let mut program = Vec::new();
    match kind {
        Archive::Gz => {
            flate2::read::GzDecoder::new(bytes)
                .read_to_end(&mut program)
                .map_err(|e| format!("the download is not a gzip file: {e}"))?;
        }
        Archive::Zip => {
            let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
                .map_err(|e| format!("the download is not a zip file: {e}"))?;
            let mut file = archive
                .by_name("rust-analyzer.exe")
                .map_err(|e| format!("the download has no rust-analyzer.exe: {e}"))?;
            file.read_to_end(&mut program)
                .map_err(|e| format!("could not unpack rust-analyzer.exe: {e}"))?;
        }
    }
    crate::util::install_program(&program, dir, "rust-analyzer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gzipped_or_zipped_release_becomes_the_program() {
        let program = b"#!/bin/sh\nexit 0\n";
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(program).unwrap();
        let gz = gz.finish().unwrap();
        let mut zipped = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zipped
            .start_file("rust-analyzer.exe", zip::write::SimpleFileOptions::default())
            .unwrap();
        zipped.write_all(program).unwrap();
        let zipped = zipped.finish().unwrap().into_inner();
        for (bytes, kind) in [(gz, Archive::Gz), (zipped, Archive::Zip)] {
            let dir = tempfile::tempdir().unwrap();
            let target = unpack(&bytes, kind, dir.path()).unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), program);
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                1,
                "no partial file is left"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(unpack(b"not gzip", Archive::Gz, dir.path()).is_err());
    }
}
