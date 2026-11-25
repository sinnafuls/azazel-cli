//! Workspace maintenance tasks, run as `cargo xtask <task>`.

mod prune;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use azazel_ai::catalog::{CatalogFile, Cost, ModelInfo};
use azazel_ai::types::{ThinkingLevel, WireApi};
use serde_json::Value;
use sha2::{Digest, Sha256};
use xtask::TASKS;

const CATALOG_SOURCE: &str = "https://models.dev/api.json";
/// The cloud providers, as `(name in the source, provider id here)`.
const CLOUD_SOURCES: [(&str, &str); 3] = [
    ("amazon-bedrock", azazel_ai::providers::presets::BEDROCK),
    ("google-vertex", azazel_ai::providers::presets::VERTEX),
    ("google-vertex-anthropic", azazel_ai::providers::presets::VERTEX),
];

/// The limits `bench-startup` enforces.
const MAX_VERSION: Duration = Duration::from_millis(30);
const MAX_PROMPT: Duration = Duration::from_millis(150);
// The `dist` profile (thin LTO) measured 56.2 MiB; fat LTO, which `release` keeps, 53.9 MiB.
const MAX_BINARY_BYTES: u64 = 58 * 1024 * 1024;
const BENCH_RUNS: usize = 20;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("catalog") => catalog(),
        Some("bench-startup") => bench_startup(),
        Some("dist") => dist(),
        Some("rpc-schema") => rpc_schema(),
        Some("docs-check") => docs_check(),
        Some("prune") => prune::run(&workspace_root(), &std::env::args().skip(2).collect::<Vec<_>>()),
        Some(other) => Err(anyhow::anyhow!(
            "unknown task `{other}`; tasks: {}",
            TASKS.join(", ")
        )),
        None => Err(anyhow::anyhow!(
            "usage: cargo xtask <task>; tasks: {}",
            TASKS.join(", ")
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Where cargo puts what it builds: `CARGO_TARGET_DIR` when it is set (agent worktrees share the
/// main checkout's), else `target/` of the workspace.
fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
}

/// Whether building `package` with its default features turns `feature` on, directly or through
/// another feature. `metadata` is what `cargo metadata --no-deps --format-version 1` prints.
fn default_build_has(metadata: &Value, package: &str, feature: &str) -> bool {
    let features = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["name"] == package)
        .map(|entry| &entry["features"]);
    let Some(features) = features else {
        return false;
    };
    let mut seen = vec!["default".to_string()];
    let mut next = 0;
    while let Some(name) = seen.get(next).cloned() {
        next += 1;
        for enabled in features[name.as_str()].as_array().into_iter().flatten() {
            // `dep:x` and `crate/feature` name other packages, not features of this one.
            match enabled.as_str() {
                Some(other) if !other.contains([':', '/']) && !seen.iter().any(|s| s == other) => {
                    seen.push(other.to_string());
                }
                _ => {}
            }
        }
    }
    seen.iter().any(|name| name == feature)
}

/// The built web page that `crates/azazel-remote/build.rs` embeds: `AZAZEL_WEB_BUILD` when it is
/// set, else `web/build`.
fn web_build(root: &Path) -> PathBuf {
    std::env::var_os("AZAZEL_WEB_BUILD")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("web").join("build"))
}

/// A release must not ship the placeholder page: with the `remote` feature in the build, the page
/// has to be there before the build starts.
fn require_web_build(root: &Path) -> Result<()> {
    let listed = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .stderr(Stdio::inherit())
        .output()
        .context("could not run cargo metadata")?;
    if !listed.status.success() {
        bail!("cargo metadata failed");
    }
    let metadata: Value =
        serde_json::from_slice(&listed.stdout).context("cargo metadata did not print JSON")?;
    let page = web_build(root).join("index.html");
    if default_build_has(&metadata, "azazel-cli", "remote") && !page.is_file() {
        bail!(
            concat!(
                "{} is missing, and the release is built with the `remote` feature, which embeds the web page. ",
                "Build the page first: `bun install --frozen-lockfile` and `bun run build` in web/"
            ),
            page.display()
        );
    }
    Ok(())
}

/// Compares the docs with the code and fails with the list of what differs. The check is the
/// package `docs-check`: it links the whole program to ask it what it registers, which the other
/// tasks should not have to wait for.
fn docs_check() -> Result<()> {
    let status = Command::new(env!("CARGO"))
        .args(["run", "--quiet", "--package", "docs-check"])
        .current_dir(workspace_root())
        .status()
        .context("could not run cargo")?;
    if !status.success() {
        bail!("docs-check failed");
    }
    Ok(())
}

/// The median wall time of [`BENCH_RUNS`] runs of `command`, after one run that is not counted
/// (the first start pays for a cold file cache and for unpacking the built-in skills).
fn median_run(mut command: impl FnMut() -> Command) -> Result<Duration> {
    let mut times = Vec::with_capacity(BENCH_RUNS);
    for run in 0..=BENCH_RUNS {
        let started = Instant::now();
        let status = command()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .context("could not run the release binary")?;
        let took = started.elapsed();
        if !status.success() {
            bail!("the release binary exited with {status}");
        }
        if run > 0 {
            times.push(took);
        }
    }
    times.sort_unstable();
    Ok(times[times.len() / 2])
}

/// `(os, arch)` as Rust names them (`windows`, `linux`, `macos`; `x86_64`, `aarch64`) for a
/// target triple.
fn platform_of(triple: &str) -> Result<(&'static str, String)> {
    let arch = triple.split('-').next().unwrap_or_default().to_string();
    let os = if triple.contains("windows") {
        "windows"
    } else if triple.contains("apple-darwin") {
        "macos"
    } else if triple.contains("linux") {
        "linux"
    } else {
        bail!("unsupported target `{triple}`");
    };
    Ok((os, arch))
}

/// Writes the JSON Schema of the RPC protocol to `docs/rpc.schema.json`. A test of
/// `azazel-protocol` fails while the file is not what this writes.
fn rpc_schema() -> Result<()> {
    let file = workspace_root().join("docs").join("rpc.schema.json");
    let mut text = serde_json::to_string_pretty(&azazel_protocol::schema())?;
    text.push('\n');
    std::fs::write(&file, text).with_context(|| format!("could not write {}", file.display()))?;
    println!("wrote {}", file.display());
    Ok(())
}

/// Builds the `dist` profile binary (for `--target <triple>`, else this machine) and packs it with the
/// installer into `dist/azazel-<os>-<arch>.zip` (Windows) or `.tar.gz`, plus `<archive>.sha256`.
/// The names carry no version, so `releases/latest/download/<name>` always finds the newest.
fn dist() -> Result<()> {
    let root = workspace_root();
    let args: Vec<String> = std::env::args().skip(2).collect();
    let target = match args.as_slice() {
        [] => None,
        [flag, triple] if flag == "--target" => Some(triple.clone()),
        _ => bail!("usage: cargo xtask dist [--target <triple>]"),
    };
    require_web_build(&root)?;
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "--profile", "dist", "-p", "azazel-cli"]);
    if let Some(triple) = &target {
        build.args(["--target", triple]);
    }
    // An archive for Windows must carry the icon and file details: a failed resource compile is
    // an error here, where an ordinary build only warns.
    let for_windows = match &target {
        Some(triple) => triple.contains("windows"),
        None => cfg!(windows),
    };
    if for_windows {
        build.env("AZAZEL_REQUIRE_RESOURCE", "1");
    }
    let built = build.current_dir(&root).status().context("could not run cargo")?;
    if !built.success() {
        bail!("the release build failed");
    }
    let (os, arch, out_dir) = match &target {
        Some(triple) => {
            let (os, arch) = platform_of(triple)?;
            (os, arch, target_dir(&root).join(triple).join("dist"))
        }
        None => (
            std::env::consts::OS,
            std::env::consts::ARCH.to_string(),
            target_dir(&root).join("dist"),
        ),
    };
    let windows = os == "windows";
    let binary = if windows { "azazel.exe" } else { "azazel" };
    let installer = if windows { "install.ps1" } else { "install.sh" };
    let name = format!("azazel-{os}-{arch}");
    let dist = root.join("dist");
    let stage = dist.join(&name);
    if stage.exists() {
        std::fs::remove_dir_all(&stage).with_context(|| format!("cannot clear {}", stage.display()))?;
    }
    std::fs::create_dir_all(&stage)?;
    std::fs::copy(out_dir.join(binary), stage.join(binary)).context("cannot copy the release binary")?;
    std::fs::copy(root.join("install").join(installer), stage.join(installer))
        .context("cannot copy the installer")?;
    let run = if windows {
        "powershell -ExecutionPolicy Bypass -File .\\install.ps1"
    } else {
        "sh ./install.sh"
    };
    let readme = format!(
        "azazel {version}\n\nInstall for the current user and put `azazel` on PATH:\n\n    {run}\n\n\
         Then open a new terminal and run `azazel` in a project directory. The first start walks\n\
         you through signing in or adding an API key (later: /settings).\n\n\
         Remove it again with the same installer and `{flag}` (add `{purge}` to also delete\n\
         ~/.azazel, which holds settings, logins and sessions).\n",
        version = env!("CARGO_PKG_VERSION"),
        flag = if windows { "-Uninstall" } else { "--uninstall" },
        purge = if windows { "-Purge" } else { "--purge" },
    );
    std::fs::write(stage.join("README.txt"), readme)?;

    let archive_name = format!("{name}.{}", if windows { "zip" } else { "tar.gz" });
    let archive = dist.join(&archive_name);
    if archive.exists() {
        std::fs::remove_file(&archive)?;
    }
    // Windows ships bsdtar, which writes zip archives with `-a`; a GNU tar earlier on PATH (Git
    // for Windows) would not.
    let (program, flags) = match windows {
        true => {
            let system = std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .unwrap_or_default();
            (system.join("System32").join("tar.exe"), "-a -cf")
        }
        false => (PathBuf::from("tar"), "-czf"),
    };
    let mut tar = Command::new(program);
    tar.args(flags.split(' '))
        .arg(&archive)
        .arg(&name)
        .current_dir(&dist);
    let packed = tar.status().context("could not run tar")?;
    if !packed.success() {
        bail!("tar failed with {packed}");
    }
    let digest = Sha256::digest(std::fs::read(&archive)?);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let sums = dist.join(format!("{archive_name}.sha256"));
    std::fs::write(&sums, format!("{hex}  {archive_name}\n"))?;
    println!("wrote {}", archive.display());
    println!("wrote {}", sums.display());
    Ok(())
}

/// Builds the `dist` profile binary and fails when it starts too slowly or is too large: the median of
/// 20 runs of `azazel --version` and of `azazel -p hi` against the scripted provider, and the
/// size of the binary. The binary is built with the scripted provider compiled in, which makes it
/// a little larger than the one that ships.
fn bench_startup() -> Result<()> {
    let root = workspace_root();
    let built = Command::new(env!("CARGO"))
        .args([
            "build",
            "--profile",
            "dist",
            "--package",
            "azazel-cli",
            "--features",
            "test-support",
        ])
        .current_dir(&root)
        .status()
        .context("could not run cargo")?;
    if !built.success() {
        bail!("the release build failed");
    }
    let binary = target_dir(&root)
        .join("dist")
        .join(format!("azazel{}", std::env::consts::EXE_SUFFIX));
    let size = std::fs::metadata(&binary)
        .with_context(|| format!("{} is missing", binary.display()))?
        .len();

    let scratch = tempfile::tempdir()?;
    let project = scratch.path().join("project");
    std::fs::create_dir_all(&project)?;
    // Every run is a new process that reads the script from its first step.
    let script = scratch.path().join("script.json");
    std::fs::write(&script, r#"{ "lead": [ { "reply": { "text": "hello" } } ] }"#)?;

    let version = median_run(|| {
        let mut command = Command::new(&binary);
        command.arg("--version");
        command
    })?;
    let prompt = median_run(|| {
        let mut command = Command::new(&binary);
        command
            .env("AZAZEL_HOME", scratch.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script)
            .args(["-p", "hi", "--cwd"])
            .arg(&project);
        command
    })?;

    let megabytes = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    // The size of the previous run, so each change reports what it added.
    let size_file = target_dir(&root).join("bench-size.txt");
    let previous = std::fs::read_to_string(&size_file)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok());
    let _ = std::fs::write(&size_file, size.to_string());
    let delta = match previous {
        Some(before) if before != size => {
            let change = megabytes(size) - megabytes(before);
            format!(", {change:+.2} MB since the last run")
        }
        Some(_) => ", unchanged since the last run".to_string(),
        None => String::new(),
    };
    let rows = [
        (
            "azazel --version",
            format!("{} ms", version.as_millis()),
            format!("{} ms", MAX_VERSION.as_millis()),
            version <= MAX_VERSION,
        ),
        (
            "azazel -p hi (scripted provider)",
            format!("{} ms", prompt.as_millis()),
            format!("{} ms", MAX_PROMPT.as_millis()),
            prompt <= MAX_PROMPT,
        ),
        (
            "binary size",
            format!("{:.1} MB{delta}", megabytes(size)),
            format!("{:.0} MB", megabytes(MAX_BINARY_BYTES)),
            size <= MAX_BINARY_BYTES,
        ),
    ];
    for (what, measured, limit, ok) in &rows {
        let verdict = if *ok { "ok  " } else { "FAIL" };
        println!("{verdict}  {what}: {measured} (limit {limit})");
    }
    if rows.iter().any(|(_, _, _, ok)| !ok) {
        bail!("the startup gate failed");
    }
    Ok(())
}

/// Regenerates, from models.dev: `crates/azazel-ai/catalog/models.json`, the tool-capable text
/// models of the built-in and the cloud providers; `crates/azazel-ai/src/providers/generated.rs`,
/// a provider row for each models.dev provider that speaks a wire API azazel has and no
/// hand-written row covers; and the table of those rows in `docs/models-and-logins.md`.
fn catalog() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let source: Value = runtime.block_on(async {
        let response = azazel_ai::tls::client()
            .get(CATALOG_SOURCE)
            .send()
            .await?
            .error_for_status()?;
        response.json::<Value>().await
    })?;

    let hand_written = azazel_ai::providers::presets::hand_written();
    let generated = generated_rows(&source, &hand_written);

    // Each hand-written provider says what it is called in the source; several may share one.
    let sources = hand_written
        .iter()
        .filter(|(_, name, _)| !name.is_empty())
        .map(|(id, name, _)| (*name, *id))
        .chain(CLOUD_SOURCES);
    let mut models: Vec<ModelInfo> = Vec::new();
    let mut add = |model: ModelInfo| {
        // Two sources of one provider may both list a model; the first listing stands.
        if !models.iter().any(|known| known.key() == model.key()) {
            models.push(model);
        }
    };
    for (source_name, provider) in sources {
        let Some(entries) = source
            .get(source_name)
            .and_then(|p| p.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        entries
            .values()
            .filter_map(|entry| model_info(provider, entry))
            .for_each(&mut add);
    }
    // A generated provider lists the models it serves on the route its row takes; a local
    // server is asked itself.
    for row in generated.iter().filter(|row| !row.keyless) {
        let entries = source
            .pointer(&format!("/{}/models", row.id))
            .and_then(Value::as_object);
        entries
            .into_iter()
            .flat_map(|entries| entries.iter())
            .filter(|(key, _)| row.models.contains(key))
            .filter_map(|(_, entry)| model_info(&row.id, entry))
            .for_each(&mut add);
    }
    if models.is_empty() {
        bail!("{CATALOG_SOURCE} contained no models for the built-in providers; its format may have changed");
    }
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));

    // One model per line keeps diffs reviewable.
    let mut out = String::from("{\"models\":[\n");
    for (i, model) in models.iter().enumerate() {
        out.push_str(&serde_json::to_string(model)?);
        out.push_str(if i + 1 < models.len() { ",\n" } else { "\n" });
    }
    out.push_str("]}\n");
    serde_json::from_str::<CatalogFile>(&out).context("generated catalogue does not parse")?;

    let root = workspace_root();
    let path = root.join("crates/azazel-ai/catalog/models.json");
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {} models to {}", models.len(), path.display());

    let path = root.join("crates/azazel-ai/src/providers/generated.rs");
    std::fs::write(&path, generated_source(&generated))
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {} providers to {}", generated.len(), path.display());

    let path = root.join("docs/models-and-logins.md");
    let page = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    std::fs::write(&path, with_generated_table(&page, &generated)?)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote the table of generated providers to {}", path.display());
    Ok(())
}

/// The AI SDK packages of models.dev whose wire API azazel speaks, in the order a provider
/// whose models take several routes prefers them when as many models take each.
const PACKAGE_WIRES: [(&str, WireApi); 4] = [
    ("@ai-sdk/anthropic", WireApi::AnthropicMessages),
    ("@ai-sdk/openai", WireApi::OpenAiResponses),
    ("@ai-sdk/openai-compatible", WireApi::OpenAiChat),
    ("@ai-sdk/google", WireApi::GoogleGenerative),
];

/// The lines of `docs/models-and-logins.md` the table of generated providers sits between.
const TABLE_START: &str = "<!-- generated by `cargo xtask catalog`: do not edit by hand -->";
const TABLE_END: &str = "<!-- end of the generated table -->";

/// A provider row made from a models.dev provider.
#[derive(Debug, PartialEq)]
struct GeneratedRow {
    id: String,
    name: String,
    api: WireApi,
    base_url: String,
    /// The variables that hold the provider's key, in the source's order.
    env: Vec<String>,
    /// A server on this machine, called without a key when none is set.
    keyless: bool,
    /// The source's keys of the models served on the row's route.
    models: Vec<String>,
}

/// A row for each provider of `source` that no `hand_written` row (`(id, models.dev name, key
/// variables)`) covers and that serves models over a wire API azazel speaks, sorted by id.
/// models.dev routes each model through one AI SDK package at one address (the provider's,
/// unless the model names its own); the row takes the route most of the provider's models take,
/// and [`PACKAGE_WIRES`] decides between routes that as many take.
///
/// A key variable models.dev gives to several providers, or that a hand-written row reads, is
/// left out, so that one key never makes several providers usable at once (a model name would
/// then match in each); a row left without one reads `<ID>_API_KEY`.
fn generated_rows(source: &Value, hand_written: &[(&str, &str, &[&str])]) -> Vec<GeneratedRow> {
    let Some(providers) = source.as_object() else {
        return Vec::new();
    };
    let claimed = |id: &str| {
        hand_written
            .iter()
            .any(|(own, name, _)| *own == id || *name == id)
            || CLOUD_SOURCES.iter().any(|(name, _)| *name == id)
    };
    let mut rows: Vec<GeneratedRow> = providers
        .iter()
        .filter(|(id, _)| !claimed(id) && plain_id(id))
        .filter_map(|(id, provider)| generated_row(id, provider))
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));

    let readers = |name: &str| {
        let hand = hand_written
            .iter()
            .filter(|(_, _, env)| env.contains(&name))
            .count();
        hand + rows
            .iter()
            .filter(|row| row.env.iter().any(|own| own == name))
            .count()
    };
    let own: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let mut env: Vec<String> = row
                .env
                .iter()
                .filter(|name| readers(name) == 1)
                .cloned()
                .collect();
            if env.is_empty() {
                env.push(format!(
                    "{}_API_KEY",
                    row.id.to_ascii_uppercase().replace('-', "_")
                ));
            }
            env
        })
        .collect();
    for (row, env) in rows.iter_mut().zip(own) {
        row.env = env;
    }
    rows
}

fn generated_row(id: &str, provider: &Value) -> Option<GeneratedRow> {
    let text = |value: &Value, name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let package = text(provider, "npm");
    let address = text(provider, "api");
    // (preference, address) -> the keys of the models taking that route.
    let mut routes: std::collections::BTreeMap<(usize, String), Vec<String>> = Default::default();
    for (key, model) in provider.get("models").and_then(Value::as_object)? {
        if model_info(id, model).is_none() {
            continue;
        }
        let own = model.get("provider").unwrap_or(&Value::Null);
        let (Some(package), Some(address)) = (
            text(own, "npm").or_else(|| package.clone()),
            text(own, "api").or_else(|| address.clone()),
        ) else {
            continue;
        };
        let Some(preference) = PACKAGE_WIRES.iter().position(|(name, _)| *name == package) else {
            continue;
        };
        if !(address.starts_with("https://") || address.starts_with("http://") || address.starts_with("${")) {
            continue;
        }
        let address = address.trim_end_matches('/');
        // azazel adds `/v1/messages` to an Anthropic address itself.
        let base_url = match PACKAGE_WIRES[preference].1 {
            WireApi::AnthropicMessages => match address.strip_suffix("/v1") {
                Some(base) => base,
                None => continue,
            },
            _ => address,
        };
        routes
            .entry((preference, base_url.to_string()))
            .or_default()
            .push(key.clone());
    }
    // The first of the most taken routes, in preference order.
    let mut best: Option<(&(usize, String), &Vec<String>)> = None;
    for route in &routes {
        if best.is_none_or(|(_, models)| route.1.len() > models.len()) {
            best = Some(route);
        }
    }
    let ((preference, base_url), models) = best?;
    let api = PACKAGE_WIRES[*preference].1;
    let base_url = base_url.clone();
    let host = base_url.split_once("://").map_or("", |(_, rest)| rest);
    let keyless = ["localhost", "127.0.0.1"].iter().any(|local| {
        host.strip_prefix(local)
            .is_some_and(|rest| rest.starts_with([':', '/']))
    });
    // A variable the address names is part of where the provider is, not its key.
    let env: Vec<String> = provider
        .get("env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
        .filter(|name| !base_url.contains(&format!("${{{name}")))
        .map(str::to_string)
        .collect();
    Some(GeneratedRow {
        id: id.to_string(),
        name: text(provider, "name").unwrap_or_else(|| id.to_string()),
        api,
        base_url,
        env,
        keyless,
        models: models.clone(),
    })
}

/// Whether `id` can stand as a provider id: in `provider/model`, a `[providers.<id>]` table and
/// a file name alike.
fn plain_id(id: &str) -> bool {
    id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `crates/azazel-ai/src/providers/generated.rs` holding `rows`.
fn generated_source(rows: &[GeneratedRow]) -> String {
    let mut wires: Vec<WireApi> = rows.iter().map(|row| row.api).collect();
    wires.sort_by_key(|api| format!("{api:?}"));
    wires.dedup();
    let mut out = String::from(
        "//! The providers models.dev lists that no hand-written row in `table.rs` covers, one row each,\n\
         //! sorted by id. A row there with the same id, or taking its models from the same models.dev\n\
         //! provider, replaces the one here.\n\
         //!\n\
         //! Written by `cargo xtask catalog` from <https://models.dev/api.json>: do not edit it by hand.\n\
         \n\
         use crate::providers::table::{Preset, row};\n",
    );
    if !wires.is_empty() {
        let names: Vec<String> = wires.iter().map(|api| format!("{api:?}")).collect();
        out.push_str(&format!("use crate::types::WireApi::{{{}}};\n", names.join(", ")));
    }
    out.push_str("\n#[rustfmt::skip]\npub(super) static GENERATED: &[Preset] = &[\n");
    for row in rows {
        let env: Vec<String> = row.env.iter().map(|name| format!("{name:?}")).collect();
        out.push_str(&format!(
            "    row({:?}, {:?}, {:?}, {:?}, &[{}]){},\n",
            row.id,
            row.name,
            row.api,
            row.base_url,
            env.join(", "),
            if row.keyless { ".keyless()" } else { "" }
        ));
    }
    out.push_str("];\n");
    out
}

/// `page` with the lines between [`TABLE_START`] and [`TABLE_END`] replaced by the table of
/// `rows`.
fn with_generated_table(page: &str, rows: &[GeneratedRow]) -> Result<String> {
    let (Some(start), Some(end)) = (page.find(TABLE_START), page.find(TABLE_END)) else {
        bail!("the page has no `{TABLE_START}` … `{TABLE_END}` lines for the generated providers");
    };
    let mut table =
        String::from("\n\n| Id | Name | Wire API | Base URL | Key variable |\n|---|---|---|---|---|\n");
    for row in rows {
        let mut keys: Vec<String> = row.env.iter().map(|name| format!("`{name}`")).collect();
        if keys.is_empty() {
            keys.push("none".to_string());
        }
        let optional = if row.keyless { " (optional)" } else { "" };
        table.push_str(&format!(
            "| `{}` | {} | `{}` | `{}` | {}{optional} |\n",
            row.id,
            row.name.replace('|', "\\|"),
            row.api.as_str(),
            row.base_url,
            keys.join(", ")
        ));
    }
    table.push('\n');
    let head = &page[..start + TABLE_START.len()];
    Ok(format!("{head}{table}{}", &page[end..]))
}

fn model_info(provider: &str, entry: &Value) -> Option<ModelInfo> {
    let id = entry.get("id")?.as_str()?;
    if !entry.get("tool_call").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let modality = |direction: &str, kind: &str| {
        entry
            .pointer(&format!("/modalities/{direction}"))
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some(kind)))
    };
    if !modality("output", "text") {
        return None;
    }
    let limit = |name: &str| {
        entry
            .pointer(&format!("/limit/{name}"))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
    };
    let unknown = ModelInfo::unknown(provider, id);
    let rate = |name: &str| entry.pointer(&format!("/cost/{name}")).and_then(Value::as_f64);
    let cost = match (rate("input"), rate("output")) {
        (Some(input), Some(output)) => Some(Cost {
            input,
            output,
            cache_read: rate("cache_read").unwrap_or(0.0),
            cache_write: rate("cache_write").unwrap_or(0.0),
        }),
        _ => None,
    };
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && *name != id)
        .map(str::to_string);
    // The efforts the model takes, in the order of the levels; `none` is `off`, which every
    // model has, and a toggle or a token budget names no levels.
    let mut thinking_levels: Vec<ThinkingLevel> = entry
        .get("reasoning_options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|option| option.get("type").and_then(Value::as_str) == Some("effort"))
        .flat_map(|option| {
            option
                .get("values")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|value| value.as_str()?.parse::<ThinkingLevel>().ok())
        .filter(|level| *level != ThinkingLevel::Off)
        .collect();
    thinking_levels.sort();
    thinking_levels.dedup();
    Some(ModelInfo {
        name,
        thinking_levels,
        context_window: limit("context").unwrap_or(unknown.context_window),
        max_output: limit("output")
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(unknown.max_output),
        vision: modality("input", "image"),
        reasoning: entry.get("reasoning").and_then(Value::as_bool).unwrap_or(false),
        cost,
        ..unknown
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    #[test]
    fn a_feature_is_in_the_default_build_when_default_reaches_it() {
        let metadata = json!({ "packages": [
            { "name": "other", "features": { "default": ["remote"], "remote": [] } },
            { "name": "azazel-cli", "features": {
                "default": ["lua", "screen"],
                "lua": ["dep:azazel-lua"],
                "screen": ["browser", "azazel-browser/screen"],
                "browser": ["dep:azazel-browser"],
                "remote": ["dep:azazel-remote", "detach"],
                "detach": [],
            } },
        ] });
        assert!(default_build_has(&metadata, "azazel-cli", "lua"));
        assert!(
            default_build_has(&metadata, "azazel-cli", "browser"),
            "through `screen`"
        );
        assert!(!default_build_has(&metadata, "azazel-cli", "remote"));
        assert!(!default_build_has(&metadata, "azazel-cli", "detach"));
        assert!(!default_build_has(&metadata, "missing", "lua"));
    }

    #[test]
    fn a_model_takes_the_efforts_its_entry_lists() {
        let entry = |options: Value| {
            json!({
                "id": "m", "tool_call": true, "reasoning": true,
                "modalities": {"input": ["text"], "output": ["text"]},
                "reasoning_options": options
            })
        };
        let levels = |options: Value| model_info("p", &entry(options)).unwrap().thinking_levels;
        use ThinkingLevel::{High, Low, Max, Medium, Xhigh};
        assert_eq!(
            levels(json!([{"type": "effort", "values": ["max", "low", "medium", "high", "xhigh"]}])),
            [Low, Medium, High, Xhigh, Max]
        );
        // `none` is off, which every model has; unknown words are left out.
        assert_eq!(
            levels(json!([{"type": "effort", "values": ["none", "low", "high", "default"]}])),
            [Low, High]
        );
        // A toggle or a token budget names no levels.
        assert!(levels(json!([{"type": "toggle"}, {"type": "budget_tokens", "min": 1024}])).is_empty());
        assert!(levels(Value::Null).is_empty());
    }

    /// A tool-capable text model, routed through `route` (`{npm, api}`) when given.
    fn model(id: &str, route: Option<Value>) -> Value {
        let mut model = json!({
            "id": id,
            "tool_call": true,
            "modalities": { "input": ["text"], "output": ["text"] },
            "limit": { "context": 200000, "output": 32000 },
        });
        if let Some(route) = route {
            model["provider"] = route;
        }
        model
    }

    fn provider(npm: &str, api: Option<&str>, env: &[&str], models: &[Value]) -> Value {
        let models: serde_json::Map<String, Value> = models
            .iter()
            .map(|m| (m["id"].as_str().unwrap().to_string(), m.clone()))
            .collect();
        json!({ "name": "Display Name", "npm": npm, "api": api, "env": env, "models": models })
    }

    const CHAT: &str = "@ai-sdk/openai-compatible";

    #[test]
    fn a_provider_takes_the_route_most_of_its_models_take() {
        let messages = json!({ "npm": "@ai-sdk/anthropic", "api": "https://gw.example/anthropic/v1" });
        let source = json!({
            "mixed": provider(CHAT, Some("https://gw.example/v1"), &["MIXED_KEY"], &[
                model("claude-a", Some(messages.clone())),
                model("claude-b", Some(messages.clone())),
                model("open-c", None),
            ]),
            // As many on each route: messages before chat.
            "tied": provider(CHAT, Some("https://tied.example/v1/"), &["TIED_KEY"], &[
                model("claude-a", Some(messages)),
                model("open-c", None),
            ]),
            "plain": provider(CHAT, Some("https://plain.example/v1/"), &["PLAIN_KEY"], &[model("m", None)]),
        });
        let rows = generated_rows(&source, &[]);
        let row = |id: &str| rows.iter().find(|row| row.id == id).unwrap();

        let mixed = row("mixed");
        assert_eq!(mixed.api, WireApi::AnthropicMessages);
        // azazel adds `/v1/messages` itself.
        assert_eq!(mixed.base_url, "https://gw.example/anthropic");
        assert_eq!(mixed.models, ["claude-a", "claude-b"]);
        assert_eq!(mixed.name, "Display Name");

        assert_eq!(row("tied").api, WireApi::AnthropicMessages);
        let plain = row("plain");
        assert_eq!(
            (plain.api, plain.base_url.as_str()),
            (WireApi::OpenAiChat, "https://plain.example/v1")
        );
    }

    #[test]
    fn providers_azazel_cannot_reach_or_already_has_get_no_row() {
        let source = json!({
            "vendor-sdk": provider("@vendor/ai-sdk-provider", Some("https://v.example"), &["V_KEY"], &[model("m", None)]),
            "no-address": provider(CHAT, None, &["N_KEY"], &[model("m", None)]),
            "covered": provider(CHAT, Some("https://c.example/v1"), &["C_KEY"], &[model("m", None)]),
            "renamed-here": provider(CHAT, Some("https://r.example/v1"), &["R_KEY"], &[model("m", None)]),
            "amazon-bedrock": provider(CHAT, Some("https://b.example/v1"), &["B_KEY"], &[model("m", None)]),
            "no-tools": provider(CHAT, Some("https://t.example/v1"), &["T_KEY"], &[json!({
                "id": "embed", "tool_call": false, "modalities": { "input": ["text"], "output": ["text"] }
            })]),
            "Odd.Id": provider(CHAT, Some("https://o.example/v1"), &["O_KEY"], &[model("m", None)]),
        });
        let hand: [(&str, &str, &[&str]); 2] =
            [("covered", "covered", &[]), ("renamed", "renamed-here", &[])];
        assert_eq!(generated_rows(&source, &hand), []);
    }

    #[test]
    fn every_row_reads_a_key_variable_of_its_own() {
        let source = json!({
            "cloud": provider(CHAT, Some("https://${CLOUD_ACCOUNT}.cloud.example/v1"), &["CLOUD_ACCOUNT", "CLOUD_TOKEN"], &[model("m", None)]),
            "east": provider(CHAT, Some("https://east.example/v1"), &["SHARED_KEY"], &[model("m", None)]),
            "west": provider(CHAT, Some("https://west.example/v1"), &["SHARED_KEY", "WEST_KEY"], &[model("m", None)]),
            "taken": provider(CHAT, Some("https://taken.example/v1"), &["HAND_KEY"], &[model("m", None)]),
            "local-box": provider(CHAT, Some("http://localhost:8080/v1"), &[], &[model("m", None)]),
        });
        let hand: [(&str, &str, &[&str]); 1] = [("hand", "", &["HAND_KEY"])];
        let rows = generated_rows(&source, &hand);
        let env = |id: &str| {
            let row = rows.iter().find(|row| row.id == id).unwrap();
            (row.env.clone(), row.keyless)
        };
        // A variable the address names says where the provider is, not what its key is.
        assert_eq!(env("cloud"), (vec!["CLOUD_TOKEN".to_string()], false));
        assert_eq!(env("east"), (vec!["EAST_API_KEY".to_string()], false));
        assert_eq!(env("west"), (vec!["WEST_KEY".to_string()], false));
        assert_eq!(env("taken"), (vec!["TAKEN_API_KEY".to_string()], false));
        assert_eq!(env("local-box"), (vec!["LOCAL_BOX_API_KEY".to_string()], true));
    }

    #[test]
    fn the_generated_file_and_table_hold_every_row() {
        let rows = vec![
            GeneratedRow {
                id: "acme".into(),
                name: "Acme | Co".into(),
                api: WireApi::AnthropicMessages,
                base_url: "https://acme.example".into(),
                env: vec!["ACME_KEY".into()],
                keyless: false,
                models: vec![],
            },
            GeneratedRow {
                id: "box".into(),
                name: "Box".into(),
                api: WireApi::OpenAiChat,
                base_url: "http://localhost:1/v1".into(),
                env: vec!["BOX_API_KEY".into()],
                keyless: true,
                models: vec![],
            },
        ];
        let source = generated_source(&rows);
        assert!(
            source.contains("use crate::types::WireApi::{AnthropicMessages, OpenAiChat};"),
            "{source}"
        );
        assert!(
            source.contains(
                r#"row("acme", "Acme | Co", AnthropicMessages, "https://acme.example", &["ACME_KEY"]),"#
            ),
            "{source}"
        );
        assert!(
            source.contains(
                r#"row("box", "Box", OpenAiChat, "http://localhost:1/v1", &["BOX_API_KEY"]).keyless(),"#
            ),
            "{source}"
        );

        let page = format!("# Page\n\n{TABLE_START}\n| old |\n{TABLE_END}\n\n## Next\n");
        let written = with_generated_table(&page, &rows).unwrap();
        assert!(written.contains(
            "| `acme` | Acme \\| Co | `anthropic-messages` | `https://acme.example` | `ACME_KEY` |"
        ));
        assert!(written.contains(
            "| `box` | Box | `openai-chat` | `http://localhost:1/v1` | `BOX_API_KEY` (optional) |"
        ));
        assert!(!written.contains("| old |") && written.ends_with(&format!("{TABLE_END}\n\n## Next\n")));
        // Writing it again changes nothing.
        assert_eq!(with_generated_table(&written, &rows).unwrap(), written);
        assert!(with_generated_table("# No markers\n", &rows).is_err());
    }
}
