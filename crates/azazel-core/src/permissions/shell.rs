//! Breaking a shell command line into its simple commands, the guard that keeps agents from
//! pushing, and the list of commands that destroy work.

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

/// A flag cluster (`-rf`) that contains one of `letters`, or one of the long forms.
fn has_flag(args: &[String], letters: &[char], long: &[&str]) -> bool {
    args.iter().any(|arg| {
        long.contains(&arg.as_str())
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg.chars().skip(1).any(|c| letters.contains(&c)))
    })
}

/// `/`, a drive root, the home directory, or everything directly inside one of them.
fn root_or_home(target: &str) -> bool {
    let target = target.trim_end_matches(['*', '.']).trim_end_matches(['/', '\\']);
    let drive = target.len() == 2 && target.ends_with(':') && target.starts_with(|c: char| c.is_ascii_alphabetic());
    target.is_empty() || drive || matches!(target, "~" | "$HOME" | "${HOME}" | "%USERPROFILE%" | "$env:USERPROFILE")
}

/// `..`, `../..` and everything directly inside them.
fn parent_dir(target: &str) -> bool {
    let target = target.trim_end_matches('*').trim_end_matches(['/', '\\']);
    !target.is_empty() && target.split(['/', '\\']).all(|part| part == "..")
}

/// What one simple command destroys, in words that follow "this command".
fn destroys(words: &[String], outside: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    let (first, args) = words.split_first()?;
    let name = program(first).to_ascii_lowercase();
    // What follows the options: `--` ends them.
    let targets = || {
        let mut options_over = false;
        args.iter().map(String::as_str).filter(move |arg| {
            if options_over {
                return true;
            }
            if *arg == "--" {
                options_over = true;
                return false;
            }
            !arg.starts_with('-')
        })
    };
    match name.as_str() {
        "rm" => {
            if !has_flag(args, &['r', 'R'], &["--recursive"]) {
                return None;
            }
            let mut found = None;
            for target in targets() {
                if root_or_home(target) {
                    return Some("deletes everything under / or your home directory (rm -r)");
                }
                if parent_dir(target) {
                    found = Some("deletes the directory above the one it runs in (rm -r)");
                } else if found.is_none() && !target.contains('$') && outside(target) {
                    found = Some("deletes a directory tree outside the project (rm -r)");
                }
            }
            found
        }
        "git" => {
            let mut words = positionals(args, &["-C", "-c", "--git-dir", "--work-tree", "--namespace"]);
            let subcommand = words.next()?;
            let rest: Vec<&str> = words.collect();
            let whole_tree = || rest.iter().any(|arg| matches!(*arg, "." | ":/" | "*"));
            match subcommand {
                "reset" if args.iter().any(|arg| arg == "--hard") => {
                    Some("discards uncommitted changes (git reset --hard)")
                }
                "clean"
                    if has_flag(args, &['f'], &["--force"]) && !has_flag(args, &['n'], &["--dry-run"]) =>
                {
                    Some("deletes untracked files (git clean -f)")
                }
                "checkout" if whole_tree() => Some("discards uncommitted changes (git checkout .)"),
                "restore"
                    if whole_tree()
                        && (!has_flag(args, &['S'], &["--staged"])
                            || has_flag(args, &['W'], &["--worktree"])) =>
                {
                    Some("discards uncommitted changes (git restore .)")
                }
                "branch"
                    if has_flag(args, &['D'], &[])
                        || (has_flag(args, &['d'], &["--delete"]) && has_flag(args, &['f'], &["--force"])) =>
                {
                    Some("deletes a branch even when it is not merged (git branch -D)")
                }
                "stash" if matches!(rest.first(), Some(&("drop" | "clear"))) => {
                    Some("deletes stashed changes (git stash drop or clear)")
                }
                _ => None,
            }
        }
        "dd" => args
            .iter()
            .any(|arg| arg.starts_with("of=/dev/"))
            .then_some("writes over a device (dd of=/dev/…)"),
        "chmod" | "chown" => (has_flag(args, &['R'], &["--recursive"]) && targets().any(root_or_home))
            .then_some("changes permissions or owners of everything under / or your home directory"),
        "terraform" | "tofu" => {
            let first = positionals(args, &["-chdir"]).next();
            (first == Some("destroy")
                || (first == Some("apply") && args.iter().any(|arg| arg.trim_start_matches('-') == "destroy")))
            .then_some("destroys the infrastructure Terraform manages")
        }
        "kubectl" => {
            let mut words = positionals(args, &["-n", "--namespace", "--context", "--kubeconfig"]);
            (words.next() == Some("delete")
                && matches!(words.next(), Some("namespace" | "namespaces" | "ns")))
            .then_some("deletes a Kubernetes namespace and everything in it")
        }
        "docker" | "podman" => {
            let mut words = positionals(args, &["-H", "--host", "--context"]);
            (words.next() == Some("system")
                && words.next() == Some("prune")
                && has_flag(args, &['a'], &["--all"]))
            .then_some("removes every unused image, container and network (docker system prune -a)")
        }
        "psql" | "mysql" | "mariadb" => {
            let mut statements = args.iter().enumerate().filter_map(|(at, arg)| {
                ["--command=", "--execute="]
                    .iter()
                    .find_map(|long| arg.strip_prefix(long))
                    .or_else(|| {
                        matches!(arg.as_str(), "-c" | "-e" | "--command" | "--execute")
                            .then(|| args.get(at + 1).map(String::as_str))
                            .flatten()
                    })
                    .or_else(|| {
                        ["-c", "-e"]
                            .iter()
                            .find_map(|short| arg.strip_prefix(short))
                            .filter(|rest| !rest.is_empty())
                    })
            });
            statements
                .any(|sql| {
                    let sql = sql.to_ascii_lowercase();
                    sql.split_whitespace().collect::<Vec<_>>().join(" ").contains("drop database")
                })
                .then_some("drops a database")
        }
        name if name.starts_with("mkfs") => Some("formats a file system (mkfs)"),
        _ => None,
    }
}

/// What `command` would destroy, in words that follow "this command"; `None` for a command that
/// is not one of the known destructive ones. `outside` says whether a path, as written, lies
/// outside the directories agents work in.
///
/// Recognised: `rm -r` on `/`, `~`, `..` or a path outside the project; `git reset --hard`,
/// `clean -f`, `checkout .`, `restore .`, `branch -D`, `stash drop|clear`; `dd of=/dev/…`;
/// `mkfs`; `chmod -R` and `chown -R` on `/` or `~`; `terraform destroy`; `kubectl delete
/// namespace`; `docker system prune -a`; `drop database` given to `psql` or `mysql`.
pub fn destructive(
    command: &str,
    analysis: &ShellAnalysis,
    outside: &dyn Fn(&str) -> bool,
) -> Option<&'static str> {
    if analysis.parsed {
        return analysis
            .commands
            .iter()
            .find_map(|simple| destroys(&simple.words, outside));
    }
    // Not parsed (PowerShell, or a syntax error): split roughly and look at the same words.
    command.split([';', '|', '&', '\n']).find_map(|part| {
        let words = strip_wrappers(part.split_whitespace().map(str::to_string).collect());
        let words: Vec<String> = words.iter().map(|word| unquote(word)).collect();
        destroys(&words, outside)
    })
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

    fn risk(command: &str) -> Option<&'static str> {
        // Paths that start with `/` or `..` count as outside the project.
        let outside = |path: &str| path.starts_with('/') || path.starts_with("..");
        destructive(command, &analyze(command, true), &outside)
    }

    #[test]
    fn destructive_commands_are_recognised_in_every_position() {
        for command in [
            "rm -rf /",
            "rm -rf /*",
            "rm -r ~",
            "rm -fr ~/",
            "rm --recursive $HOME",
            "rm -rf ..",
            "rm -rf ../*",
            "rm -rf /var/lib/app",
            "rm -rf build ../other-project",
            "sudo rm -rf -- /etc",
            "git reset --hard",
            "git reset --hard HEAD~3",
            "git -C repo reset --hard origin/main",
            "git clean -fd",
            "git clean -xdf",
            "git clean --force",
            "git checkout -- .",
            "git checkout .",
            "git restore .",
            "git restore --staged --worktree .",
            "git branch -D feature",
            "git branch --delete --force feature",
            "git stash drop",
            "git stash clear",
            "dd if=/dev/zero of=/dev/sda bs=1M",
            "mkfs.ext4 /dev/sdb1",
            "mkfs -t ext4 /dev/sdb1",
            "chmod -R 777 /",
            "chown -R me ~",
            "terraform destroy",
            "terraform apply -destroy -auto-approve",
            "kubectl delete namespace staging",
            "kubectl -n x delete ns staging",
            "docker system prune -a",
            "docker system prune -af --volumes",
            "psql -c 'DROP DATABASE app'",
            "psql --command='drop   database app'",
            "mysql -e \"drop database app\"",
            "cargo build && git reset --hard",
            "echo $(git clean -f)",
            "bash -c 'git stash clear'",
        ] {
            assert!(risk(command).is_some(), "should be destructive: {command}");
        }
        assert_eq!(
            risk("git reset --hard"),
            Some("discards uncommitted changes (git reset --hard)")
        );
        assert_eq!(
            risk("rm -rf build /"),
            Some("deletes everything under / or your home directory (rm -r)")
        );
    }

    #[test]
    fn ordinary_commands_are_not_destructive() {
        for command in [
            "rm -r target/tmp",
            "rm -rf ./build node_modules",
            "rm file.txt",
            "rm -f /tmp/one-file",
            "git reset HEAD~1",
            "git reset --soft HEAD~1",
            "git clean -n",
            "git clean -nfd",
            "git checkout main",
            "git checkout -- src/lib.rs",
            "git restore src/lib.rs",
            "git restore --staged .",
            "git branch -d merged",
            "git stash",
            "git stash pop",
            "git log --grep 'reset --hard'",
            "dd if=in.img of=out.img",
            "chmod -R 755 scripts",
            "chmod 600 ~/.ssh/id_ed25519",
            "terraform plan -destroy",
            "kubectl delete pod web-1",
            "kubectl get namespace",
            "docker system prune",
            "docker image prune -a",
            "psql -c 'select 1'",
            "echo git reset --hard",
            "echo 'drop database app'",
        ] {
            assert_eq!(risk(command), None, "should be ordinary: {command}");
        }
    }

    #[test]
    fn unparsed_commands_are_checked_word_by_word() {
        let risk = |command: &str| destructive(command, &analyze(command, false), &|_| false);
        assert!(risk("git reset --hard; Write-Output done").is_some());
        assert!(risk("cd repo; git clean -fdx").is_some());
        assert!(risk("rm -r C:\\").is_some());
        assert_eq!(risk("Write-Output 'git reset --hard'"), None);
        assert_eq!(risk("Get-ChildItem | Remove-Item"), None);
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
