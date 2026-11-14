//! The check against this repository, and against copies of its docs with drift put in.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::code::Code;
use super::{Docs, Finding, check, gather};

fn root() -> PathBuf {
    super::workspace_root()
}

/// What the code registers; read once, since it builds the application.
static CODE: LazyLock<Code> = LazyLock::new(|| gather(&root()).unwrap());

fn code() -> &'static Code {
    &CODE
}

/// A copy of the pages of this repository that a test may change.
struct Copy {
    dir: tempfile::TempDir,
}

/// Copies a directory and what is below it.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&entry.path(), &target),
            false => drop(std::fs::copy(entry.path(), target).unwrap()),
        }
    }
}

impl Copy {
    /// The files at the root of the repository, `docs/` and `skills/`: every page, and what
    /// the pages link to.
    fn new() -> Copy {
        let dir = tempfile::tempdir().unwrap();
        let root = root();
        for entry in std::fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
            }
        }
        for folder in ["docs", "skills"] {
            copy_dir(&root.join(folder), &dir.path().join(folder));
        }
        Copy { dir }
    }

    fn edit(&self, page: &str, change: impl FnOnce(String) -> String) {
        let file = self.dir.path().join(page);
        let before = std::fs::read_to_string(&file).unwrap();
        let after = change(before.clone());
        assert_ne!(before, after, "the change to {page} changed nothing");
        std::fs::write(file, after).unwrap();
    }

    /// Takes out every line that starts with `start`.
    fn remove_lines(&self, page: &str, start: &str) {
        self.edit(page, |text| {
            let kept: Vec<&str> = text.lines().filter(|line| !line.starts_with(start)).collect();
            kept.join("\n") + "\n"
        });
    }

    /// Puts `line` in after the first line that starts with `start`.
    fn add_after(&self, page: &str, start: &str, line: &str) {
        self.edit(page, |text| {
            let mut out = Vec::new();
            let mut added = false;
            for existing in text.lines() {
                out.push(existing.to_string());
                if !added && existing.starts_with(start) {
                    out.push(line.to_string());
                    added = true;
                }
            }
            out.join("\n") + "\n"
        });
    }

    fn findings(&self) -> Vec<String> {
        self.findings_for(code())
    }

    fn findings_for(&self, code: &Code) -> Vec<String> {
        let docs = Docs::load(self.dir.path()).unwrap();
        check(&docs, code).iter().map(Finding::to_string).collect()
    }
}

/// The findings that are about `name` on `page`.
fn about<'a>(findings: &'a [String], page: &str, name: &str) -> Vec<&'a String> {
    findings
        .iter()
        .filter(|finding| finding.starts_with(page) && finding.contains(name))
        .collect()
}

#[test]
fn the_docs_of_this_repository_agree_with_the_code() {
    let docs = Docs::load(&root()).unwrap();
    let findings: Vec<String> = check(&docs, code()).iter().map(Finding::to_string).collect();
    assert!(
        findings.is_empty(),
        "run `cargo xtask docs-check`:\n{}",
        findings.join("\n")
    );
    // An untouched copy is the same docs.
    assert_eq!(Copy::new().findings(), Vec::<String>::new());
}

#[test]
fn a_setting_without_a_row_and_a_row_without_a_setting_are_reported() {
    let copy = Copy::new();
    copy.remove_lines("docs/configuration.md", "| | `cooldown_secs` |");
    copy.add_after(
        "docs/configuration.md",
        "| `[retry]` | `max_attempts` |",
        "| | `patience` | `7` | Not a setting |",
    );
    let findings = copy.findings();
    assert_eq!(
        about(&findings, "docs/configuration.md", "`retry.cooldown_secs`"),
        [
            "docs/configuration.md: setting `retry.cooldown_secs` is in the code and missing from the settings tables"
        ]
    );
    let stale = about(&findings, "docs/configuration.md", "`retry.patience`");
    assert_eq!(stale.len(), 1, "{findings:?}");
    // The row is named by its line.
    let line = stale[0].split(':').nth(1).unwrap().parse::<usize>().unwrap();
    let page = std::fs::read_to_string(copy.dir.path().join("docs/configuration.md")).unwrap();
    assert!(page.lines().nth(line - 1).unwrap().contains("`patience`"));
    assert!(stale[0].ends_with("is in the settings tables and not in the code"));
    assert_eq!(findings.len(), 2, "{findings:?}");
}

#[test]
fn a_default_that_differs_from_the_code_is_reported() {
    let copy = Copy::new();
    copy.edit("docs/configuration.md", |text| {
        text.replace(
            "| `[retry]` | `max_attempts` | `3` |",
            "| `[retry]` | `max_attempts` | `5` |",
        )
    });
    copy.edit("docs/tools.md", |text| {
        text.replace(
            "| `max_result_bytes` | `50000` |",
            "| `max_result_bytes` | `64000` |",
        )
    });
    let findings = copy.findings();
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(findings[0].starts_with("docs/configuration.md:"));
    assert!(
        findings[0]
            .ends_with("setting `retry.max_attempts` defaults to `3` in the code and the table says `5`")
    );
    assert!(findings[1].starts_with("docs/tools.md:"));
    assert!(findings[1].contains("`tools.max_result_bytes` defaults to `50000`"));
}

#[test]
fn the_keys_of_the_key_table_are_the_default_bindings() {
    let copy = Copy::new();
    // A key that is not the action's, one of two keys left out, and a key nothing can bind.
    copy.edit("docs/tui.md", |text| {
        text.replace(
            "| `Ctrl+L`, `Alt+P` | `model_picker` |",
            "| `Ctrl+K`, `Alt+P` | `model_picker` |",
        )
        .replace(
            "| `Shift+Enter`, `Ctrl+J`, `Ctrl+Enter` | `newline` |",
            "| `Shift+Enter` | `newline` |",
        )
        .replace("| `F5` | `retry` |", "| `F5`, `Hyper+Nope` | `retry` |")
    });
    let findings = copy.findings();
    assert_eq!(findings.len(), 3, "{findings:?}");
    assert!(findings.iter().all(|finding| finding.starts_with("docs/tui.md:")));
    assert!(findings[0].ends_with(
        "keymap action `newline` is bound to `ctrl+enter`, `ctrl+j`, `shift+enter` by default and the key \
         table says `shift+enter`"
    ));
    assert!(findings[1].ends_with(
        "keymap action `model_picker` is bound to `alt+p`, `ctrl+l` by default and the key table says `alt+p`, \
         `ctrl+k`"
    ));
    assert!(findings[2].ends_with("`Hyper+Nope` in the key table is not a key the keymap can bind"));
}

#[test]
fn a_new_name_in_the_code_is_reported_on_the_page_that_owns_it() {
    let mut code = code().clone();
    code.config
        .insert("tui.sparkle".to_string(), Some("true".to_string()));
    code.trust_gated.insert("tui.sparkle".to_string());
    code.hook_events.insert("pre_sparkle".to_string());
    code.key_actions
        .insert("sparkle".to_string(), ["f9".to_string()].into());
    code.rpc_commands.insert("sparkle".to_string());
    code.rpc_frames.insert("sparkled".to_string());
    code.luau_api.insert("azazel.sparkle".to_string());
    code.luau_events.insert("sparkle".to_string());
    code.slash_commands.insert("sparkle".to_string());
    code.tools.insert("sparkle".to_string());
    let findings = Copy::new().findings_for(&code);
    let expected = [
        "docs/configuration.md: setting `tui.sparkle` is in the code and missing from the settings tables",
        "docs/configuration.md: trust-gated setting `tui.sparkle` is in the code and missing from the `Gated settings:` list",
        "docs/hooks.md: hook event `pre_sparkle` is in the code and missing from the events table",
        "docs/configuration.md: hook event `pre_sparkle` is in the code and missing from the `Hook events:` list",
        "skills/azazel-extensions/SKILL.md: hook event `pre_sparkle` is in the code and missing from the hook events table",
        "docs/tui.md: keymap action `sparkle` is in the code and missing from the key table",
        "docs/rpc.md: RPC command `sparkle` is in the code and missing from the command tables",
        "docs/rpc.md: RPC frame `sparkled` is in the code and missing from the events table, the frame sections and the JSON under \"Starting\" and \"Responses\"",
        "docs/extensions.md: Luau function `azazel.sparkle` is in the code and missing from the `Function | Contract` tables",
        "docs/extensions.md: Luau event `sparkle` is in the code and missing from the events table",
        "skills/azazel-extensions/SKILL.md: Luau function `azazel.sparkle` is in the code and missing from the `Function | Contract` tables",
        "skills/azazel-extensions/SKILL.md: Luau event `sparkle` is in the code and missing from the events table",
        "docs/tui.md: slash command `sparkle` is in the code and missing from the tables of \"Slash commands\"",
        "docs/tools.md: tool `sparkle` is in the code and missing from the `Tool` tables",
    ];
    assert_eq!(findings, expected);
}

#[test]
fn a_name_only_the_docs_have_is_reported_with_its_line() {
    let copy = Copy::new();
    copy.add_after(
        "docs/hooks.md",
        "| `setup` | `Setup` |",
        "| `teardown` | `Teardown` | none | Cannot block |",
    );
    copy.add_after(
        "docs/tui.md",
        "| `F5` | `retry` |",
        "| `F6` | `sparkle` | Nothing |",
    );
    copy.add_after(
        "docs/tui.md",
        "| `/help` |",
        "| `/queue <text>` | Queue a message for later |",
    );
    copy.add_after(
        "docs/tools.md",
        "| `glob` |",
        "| `find_files` | `pattern` | Finds files |",
    );
    copy.add_after("docs/rpc.md", "| `abort` |", "| `dance` | | Dances | `null` |");
    // The example at the end of the page still shows the frame; that does not document it.
    copy.remove_lines("docs/rpc.md", "| `tool_start` |");
    copy.add_after(
        "docs/extensions.md",
        "| `azazel.notify(text, level?)` |",
        "| `azazel.beep()` | Beeps |",
    );
    copy.add_after(
        "skills/azazel-extensions/SKILL.md",
        "| `user_prompt` |",
        "| `user_idle` | | |",
    );
    let findings = copy.findings();
    let named = |page: &str, text: &str| {
        let found = about(&findings, page, text);
        assert_eq!(found.len(), 1, "{page} {text}: {findings:?}");
        // `page:line: text`: the line is a number.
        assert!(
            found[0].split(':').nth(1).unwrap().parse::<usize>().is_ok(),
            "{}",
            found[0]
        );
    };
    named(
        "docs/hooks.md",
        "hook event `teardown` is in the events table and not in the code",
    );
    named(
        "docs/tui.md",
        "keymap action `sparkle` is in the key table and not in the code",
    );
    named(
        "docs/tui.md",
        "slash command `queue` is in the tables of \"Slash commands\" and not in the code",
    );
    named(
        "docs/tools.md",
        "tool `find_files` is in the `Tool` tables and not in the code",
    );
    named(
        "docs/rpc.md",
        "RPC command `dance` is in the command tables and not in the code",
    );
    named(
        "docs/extensions.md",
        "Luau function `azazel.beep` is in the `Function | Contract` tables and not in the code",
    );
    named(
        "skills/azazel-extensions/SKILL.md",
        "Luau event `user_idle` is in the events table and not in the code",
    );
    assert!(findings.iter().any(|finding| {
        finding.starts_with(
            "docs/rpc.md: RPC frame `tool_start` is in the code and missing from the events table",
        )
    }));
    assert_eq!(findings.len(), 8, "{findings:?}");
}

#[test]
fn flags_and_subcommands_are_checked_in_their_tables_and_wherever_a_command_line_is_shown() {
    let copy = Copy::new();
    copy.remove_lines("docs/configuration.md", "| `--continue-run` |");
    copy.add_after(
        "docs/configuration.md",
        "| `--trust` |",
        "| `--yolo` | Not a flag |",
    );
    copy.remove_lines("docs/configuration.md", "| `azazel doctor` |");
    copy.edit("docs/configuration.md", |text| {
        text.replace(
            "`list`, `search`, `delete`, `prune`, `export`",
            "`list`, `search`, `delete`, `archive`, `export`",
        )
    });
    // The only line that documents the flag of a subcommand, and command lines that are wrong.
    copy.edit("docs/sessions-and-checkpoints.md", |text| {
        text.replace("[--older-than 30d] ", "")
            + "\nRun `azazel sessions prune --force`, `azazel sessions tidy` or `azazel --colour never -p \"hi\"`.\n\n```sh\nAZAZEL_HOME=/tmp/h azazel mcp add files --transport stdio -- npx --yes server\n```\n"
    });
    let findings = copy.findings();
    let expected = [
        "docs/configuration.md: flag `--continue-run` is in the code and missing from the flags table",
        "docs/configuration.md: subcommand `azazel doctor` is in the code and missing from the subcommands table",
        "docs/configuration.md: action of `azazel sessions` `prune` is in the code and missing from the subcommands table",
        "docs: `azazel sessions prune --older-than` is not documented: no line of a page under docs/ names both `azazel sessions prune` and `--older-than`",
    ];
    for finding in expected {
        assert!(
            findings.iter().any(|found| found == finding),
            "{finding}\n{findings:?}"
        );
    }
    let has =
        |page: &str, text: &str| assert_eq!(about(&findings, page, text).len(), 1, "{text}\n{findings:?}");
    has(
        "docs/configuration.md",
        "flag `--yolo` is in the flags table and not in the code",
    );
    has(
        "docs/configuration.md",
        "action of `azazel sessions` `archive` is in the subcommands table and not in the code",
    );
    has(
        "docs/sessions-and-checkpoints.md",
        "`azazel sessions prune --force`: `azazel sessions prune` has no flag `--force`",
    );
    has(
        "docs/sessions-and-checkpoints.md",
        "`azazel sessions tidy`: `tidy` is not an action of `azazel sessions`",
    );
    has(
        "docs/sessions-and-checkpoints.md",
        "`azazel` has no flag `--colour`",
    );
    // What follows `--` belongs to another program; the flag before it is azazel's.
    has(
        "docs/sessions-and-checkpoints.md",
        "`azazel mcp add` has no flag `--transport`",
    );
    assert_eq!(findings.len(), 10, "{findings:?}");
}

#[test]
fn pages_must_be_linked_and_links_must_lead_somewhere() {
    let copy = Copy::new();
    std::fs::write(copy.dir.path().join("docs/orphan.md"), "# Orphan\n\nSee [tools](tools.md#no-such-heading), [the UI](tui.md#keys), [gone](gone.md) and [the web](https://example.com/x.md).\n").unwrap();
    let findings = copy.findings();
    assert_eq!(
        findings,
        [
            "docs/README.md: `orphan.md` is not linked from the index",
            "docs/orphan.md:3: the link `tools.md#no-such-heading` names a heading `#no-such-heading` that `docs/tools.md` does not have",
            "docs/orphan.md:3: the link `gone.md` leads to `docs/gone.md`, which does not exist",
        ]
    );
}

#[test]
fn the_schema_file_the_changelog_and_the_feature_stamp_follow_the_code() {
    let copy = Copy::new();
    copy.edit("docs/rpc.schema.json", |text| {
        text.replacen("prompt", "prompted", 1)
    });
    copy.edit("FEATURES.md", |text| {
        text.replace(
            &format!("azazel column as of {}", code().version),
            "azazel column as of 0.1.0",
        )
    });
    copy.edit("CLAUDE.md", |_| {
        "# CLAUDE.md\n\nRead the guide first.\n".to_string()
    });
    copy.edit("AGENTS.md", |text| text + "\nRun `cargo xtask polish` first.\n");
    let findings = copy.findings();
    assert_eq!(findings.len(), 4, "{findings:?}");
    assert_eq!(
        findings[0],
        "docs/rpc.schema.json: the file is not what the protocol generates; run `cargo xtask rpc-schema`"
    );
    assert!(findings[1].starts_with("FEATURES.md:"));
    assert!(findings[1].contains(&format!(
        "the azazel column is stamped 0.1.0 and the newest release in CHANGELOG.md is {}",
        code().version
    )));
    assert_eq!(findings[2], "CLAUDE.md: the page must point to AGENTS.md");
    assert!(findings[3].starts_with("AGENTS.md:"));
    assert!(findings[3].ends_with("`cargo xtask polish` is not a task of xtask"));

    // A version in Cargo.toml that the changelog has no section for.
    let mut next = code().clone();
    next.version = "99.0.0".to_string();
    assert_eq!(
        Copy::new().findings_for(&next),
        ["CHANGELOG.md: there is no section `## [99.0.0]` for the version in Cargo.toml"]
    );
}
