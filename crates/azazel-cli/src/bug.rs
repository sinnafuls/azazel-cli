//! Bug reports (`/bug`, `azazel bug`): one zip under `~/.azazel/reports/` with what a
//! maintainer asks for first, and the offer to open an issue with its summary.
//!
//! The zip holds `report.md` (what the user said, the version, the system), `doctor.txt`
//! (`azazel doctor`), `config.toml` (the settings in effect, secrets removed), `log.txt` (the
//! end of the log) and, when asked for, `transcript.html` (a session, secrets hidden). It stays
//! on the machine: only the summary goes into an issue, and only after a yes.

use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use azazel_core::{Config, LoadOptions, Paths, SessionStore};
use azazel_tui::text::redact;

use crate::app::App;
use crate::doctor::{self, Severity};
use crate::export::{Format, has_messages, render};
use crate::print::{EXIT_FAILED, EXIT_OK};
use crate::share;

/// How much of the end of the log goes into a report.
const LOG_TAIL_BYTES: u64 = 256 * 1024;
const REMOVED: &str = "[removed]";
/// How long `doctor` gets, as when it is run by name.
const DOCTOR_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Keys whose text is a secret wherever they are in the configuration.
const SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
    "credential",
];
/// Tables whose every value may be one: headers sent to a provider, the environment given to
/// a program.
const SECRET_TABLES: &[&str] = &["headers", "oauth_headers", "env"];

fn names_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    // `api_key_env` names a variable, `api_key_command` a program and `secret_paths` files:
    // none is a secret itself.
    let names_one = ["_env", "_command", "_paths"]
        .iter()
        .any(|end| key.ends_with(end));
    !names_one && SECRET_KEYS.iter().any(|word| key.contains(word))
}

/// Replaces the text of every setting that holds a secret. `all` is whether everything under
/// `value` counts as one.
fn remove_secrets(value: &mut toml::Value, all: bool) {
    match value {
        toml::Value::String(text) if all => *text = REMOVED.to_string(),
        toml::Value::Array(items) => items.iter_mut().for_each(|item| remove_secrets(item, all)),
        toml::Value::Table(table) => {
            for (key, value) in table.iter_mut() {
                let secret = all || names_secret(key) || SECRET_TABLES.contains(&key.as_str());
                remove_secrets(value, secret);
            }
        }
        _ => {}
    }
}

/// The settings in effect as TOML, without the secrets in them: by the name of the setting,
/// and then by what the text looks like.
pub fn config_without_secrets(config: &Config) -> String {
    let mut value = match toml::Value::try_from(config) {
        Ok(value) => value,
        Err(e) => return format!("# the settings could not be written out: {e}\n"),
    };
    remove_secrets(&mut value, false);
    match toml::to_string(&value) {
        Ok(text) => redact(&text).into_owned(),
        Err(e) => format!("# the settings could not be written out: {e}\n"),
    }
}

/// The last lines of the log, secrets hidden; empty when there is no log.
fn log_tail(file: &Path) -> String {
    let Ok(mut log) = std::fs::File::open(file) else {
        return String::new();
    };
    let length = log.metadata().map_or(0, |meta| meta.len());
    let from = length.saturating_sub(LOG_TAIL_BYTES);
    let mut bytes = Vec::new();
    if log.seek(SeekFrom::Start(from)).is_err() || log.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    // From the first whole line on.
    let text = match (from, text.split_once('\n')) {
        (0, _) | (_, None) => &*text,
        (_, Some((_, rest))) => rest,
    };
    redact(text).into_owned()
}

/// The system's own name for its version (`Microsoft Windows [Version 10.0.26100.1]`,
/// `Linux 6.8.0`); empty when it does not say.
async fn os_version() -> String {
    let mut command = if cfg!(windows) {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/d", "/c", "ver"]);
        command
    } else {
        let mut command = tokio::process::Command::new("uname");
        command.arg("-sr");
        command
    };
    command.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), command.output());
    match run.await {
        Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// The terminal as its environment describes it.
fn terminal() -> String {
    let mut parts = Vec::new();
    for name in ["TERM_PROGRAM", "TERM", "COLORTERM", "LANG"] {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            parts.push(format!("{name}={value}"));
        }
    }
    if std::env::var_os("WT_SESSION").is_some() {
        parts.push("Windows Terminal".to_string());
    }
    if parts.is_empty() {
        "not said".to_string()
    } else {
        parts.join(", ")
    }
}

/// A bug report before it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// What an issue would say: the description, the version, the system, what `doctor`
    /// found wrong.
    pub summary: String,
    /// The files of the zip, each with its name.
    pub files: Vec<(&'static str, String)>,
}

/// One line of what each file of a report is.
fn about(name: &str) -> &'static str {
    match name {
        "report.md" => "what you said, the version, the system",
        "doctor.txt" => "what `azazel doctor` prints",
        "config.toml" => "the settings in effect, secrets removed",
        "log.txt" => "the end of azazel's log, secrets hidden",
        "transcript.html" => "the session, secrets hidden",
        _ => "",
    }
}

impl Report {
    /// Collects a report about this installation and the project of `paths`. `config` is the
    /// configuration in effect; `transcript` a session to include.
    pub async fn gather(
        paths: &Paths,
        trusted: bool,
        config: &Config,
        description: &str,
        transcript: Option<&SessionStore>,
    ) -> Report {
        let examined = tokio::time::timeout(DOCTOR_LIMIT, doctor::examine(paths, trusted)).await;
        let (found, doctor_text) = match examined {
            Ok(found) => {
                let text = redact(&found.render()).into_owned();
                (found, text)
            }
            Err(_) => (
                doctor::Report::default(),
                format!("azazel doctor did not finish in {} s\n", DOCTOR_LIMIT.as_secs()),
            ),
        };
        let description = description.trim();
        let mut summary = String::new();
        summary.push_str(if description.is_empty() {
            "(no description given)"
        } else {
            description
        });
        let system = os_version().await;
        summary.push_str(&format!(
            "\n\n- azazel: {}\n- system: {} {}{}\n- terminal: {}\n- model: {}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            if system.is_empty() {
                String::new()
            } else {
                format!(" ({system})")
            },
            terminal(),
            config.roles.get("default").map_or_else(
                || "none configured".to_string(),
                |chain| chain
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
        let wrong: Vec<String> = found
            .findings
            .iter()
            .filter(|finding| finding.severity != Severity::Ok)
            .map(|finding| {
                let mark = if finding.severity == Severity::Error {
                    "FAIL"
                } else {
                    "warn"
                };
                format!("- {mark}: {}", finding.text)
            })
            .collect();
        if wrong.is_empty() {
            summary.push_str("\n`azazel doctor` found nothing wrong.\n");
        } else {
            summary.push_str(&format!("\n`azazel doctor` found:\n{}\n", wrong.join("\n")));
        }
        let summary = redact(&summary).into_owned();

        let mut files = vec![
            ("report.md", format!("# azazel bug report\n\n{summary}")),
            ("doctor.txt", doctor_text),
            ("config.toml", config_without_secrets(config)),
        ];
        let log = log_tail(&App::log_file(paths));
        if !log.is_empty() {
            files.push(("log.txt", log));
        }
        if let Some(session) = transcript.filter(|session| has_messages(session)) {
            files.push(("transcript.html", render(session, Format::Html, true)));
        }
        Report { summary, files }
    }

    /// Writes the report as `azazel-report-<time>.zip` in `dir` and returns the file.
    pub fn write(&self, dir: &Path) -> Result<PathBuf, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
        let mut file = dir.join(format!("azazel-report-{stamp}.zip"));
        // A second report within the second gets a name of its own, one that sorts after the
        // first.
        let mut again = 1;
        while file.exists() {
            again += 1;
            file = dir.join(format!("azazel-report-{stamp}_{again}.zip"));
        }
        let failed = |e: &dyn std::fmt::Display| format!("could not write {}: {e}", file.display());
        let out = std::fs::File::create(&file).map_err(|e| failed(&e))?;
        let mut zip = zip::ZipWriter::new(out);
        let options =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, text) in &self.files {
            zip.start_file(*name, options).map_err(|e| failed(&e))?;
            zip.write_all(text.as_bytes()).map_err(|e| failed(&e))?;
        }
        zip.finish().map_err(|e| failed(&e))?;
        Ok(file)
    }

    /// What was written, for the user to read: the file, then each thing in it.
    pub fn written(&self, file: &Path) -> String {
        let mut out = format!("bug report written to {}\n", file.display());
        for (name, text) in &self.files {
            out.push_str(&format!(
                "  {name:<16} {} ({})\n",
                about(name),
                share::size(text.len())
            ));
        }
        out.push_str("It stays on this machine. Read it before you send it to anyone.");
        out
    }

    /// The title of the issue this report would open.
    pub fn issue_title(&self) -> String {
        let first = self.summary.lines().next().unwrap_or_default().trim();
        if first.is_empty() || first.starts_with('(') {
            return format!(
                "Bug report: azazel {} on {}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS
            );
        }
        match first.char_indices().nth(80) {
            Some((at, _)) => format!("{}…", &first[..at]),
            None => first.to_string(),
        }
    }

    /// The text of the issue: the summary, and that the rest stayed behind.
    pub fn issue_body(&self) -> String {
        format!(
            "{}\nThe full report (`azazel doctor`, the settings without secrets, the end of the log) is in a zip \
             on my machine; I can attach it.\n",
            self.summary
        )
    }
}

fn yes(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// What `azazel bug` was asked for.
pub struct Args<'a> {
    pub description: &'a str,
    /// Include the latest session of the project.
    pub transcript: bool,
    /// Include this session instead: a path to its file, or its id.
    pub session: Option<&'a str>,
    /// Open the issue without asking.
    pub issue: bool,
}

/// `azazel bug [DESCRIPTION] [--transcript] [--session ID] [--issue]`: writes the report and
/// says where it is. At a terminal it asks about the transcript and about opening an issue;
/// elsewhere it does what the flags say and nothing more.
pub async fn run(paths: &Paths, trusted: bool, args: Args<'_>) -> u8 {
    let asks = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let options = LoadOptions {
        trusted,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    // A configuration that does not load is what `doctor.txt` then says.
    let config = Config::load(paths, &options).map_or_else(|_| Config::default(), |loaded| loaded.config);

    let wanted = args.transcript || args.session.is_some();
    let picked = crate::sessions::pick(paths, args.session)
        .and_then(|summary| SessionStore::open(&summary.path).map_err(|e| e.to_string()));
    let session = match picked {
        Ok(session) if wanted => Some(session),
        Ok(session)
            if asks
                && has_messages(&session)
                && yes(&format!(
                    "Include the transcript of this project's latest session ({}), secrets hidden?",
                    session.name().unwrap_or_else(|| session.id())
                )) =>
        {
            Some(session)
        }
        Ok(_) => None,
        Err(why) if wanted => {
            eprintln!("azazel: {why}");
            return EXIT_FAILED;
        }
        Err(_) => None,
    };

    let report = Report::gather(paths, trusted, &config, args.description, session.as_ref()).await;
    let file = match report.write(&paths.reports_dir()) {
        Ok(file) => file,
        Err(why) => {
            eprintln!("azazel: {why}");
            return EXIT_FAILED;
        }
    };
    println!("{}", report.written(&file));

    let gh = share::gh();
    let open = match (&gh, args.issue) {
        (Err(why), true) => {
            eprintln!("azazel: no issue was opened: {why}");
            return EXIT_FAILED;
        }
        (Ok(_), true) => true,
        (Ok(_), false) if asks => {
            println!(
                "\nAn issue on {} would say, in public:\n\nTitle: {}\n\n{}",
                share::REPO,
                report.issue_title(),
                report.issue_body()
            );
            yes("Open it?")
        }
        _ => false,
    };
    if !open {
        if !args.issue && !asks {
            println!(
                "To report it: azazel bug --issue \"what went wrong\", or open an issue at \
                 https://github.com/{}/issues",
                share::REPO
            );
        }
        return EXIT_OK;
    }
    let Ok(gh) = gh else { return EXIT_OK };
    match share::open_issue(&gh, share::REPO, &report.issue_title(), &report.issue_body()).await {
        Ok(link) => {
            println!("issue opened: {link}");
            EXIT_OK
        }
        Err(why) => {
            eprintln!("azazel: no issue was opened: {why}");
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn unzip(file: &Path) -> BTreeMap<String, String> {
        let mut archive = zip::ZipArchive::new(std::fs::File::open(file).unwrap()).unwrap();
        let mut out = BTreeMap::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let mut text = String::new();
            entry.read_to_string(&mut text).unwrap();
            out.insert(entry.name().unwrap().to_string(), text);
        }
        out
    }

    #[test]
    fn settings_are_written_without_their_secrets() {
        let config: Config = toml::from_str(
            r#"
            [roles]
            default = ["anthropic/claude-opus"]
            [providers.corp]
            api = "openai-chat"
            base_url = "https://llm.corp.example/v1"
            api_key = "plain-text-key-of-the-company"
            api_key_env = "CORP_KEY"
            api_key_command = "pass show corp"
            [providers.corp.headers]
            x-team = "team-seven"
            [telemetry]
            exporter = "otlp"
            api_key_env = "TRACE_KEY"
            [permissions]
            secret_paths = ["vault/**"]
            deny = ["shell(echo ghp_0123456789abcdefghijklmnopqrstuvwxyzAB)"]
            "#,
        )
        .unwrap();
        let text = config_without_secrets(&config);
        let read: toml::Table = toml::from_str(&text).unwrap();
        let corp = &read["providers"]["corp"];
        assert_eq!(corp["api_key"].as_str(), Some(REMOVED));
        assert_eq!(corp["headers"]["x-team"].as_str(), Some(REMOVED));
        // What names a variable, a program or a path is not a secret, and stays.
        assert_eq!(corp["api_key_env"].as_str(), Some("CORP_KEY"));
        assert_eq!(corp["api_key_command"].as_str(), Some("pass show corp"));
        assert_eq!(corp["base_url"].as_str(), Some("https://llm.corp.example/v1"));
        assert_eq!(read["telemetry"]["api_key_env"].as_str(), Some("TRACE_KEY"));
        assert_eq!(read["permissions"]["secret_paths"][0].as_str(), Some("vault/**"));
        assert_eq!(
            read["roles"]["default"][0].as_str(),
            Some("anthropic/claude-opus")
        );
        // A secret under a name that does not give it away is caught by what it looks like.
        assert!(!text.contains("ghp_0123456789"), "{text}");
        assert!(!text.contains("plain-text-key") && !text.contains("team-seven"));
    }

    #[test]
    fn the_log_tail_starts_on_a_whole_line_and_hides_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("azazel.log");
        assert_eq!(log_tail(&log), "");
        std::fs::write(
            &log,
            "one\ntwo with sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345\n",
        )
        .unwrap();
        let tail = log_tail(&log);
        assert!(
            tail.starts_with("one\ntwo with ") && !tail.contains("abcdefghij"),
            "{tail}"
        );

        let line = format!("{}\n", "x".repeat(99));
        let lines = usize::try_from(LOG_TAIL_BYTES).unwrap() / 100 + 50;
        std::fs::write(&log, format!("first line\n{}", line.repeat(lines))).unwrap();
        let tail = log_tail(&log);
        assert!(tail.len() <= usize::try_from(LOG_TAIL_BYTES).unwrap());
        assert!(!tail.contains("first line"));
        assert!(tail.lines().all(|line| line.len() == 99), "a line was cut");
    }

    #[tokio::test]
    async fn a_report_is_a_zip_of_what_was_gathered() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let home = dir.path().join("home");
        let paths = Paths::discover(&project, Some(&home.to_string_lossy()));
        std::fs::create_dir_all(paths.logs_dir()).unwrap();
        std::fs::write(App::log_file(&paths), "WARN something went wrong\n").unwrap();
        let session = SessionStore::create(&paths.sessions_dir(), &project, None).unwrap();
        session.append(azazel_core::EntryKind::Message {
            message: azazel_ai::Message::user("my DB_PASSWORD=hunter2hunter2 does not work"),
        });

        let config = Config::default();
        let report = Report::gather(
            &paths,
            false,
            &config,
            "The status line flickers\nwhen I resize",
            Some(&session),
        )
        .await;
        assert!(
            report
                .summary
                .starts_with("The status line flickers\nwhen I resize\n\n- azazel: ")
        );
        assert!(
            report
                .summary
                .contains(&format!("- system: {} ", std::env::consts::OS))
        );
        assert!(report.summary.contains("- model: none configured"));
        assert_eq!(report.issue_title(), "The status line flickers");
        assert!(report.issue_body().starts_with(&report.summary));

        let file = report.write(&paths.reports_dir()).unwrap();
        assert_eq!(file.parent().unwrap(), home.join("reports"));
        let again = report.write(&paths.reports_dir()).unwrap();
        assert!(file < again, "a second report is not listed after the first");
        let files = unzip(&file);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "config.toml",
                "doctor.txt",
                "log.txt",
                "report.md",
                "transcript.html"
            ]
        );
        assert!(files["report.md"].starts_with("# azazel bug report\n\nThe status line flickers"));
        assert!(files["doctor.txt"].starts_with(&format!("ok    azazel {}", env!("CARGO_PKG_VERSION"))));
        assert!(files["config.toml"].parse::<toml::Table>().is_ok());
        assert_eq!(files["log.txt"], "WARN something went wrong\n");
        assert!(files["transcript.html"].contains("does not work"));
        assert!(!files["transcript.html"].contains("hunter2"));
        let listed = report.written(&file);
        assert!(listed.starts_with(&format!("bug report written to {}\n", file.display())));
        assert!(listed.contains("transcript.html") && listed.contains("stays on this machine"));

        // Without a description, a log or a session the report is the three files that always
        // are, and its issue is named for the version.
        std::fs::remove_file(App::log_file(&paths)).unwrap();
        let bare = Report::gather(&paths, false, &config, "  ", None).await;
        let names: Vec<&str> = bare.files.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["report.md", "doctor.txt", "config.toml"]);
        assert!(bare.issue_title().starts_with("Bug report: azazel "));
    }
}
