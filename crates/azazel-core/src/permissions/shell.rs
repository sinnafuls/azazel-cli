//! Breaking a shell command line into its simple commands, and the guard that keeps agents from
//! pushing.

use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::{Node, Parser};

pub const PUSH_DENIED: &str = "agents never push. Ask the user to push (they can type \"!git push\").";

/// One command as the shell would execute it, without leading assignments or wrappers such as
/// `sudo` and `env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    /// The words as written, joined by single spaces; what permission rules match.
    pub text: String,
    /// The words with surrounding quotes removed.
    pub words: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellAnalysis {
    pub commands: Vec<SimpleCommand>,
    /// False when the command line could not be parsed reliably. Such a command is one opaque
    /// subject and is never auto-allowed by an allow rule.
    pub parsed: bool,
}

impl ShellAnalysis {
    fn opaque(command: &str) -> Self {
        ShellAnalysis {
            commands: vec![SimpleCommand {
                text: command.trim().to_string(),
                words: vec![command.trim().to_string()],
            }],
            parsed: false,
        }
    }

    /// The strings permission rules are matched against.
    pub fn subjects(&self) -> Vec<String> {
        self.commands.iter().map(|c| c.text.clone()).collect()
    }
}

/// Every simple command in `command`: through pipelines, lists, subshells, command substitutions
/// and scripts passed to `sh -c` or `eval`. `posix` is false for PowerShell, which is not parsed.
pub fn analyze(command: &str, posix: bool) -> ShellAnalysis {
    if !posix {
        return ShellAnalysis::opaque(command);
    }
    analyze_depth(command, 0).unwrap_or_else(|| ShellAnalysis::opaque(command))
}

const MAX_NESTING: usize = 3;

fn analyze_depth(command: &str, depth: usize) -> Option<ShellAnalysis> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_bash::LANGUAGE.into()).ok()?;
    let tree = parser.parse(command, None)?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut commands = Vec::new();
    collect(tree.root_node(), command.as_bytes(), depth, &mut commands)?;
    Some(ShellAnalysis {
        commands,
        parsed: true,
    })
}

/// `None` when a nested script could not be analysed.
fn collect(node: Node<'_>, source: &[u8], depth: usize, out: &mut Vec<SimpleCommand>) -> Option<()> {
    if node.kind() == "command" {
        let mut raw: Vec<String> = Vec::new();
        if let Some(name) = node.child_by_field_name("name") {
            raw.push(name.utf8_text(source).ok()?.to_string());
        }
        let mut cursor = node.walk();
        for argument in node.children_by_field_name("argument", &mut cursor) {
            raw.push(argument.utf8_text(source).ok()?.to_string());
        }
        let raw = strip_wrappers(raw);
        if !raw.is_empty() {
            let words: Vec<String> = raw.iter().map(|word| unquote(word)).collect();
            if let Some(script) = inline_script(&words) {
                if depth >= MAX_NESTING {
                    return None;
                }
                out.extend(analyze_depth(&script, depth + 1)?.commands);
            }
            out.push(SimpleCommand {
                text: raw.join(" "),
                words,
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, depth, out)?;
    }
    Some(())
}

fn unquote(word: &str) -> String {
    let inner = word
        .strip_prefix("$'")
        .and_then(|w| w.strip_suffix('\''))
        .or_else(|| {
            ['"', '\'']
                .iter()
                .find_map(|q| word.strip_prefix(*q).and_then(|w| w.strip_suffix(*q)))
        });
    inner.unwrap_or(word).to_string()
}

/// The program name without directory or `.exe`.
fn program(word: &str) -> &str {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    name.strip_suffix(".exe").unwrap_or(name)
}

/// Drops leading wrappers (`sudo`, `env`, `command`, `nohup`, `time`, `xargs`) with their options
/// and, for `env`, its assignments. A wrapper with nothing after it is kept as the command.
fn strip_wrappers(mut words: Vec<String>) -> Vec<String> {
    for _ in 0..8 {
        let Some(first) = words.first() else { break };
        let takes_value: &[&str] = match program(&unquote(first)) {
            "sudo" => &["-u", "-g", "-h", "-p", "-C", "-D", "-R", "-T", "-U"],
            "xargs" => &["-n", "-P", "-I", "-L", "-s", "-E", "-d", "-a"],
            "env" => &["-u", "-C", "-S"],
            "time" => &["-f", "-o"],
            "command" | "nohup" => &[],
            _ => break,
        };
        let is_env = program(&unquote(first)) == "env";
        let mut i = 1;
        while i < words.len() {
            let word = words[i].as_str();
            if word == "--" {
                i += 1;
                break;
            }
            if word.starts_with('-') {
                i += if takes_value.contains(&word) { 2 } else { 1 };
            } else if is_env && word.contains('=') {
                i += 1;
            } else {
                break;
            }
        }
        if i >= words.len() {
            break;
        }
        words.drain(..i);
    }
    words
}

/// The script text of `sh -c <script>` (any POSIX shell) or `eval <words>`.
fn inline_script(words: &[String]) -> Option<String> {
    let (first, rest) = words.split_first()?;
    match program(first) {
        "eval" if !rest.is_empty() => Some(rest.join(" ")),
        "bash" | "sh" | "zsh" | "dash" | "ksh" => {
            let flag = rest
                .iter()
                .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))?;
            rest.get(flag + 1).cloned()
        }
        _ => None,
    }
}

static PUSH_FALLBACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bgit\b[^|;&\n]*\b(push|send-pack)\b").expect("static regex"));

/// Whether running `command` would push or publish: `git push`, `git send-pack`, `jj git push`,
/// `hg push`, `gh pr create|merge`, `gh release create`, `gh repo create --push`.
pub fn pushes(command: &str, analysis: &ShellAnalysis) -> bool {
    if !analysis.parsed {
        return PUSH_FALLBACK.is_match(command);
    }
    analysis.commands.iter().any(|c| is_push(&c.words))
}

/// Non-option arguments, skipping the value of options listed in `takes_value`.
fn positionals<'a>(args: &'a [String], takes_value: &'a [&str]) -> impl Iterator<Item = &'a str> {
    let mut skip = false;
    args.iter().map(String::as_str).filter(move |arg| {
        if std::mem::take(&mut skip) {
            return false;
        }
        if arg.starts_with('-') {
            skip = takes_value.contains(arg);
            return false;
        }
        true
    })
}

fn is_push(words: &[String]) -> bool {
    let Some((first, args)) = words.split_first() else {
        return false;
    };
    match program(first) {
        "git" => {
            let subcommand =
                positionals(args, &["-C", "-c", "--git-dir", "--work-tree", "--namespace"]).next();
            matches!(subcommand, Some("push" | "send-pack"))
        }
        "jj" => {
            let mut pos = positionals(args, &["-R", "--repository", "--at-op", "--config"]);
            pos.next() == Some("git") && pos.next() == Some("push")
        }
        "hg" => positionals(args, &["-R", "--repository", "--cwd", "--config"]).next() == Some("push"),
        "gh" => {
            let mut pos = positionals(args, &["-R", "--repo"]);
            match (pos.next(), pos.next()) {
                (Some("pr"), Some("create" | "merge")) | (Some("release"), Some("create")) => true,
                (Some("repo"), Some("create")) => args.iter().any(|a| a == "--push"),
                _ => false,
            }
        }
        _ => false,
    }
}

const BLOCKED_KEY: &str = "url.azazel-push-blocked://denied/.pushInsteadOf";
const PUSH_URL_PREFIXES: [&str; 6] = ["https://", "http://", "ssh://", "git://", "git@", "file://"];

/// Environment that makes any push fail at the transport, whoever starts it: every push URL is
/// rewritten to an unusable scheme. `lookup` reads the environment the child will inherit, so the
/// entries are appended after any git config already passed that way.
pub fn guard_env(lookup: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let existing: usize = lookup("GIT_CONFIG_COUNT")
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    let mut env = vec![
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        (
            "GIT_CONFIG_COUNT".to_string(),
            (existing + PUSH_URL_PREFIXES.len()).to_string(),
        ),
    ];
    for (i, prefix) in PUSH_URL_PREFIXES.iter().enumerate() {
        env.push((
            format!("GIT_CONFIG_KEY_{}", existing + i),
            BLOCKED_KEY.to_string(),
        ));
        env.push((format!("GIT_CONFIG_VALUE_{}", existing + i), prefix.to_string()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subjects(command: &str) -> Vec<String> {
        let analysis = analyze(command, true);
        assert!(analysis.parsed, "{command}");
        analysis.subjects()
    }

    fn blocked(command: &str) -> bool {
        pushes(command, &analyze(command, true))
    }

    #[test]
    fn splits_pipelines_lists_subshells_and_substitutions() {
        assert_eq!(subjects("cargo test -p x"), ["cargo test -p x"]);
        assert_eq!(
            subjects("cargo build && cargo test | tee log.txt; ls"),
            ["cargo build", "cargo test", "tee log.txt", "ls"]
        );
        assert_eq!(
            subjects("(cd sub && make) || echo failed"),
            ["cd sub", "make", "echo failed"]
        );
        assert_eq!(
            subjects("echo $(git rev-parse HEAD) `date`"),
            ["echo $(git rev-parse HEAD) `date`", "git rev-parse HEAD", "date"]
        );
        assert_eq!(
            subjects("for f in *.rs; do rustfmt \"$f\"; done"),
            ["rustfmt \"$f\""]
        );
        assert_eq!(subjects("cat <<'EOF' > out.txt\nhello\nEOF"), ["cat"]);
    }

    #[test]
    fn strips_assignments_and_wrappers() {
        assert_eq!(subjects("RUST_LOG=debug CI=1 cargo test"), ["cargo test"]);
        assert_eq!(subjects("sudo -u root rm -rf /tmp/x"), ["rm -rf /tmp/x"]);
        assert_eq!(
            subjects("env -u HOME FOO=1 time nohup npm run build"),
            ["npm run build"]
        );
        assert_eq!(
            subjects("find . -name '*.o' | xargs -n 1 rm"),
            ["find . -name '*.o'", "rm"]
        );
        // A wrapper with nothing to wrap stays visible.
        assert_eq!(subjects("env"), ["env"]);
    }

    #[test]
    fn scripts_passed_to_a_shell_are_analysed_too() {
        assert_eq!(
            subjects("bash -lc 'cd app && npm test'"),
            ["cd app", "npm test", "bash -lc 'cd app && npm test'"]
        );
        assert_eq!(
            subjects("eval \"make clean\""),
            ["make clean", "eval \"make clean\""]
        );
    }

    #[test]
    fn unparsable_and_powershell_commands_are_one_opaque_subject() {
        for (command, posix) in [
            ("echo 'unterminated", true),
            ("Get-ChildItem | Remove-Item", false),
        ] {
            let analysis = analyze(command, posix);
            assert!(!analysis.parsed);
            assert_eq!(analysis.subjects(), [command]);
        }
    }

    #[test]
    fn push_is_caught_in_every_position() {
        for command in [
            "git push",
            "git push origin main --force",
            "git -C repo -c user.name=x push",
            "cargo test && git push",
            "echo $(git push)",
            "(cd repo; git push) | cat",
            "sudo git push",
            "GIT_SSH_COMMAND=x git push",
            "bash -c 'git push origin'",
            "eval \"git push\"",
            "/usr/bin/git push",
            "git.exe send-pack origin",
            "jj git push",
            "hg push",
            "gh pr create --fill",
            "gh pr merge 12",
            "gh release create v1",
            "gh repo create x --source . --push",
        ] {
            assert!(blocked(command), "should be blocked: {command}");
        }
    }

    #[test]
    fn commands_that_only_mention_push_are_not_blocked() {
        for command in [
            "git status",
            "git log --grep push",
            "git commit -m 'push the limits'",
            "git stash push -m wip",
            "echo git push",
            "grep -r 'git push' docs",
            "gh pr view 12",
            "gh repo create x --private",
            "git config --get remote.pushDefault",
            "cargo run -- push",
        ] {
            assert!(!blocked(command), "should be allowed: {command}");
        }
    }

    #[test]
    fn unparsed_commands_fall_back_to_a_pattern() {
        let check = |command: &str| pushes(command, &analyze(command, false));
        assert!(check("git push origin main"));
        assert!(check("cd repo; git -C . push"));
        assert!(!check("git status; echo push"));
        assert!(!check("Write-Output 'pushing'"));
    }

    #[test]
    fn guard_env_appends_after_existing_git_config() {
        let env = guard_env(&|name| (name == "GIT_CONFIG_COUNT").then(|| "2".to_string()));
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("GIT_CONFIG_COUNT"), Some("8"));
        assert_eq!(
            get("GIT_CONFIG_KEY_2"),
            Some("url.azazel-push-blocked://denied/.pushInsteadOf")
        );
        assert_eq!(get("GIT_CONFIG_VALUE_2"), Some("https://"));
        assert_eq!(get("GIT_CONFIG_VALUE_7"), Some("file://"));
        assert_eq!(get("GIT_CONFIG_KEY_0"), None);
        assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
        assert_eq!(
            guard_env(&|_| None)
                .iter()
                .filter(|(k, _)| k.starts_with("GIT_CONFIG_KEY_"))
                .count(),
            6
        );
    }
}
