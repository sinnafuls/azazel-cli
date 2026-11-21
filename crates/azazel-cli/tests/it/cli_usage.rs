//! The command line as a person types it: `azazel config`, a mistyped subcommand, bad flag
//! values, the help of a subcommand, `azazel doctor` and `azazel browser open`.

use std::path::PathBuf;
use std::process::Output;

use crate::common::{stderr, stdout};
use assert_cmd::Command;

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.azazel")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Sandbox { dir }
    }

    fn home_config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("home/config.toml")).unwrap_or_default()
    }

    fn project_config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("project/.azazel/config.toml")).unwrap_or_default()
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .arg("--cwd")
            .arg::<PathBuf>(self.dir.path().join("project"));
        command
    }

    fn azazel(&self, args: &[&str]) -> Output {
        self.command().args(args).write_stdin("").output().unwrap()
    }
}

#[test]
fn config_set_writes_the_home_config_and_get_says_which_layer_sets_the_value() {
    let sandbox = Sandbox::new();
    let before = sandbox.azazel(&["config", "get", "retry.max_attempts"]);
    assert_eq!(before.status.code(), Some(0), "{}", stderr(&before));
    assert_eq!(stdout(&before).trim(), "3");
    assert!(stderr(&before).contains("the default"), "{}", stderr(&before));

    let set = sandbox.azazel(&["config", "set", "retry.max_attempts", "5"]);
    assert_eq!(set.status.code(), Some(0), "{}", stderr(&set));
    assert!(
        sandbox.home_config().contains("max_attempts = 5"),
        "{}",
        sandbox.home_config()
    );

    let after = sandbox.azazel(&["config", "get", "retry.max_attempts"]);
    assert_eq!(stdout(&after).trim(), "5");
    assert!(stderr(&after).contains("home config"), "{}", stderr(&after));

    // A list is given as a TOML array, as the docs show.
    let list = sandbox.azazel(&["config", "set", "triggers.slack_users", "[\"U0123ABCD\"]"]);
    assert_eq!(list.status.code(), Some(0), "{}", stderr(&list));
    let got = sandbox.azazel(&["config", "get", "triggers.slack_users"]);
    assert_eq!(stdout(&got).trim(), "[\"U0123ABCD\"]");
}

#[test]
fn config_unset_takes_the_entry_out_of_the_config_where_set_with_an_empty_value_writes_one() {
    let sandbox = Sandbox::new();
    assert_eq!(
        sandbox
            .azazel(&["config", "set", "retry.max_attempts", "5"])
            .status
            .code(),
        Some(0)
    );
    assert!(
        sandbox.home_config().contains("max_attempts = 5"),
        "{}",
        sandbox.home_config()
    );

    let unset = sandbox.azazel(&["config", "unset", "retry.max_attempts"]);
    assert_eq!(unset.status.code(), Some(0), "{}", stderr(&unset));
    assert!(
        !sandbox.home_config().contains("max_attempts"),
        "{}",
        sandbox.home_config()
    );
    let after = sandbox.azazel(&["config", "get", "retry.max_attempts"]);
    assert!(stderr(&after).contains("the default"), "{}", stderr(&after));

    // Nothing left to remove is a failure that says so; an unknown key is a usage error.
    let again = sandbox.azazel(&["config", "unset", "retry.max_attempts"]);
    assert_eq!(again.status.code(), Some(1), "{}", stderr(&again));
    let unknown = sandbox.azazel(&["config", "unset", "nonsense.key"]);
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr(&unknown));

    // `--project` takes it out of the project's config only, in a trusted project.
    assert_eq!(
        sandbox
            .azazel(&["config", "set", "retry.max_attempts", "5"])
            .status
            .code(),
        Some(0)
    );
    let set = sandbox.azazel(&["--trust", "config", "set", "--project", "retry.max_attempts", "4"]);
    assert_eq!(set.status.code(), Some(0), "{}", stderr(&set));
    let refused = sandbox.azazel(&["config", "unset", "--project", "retry.max_attempts"]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    let project = sandbox.azazel(&["--trust", "config", "unset", "--project", "retry.max_attempts"]);
    assert_eq!(project.status.code(), Some(0), "{}", stderr(&project));
    assert!(
        !sandbox.project_config().contains("max_attempts"),
        "{}",
        sandbox.project_config()
    );
    assert!(
        sandbox.home_config().contains("max_attempts = 5"),
        "{}",
        sandbox.home_config()
    );
}

#[test]
fn config_set_refuses_an_unknown_key_a_bad_value_and_an_untrusted_project() {
    let sandbox = Sandbox::new();
    for args in [
        &["config", "set", "nonsense.key", "1"][..],
        &["config", "set", "permissions.mode", "wrong"],
        &["config", "set", "retry.max_attempts", "many"],
        &["config", "get", "nonsense.key"],
        &["config", "set", "--project", "retry.max_attempts", "4"],
    ] {
        let output = sandbox.azazel(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", stderr(&output));
    }
    assert_eq!(sandbox.home_config(), "");
    assert_eq!(sandbox.project_config(), "");

    // A trusted project takes it, in its own file.
    let trusted = sandbox.azazel(&["--trust", "config", "set", "--project", "retry.max_attempts", "4"]);
    assert_eq!(trusted.status.code(), Some(0), "{}", stderr(&trusted));
    assert!(
        sandbox.project_config().contains("max_attempts = 4"),
        "{}",
        sandbox.project_config()
    );
    assert_eq!(sandbox.home_config(), "");
}

#[test]
fn a_lone_word_close_to_a_subcommand_is_a_typo_and_anything_else_is_a_prompt() {
    let sandbox = Sandbox::new();
    let typo = sandbox.azazel(&["sesions"]);
    assert_eq!(typo.status.code(), Some(2));
    assert!(
        stderr(&typo).contains("unknown command `sesions`; did you mean `sessions`?")
            && stderr(&typo).contains("To send `sesions` as a prompt, run `azazel -- sesions`."),
        "{}",
        stderr(&typo)
    );
    // Not close to anything, or more than one word: a prompt, which needs a terminal here.
    for args in [&["nonsense"][..], &["fix", "the", "bug"]] {
        let prompt = sandbox.azazel(args);
        assert!(
            stderr(&prompt).contains("needs a terminal"),
            "{args:?}: {}",
            stderr(&prompt)
        );
    }
    // `--` says that what follows is a prompt.
    let escaped = sandbox.azazel(&["--", "sesions"]);
    assert!(
        stderr(&escaped).contains("needs a terminal"),
        "{}",
        stderr(&escaped)
    );
}

#[test]
fn bad_values_of_flags_exit_with_the_usage_status() {
    let sandbox = Sandbox::new();
    for flag in ["--mode", "--thinking", "--tier", "--profile"] {
        let output = sandbox.azazel(&["-p", "hi", flag, "wrong"]);
        assert_eq!(output.status.code(), Some(2), "{flag}: {}", stderr(&output));
    }
}

#[test]
fn the_help_of_a_subcommand_lists_only_the_options_that_apply_to_it() {
    let sandbox = Sandbox::new();
    let top = stdout(&sandbox.azazel(&["--help"]));
    for flag in ["--model", "--mode", "--output", "--cwd"] {
        assert!(top.contains(flag), "{flag} is missing from the top-level help");
    }
    // The pointer of `--agent` leads to something that exists.
    assert!(!top.contains("azazel agents"), "{top}");
    assert!(top.contains("/agents"), "{top}");

    for command in [
        &["roles"][..],
        &["sessions", "list"],
        &["doctor"],
        &["secrets", "list"],
    ] {
        let mut args = command.to_vec();
        args.push("--help");
        let help = stdout(&sandbox.azazel(&args));
        for flag in [
            "--model ",
            "--models",
            "--mode ",
            "--output",
            "--tier",
            "--coordinator",
            "--ssh",
        ] {
            assert!(!help.contains(flag), "{command:?} offers {flag}:\n{help}");
        }
        assert!(help.contains("--cwd"), "{command:?}:\n{help}");
    }
    // What a subcommand does use is still there, and an option it hides is still accepted.
    assert!(stdout(&sandbox.azazel(&["roles", "--help"])).contains("--json"));
    assert!(stdout(&sandbox.azazel(&["logs", "--help"])).contains("--follow"));
    let accepted = sandbox.azazel(&["roles", "--json", "--model", "anthropic/claude-sonnet-4-5"]);
    assert_eq!(accepted.status.code(), Some(0), "{}", stderr(&accepted));
}

#[test]
fn doctor_counts_the_providers_without_a_credential_instead_of_listing_them() {
    let sandbox = Sandbox::new();
    let output = sandbox.azazel(&["doctor"]);
    let report = stdout(&output);
    let line = report
        .lines()
        .find(|line| line.contains("providers without a credential"))
        .unwrap_or_else(|| panic!("{report}"));
    assert!(line.contains("azazel models --all"), "{line}");
    assert!(line.len() < 200, "{line}");
}

#[cfg(feature = "browser")]
#[test]
fn browser_open_says_it_waits_for_the_window_before_it_does() {
    let sandbox = Sandbox::new();
    // A program that exits at once stands in for a browser that cannot start.
    let fake = crate::common::fixture_program("fake-check");
    let output = sandbox
        .command()
        .env("AZAZEL_BROWSER", fake)
        .args(["browser", "open"])
        .write_stdin("")
        .output()
        .unwrap();
    assert!(
        stdout(&output).contains("waits until the window is closed"),
        "{}\n{}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn scripts_list_show_and_delete_work_on_the_saved_scripts_of_the_user() {
    let sandbox = Sandbox::new();
    let dir = sandbox.dir.path().join("home/scripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("hello.luau"),
        "---\ndescription: Say hello\n---\nreturn \"hi\"\n",
    )
    .unwrap();

    let listed = sandbox.azazel(&["scripts", "list"]);
    assert!(listed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout).trim(),
        "hello  (user)  Say hello"
    );
    let shown = String::from_utf8_lossy(&sandbox.azazel(&["scripts", "show", "hello"]).stdout).into_owned();
    assert!(
        shown.contains("description: Say hello") && shown.contains("return \"hi\""),
        "{shown}"
    );
    assert!(sandbox.azazel(&["scripts", "delete", "hello"]).status.success());
    assert!(!dir.join("hello.luau").exists());
    let missing = sandbox.azazel(&["scripts", "show", "hello"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("there is no saved script `hello`"));
}
