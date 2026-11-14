//! `/debug`: where azazel keeps its files, what state the UI and the session are in, and the
//! end of the log, in the full-screen viewer. It is what a bug report starts from, shown
//! instead of zipped.

use std::path::PathBuf;

use azazel_tui::term::Terminal;

use super::Tui;
use super::pager::{self, Source, Start};
use crate::app::App;

/// How many lines of the end of the log are shown.
const LOG_LINES: usize = 200;

/// Variables that say what terminal this is; their values are shown.
const TERMINAL_VARIABLES: &[&str] = &[
    "TERM",
    "TERM_PROGRAM",
    "COLORTERM",
    "WT_SESSION",
    "TMUX",
    "SSH_CONNECTION",
    "LANG",
    "LC_ALL",
];

/// What `/debug` shows, gathered.
pub(super) struct Report {
    /// `(what, where)`; a file that is not there is marked.
    pub paths: Vec<(&'static str, PathBuf)>,
    /// `(what, value)`.
    pub state: Vec<(&'static str, String)>,
    /// `(name, value)` of the variables that are set; the value of an `AZAZEL_` variable is
    /// left out, since it can hold a key.
    pub variables: Vec<(String, Option<String>)>,
    pub log_file: PathBuf,
    /// The end of the log, secrets hidden.
    pub log: String,
}

impl Report {
    /// The report as Markdown, for the viewer.
    pub(super) fn render(&self) -> String {
        let mut out = format!("# azazel {}\n\n## Paths\n\n", env!("CARGO_PKG_VERSION"));
        let widest = self.paths.iter().map(|(what, _)| what.len()).max().unwrap_or(0);
        out.push_str("```text\n");
        for (what, path) in &self.paths {
            let missing = if path.exists() { "" } else { "  (not there)" };
            out.push_str(&format!("{what:<widest$}  {}{missing}\n", path.display()));
        }
        out.push_str("```\n\n## State\n\n```text\n");
        let widest = self.state.iter().map(|(what, _)| what.len()).max().unwrap_or(0);
        for (what, value) in &self.state {
            out.push_str(&format!("{what:<widest$}  {value}\n"));
        }
        out.push_str("```\n\n## Environment\n\n```text\n");
        for (name, value) in &self.variables {
            match value {
                Some(value) => out.push_str(&format!("{name}={value}\n")),
                None => out.push_str(&format!("{name} is set\n")),
            }
        }
        if self.variables.is_empty() {
            out.push_str("none of the variables azazel reads is set\n");
        }
        out.push_str("```\n\n## Log\n\n");
        let lines: Vec<&str> = self.log.lines().collect();
        if lines.is_empty() {
            out.push_str(&format!("{} is empty or not there.\n", self.log_file.display()));
            return out;
        }
        let from = lines.len().saturating_sub(LOG_LINES);
        out.push_str(&format!(
            "The last {} lines of {}; `--debug` makes it say more.\n\n```text\n",
            lines.len() - from,
            self.log_file.display()
        ));
        for line in &lines[from..] {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("```\n");
        out
    }
}

/// The variables of `all` that `/debug` lists: the terminal's with their values, azazel's own
/// by name only.
pub(super) fn variables(all: impl Iterator<Item = (String, String)>) -> Vec<(String, Option<String>)> {
    let mut listed: Vec<(String, Option<String>)> = all
        .filter_map(|(name, value)| {
            if TERMINAL_VARIABLES.contains(&name.as_str()) {
                Some((name, Some(value)))
            } else if name.starts_with("AZAZEL_") {
                Some((name, None))
            } else {
                None
            }
        })
        .collect();
    listed.sort();
    listed
}

impl Tui {
    /// `/debug`: paths, state and the end of the log, full screen.
    pub(super) async fn debug_command(&mut self) {
        let report = self.debug_report();
        let sources = vec![Source::Markdown(report.render())];
        pager::show(self, "Debug", sources, Start::Top).await;
    }

    fn debug_report(&self) -> Report {
        let paths = &self.env.paths;
        let log_file = App::log_file(paths);
        let mut listed = vec![
            ("home", paths.home.clone()),
            ("settings", paths.home_config()),
            ("project", paths.project_root.clone()),
            ("project settings", paths.project_config()),
            ("local settings", paths.project_local_config()),
            ("key bindings", paths.keybindings_file()),
            ("sign-ins and keys", paths.auth_file()),
            ("trusted projects", paths.trust_file()),
            ("sessions", paths.sessions_dir()),
            ("session file", self.session.path()),
            ("cache", paths.cache_dir()),
            ("log", log_file.clone()),
        ];
        if let Ok(binary) = std::env::current_exe() {
            listed.push(("binary", binary));
        }

        let status = self.backend.status();
        let (columns, rows) = self.renderer.size();
        let yes_no = |on: bool| if on { "yes" } else { "no" }.to_string();
        let chain: Vec<String> = status.chain.iter().map(ToString::to_string).collect();
        let terminal = Terminal::detect(&|name| std::env::var(name).ok());
        let theme = self.theme_label(&self.theme_name);
        let state = vec![
            ("process", std::process::id().to_string()),
            (
                "session",
                match self.session.name() {
                    Some(name) => format!("{} ({name})", self.session.id()),
                    None => self.session.id(),
                },
            ),
            (
                "runs",
                self.backend
                    .place()
                    .unwrap_or_else(|| "in this process".to_string()),
            ),
            ("directory", self.backend.cwd().display().to_string()),
            ("model", self.model_label()),
            ("chain", chain.join(", ")),
            ("thinking", status.thinking.to_string()),
            ("mode", self.backend.mode().to_string()),
            ("working", yes_no(self.busy)),
            ("agents", self.backend.roster().len().to_string()),
            ("queued messages", self.queued.len().to_string()),
            ("stashed drafts", self.stashed.len().to_string()),
            ("terminal", format!("{terminal:?}, {columns}x{rows}")),
            ("tmux", yes_no(self.caps.tmux)),
            ("images", format!("{:?}", self.caps.images)),
            ("links", yes_no(self.caps.links)),
            ("colours", format!("{:?}", self.theme.depth())),
            ("theme", theme),
            ("symbols", self.theme.glyphs().level().to_string()),
            ("popups", yes_no(self.env.config.tui.popups)),
            ("enhanced keys", yes_no(self.guard.enhanced_keys)),
        ];
        Report {
            paths: listed,
            state,
            variables: variables(std::env::vars()),
            log: crate::bug::log_tail(&log_file),
            log_file,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_lists_paths_state_and_the_end_of_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_file = dir.path().join("azazel.log");
        let lines: Vec<String> = (1..=LOG_LINES + 50).map(|n| format!("line {n}")).collect();
        std::fs::write(&log_file, lines.join("\n")).unwrap();
        let report = Report {
            paths: vec![
                ("home", dir.path().to_path_buf()),
                ("settings", dir.path().join("config.toml")),
            ],
            state: vec![("model", "mock/lead".to_string()), ("working", "no".to_string())],
            variables: variables(
                [
                    ("TERM", "xterm-256color"),
                    ("AZAZEL_PROVIDERS__X__API_KEY", "sk-secret"),
                    ("PATH", "/bin"),
                ]
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.to_string())),
            ),
            log: crate::bug::log_tail(&log_file),
            log_file: log_file.clone(),
        };
        let text = report.render();
        // A file that is not there is said to be missing; one that is, is not.
        assert!(
            text.contains(&format!("home      {}\n", dir.path().display())),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "settings  {}  (not there)\n",
                dir.path().join("config.toml").display()
            )),
            "{text}"
        );
        assert!(text.contains("model    mock/lead\nworking  no\n"), "{text}");
        // A variable of azazel's own is named, and what it holds stays out.
        assert!(text.contains("AZAZEL_PROVIDERS__X__API_KEY is set\nTERM=xterm-256color\n"));
        assert!(!text.contains("sk-secret") && !text.contains("PATH"));
        // Only the end of the log is shown.
        assert!(text.contains(&format!("The last {LOG_LINES} lines of {}", log_file.display())));
        assert!(text.contains("\nline 51\n") && text.contains(&format!("line {}\n```", LOG_LINES + 50)));
        assert!(!text.contains("\nline 50\n"));

        let empty = Report {
            log: String::new(),
            ..report
        };
        assert!(empty.render().ends_with("is empty or not there.\n"));
    }
}
