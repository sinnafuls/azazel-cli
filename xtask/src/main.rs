//! Workspace maintenance tasks, run as `cargo xtask <task>`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use azazel_ai::catalog::{CatalogFile, Cost, ModelInfo};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
const MAX_BINARY_BYTES: u64 = 56 * 1024 * 1024;
const BENCH_RUNS: usize = 20;
const TASKS: &str = "catalog, bench-startup, dist";

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("catalog") => catalog(),
        Some("bench-startup") => bench_startup(),
        Some("dist") => dist(),
        Some(other) => Err(anyhow::anyhow!("unknown task `{other}`; tasks: {TASKS}")),
        None => Err(anyhow::anyhow!("usage: cargo xtask <task>; tasks: {TASKS}")),
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

/// Builds the release binary (for `--target <triple>`, else this machine) and packs it with the
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
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "--release", "-p", "azazel-cli"]);
    if let Some(triple) = &target {
        build.args(["--target", triple]);
    }
    let built = build.current_dir(&root).status().context("could not run cargo")?;
    if !built.success() {
        bail!("the release build failed");
    }
    let (os, arch, out_dir) = match &target {
        Some(triple) => {
            let (os, arch) = platform_of(triple)?;
            (os, arch, root.join("target").join(triple).join("release"))
        }
        None => (
            std::env::consts::OS,
            std::env::consts::ARCH.to_string(),
            root.join("target/release"),
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

/// Builds the release binary and fails when it starts too slowly or is too large: the median of
/// 20 runs of `azazel --version` and of `azazel -p hi` against the scripted provider, and the
/// size of the binary. The binary is built with the scripted provider compiled in, which makes it
/// a little larger than the one that ships.
fn bench_startup() -> Result<()> {
    let root = workspace_root();
    let built = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
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
    let binary = root
        .join("target")
        .join("release")
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
    let size_file = root.join("target").join("bench-size.txt");
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

/// Regenerates `crates/azazel-ai/catalog/models.json` from models.dev, keeping the tool-capable text
/// models of the built-in providers and of the cloud providers.
fn catalog() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let source: Value = runtime.block_on(async {
        let response = reqwest::Client::new()
            .get(CATALOG_SOURCE)
            .send()
            .await?
            .error_for_status()?;
        response.json::<Value>().await
    })?;

    // Each built-in provider says what it is called in the source; several may share one.
    let sources = azazel_ai::providers::presets::catalog_sources()
        .into_iter()
        .chain(CLOUD_SOURCES);
    let mut models: Vec<ModelInfo> = Vec::new();
    for (source_name, provider) in sources {
        let Some(entries) = source
            .get(source_name)
            .and_then(|p| p.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for model in entries.values().filter_map(|entry| model_info(provider, entry)) {
            // Two sources of one provider may both list a model; the first listing stands.
            if !models.iter().any(|known| known.key() == model.key()) {
                models.push(model);
            }
        }
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

    let path = workspace_root().join("crates/azazel-ai/catalog/models.json");
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {} models to {}", models.len(), path.display());
    Ok(())
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
    Some(ModelInfo {
        name,
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
