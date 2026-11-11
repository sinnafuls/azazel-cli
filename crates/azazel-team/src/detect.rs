//! Working out a project's build and test commands from the files at its root.

use std::path::Path;

use serde_json::Value;

/// Scripts of a `package.json` that count as checks, in the order they run.
const NODE_SCRIPTS: [&str; 4] = ["typecheck", "lint", "build", "test"];

fn node_commands(root: &Path) -> Vec<String> {
    let Some(manifest) = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    else {
        return Vec::new();
    };
    let has = |file: &str| root.join(file).is_file();
    let manager = if has("pnpm-lock.yaml") {
        "pnpm"
    } else if has("yarn.lock") {
        "yarn"
    } else if has("bun.lock") || has("bun.lockb") {
        "bun"
    } else {
        "npm"
    };
    NODE_SCRIPTS
        .iter()
        .filter(|script| {
            manifest
                .get("scripts")
                .and_then(|scripts| scripts.get(**script))
                .is_some()
        })
        .map(|script| format!("{manager} run {script}"))
        .collect()
}

fn has_entry(dir: &Path, matches: impl Fn(&str) -> bool) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.file_name().to_str().is_some_and(&matches))
}

/// The commands that build and test the project at `root`, for every ecosystem found there:
/// Rust, Node, Go, Python, .NET, then a Makefile with a `test` target. Empty when nothing is
/// recognised.
pub fn verify_commands(root: &Path) -> Vec<String> {
    let has = |file: &str| root.join(file).is_file();
    let mut commands: Vec<String> = Vec::new();
    let mut add = |list: &[&str]| commands.extend(list.iter().map(|command| command.to_string()));
    if has("Cargo.toml") {
        add(&["cargo build --all-targets", "cargo test"]);
    }
    if has("package.json") {
        let node = node_commands(root);
        add(&node.iter().map(String::as_str).collect::<Vec<_>>());
    }
    if has("go.mod") {
        add(&["go build ./...", "go vet ./...", "go test ./..."]);
    }
    let python_tests = has_entry(&root.join("tests"), |name| {
        name.starts_with("test_") && name.ends_with(".py")
    });
    if has("pyproject.toml") || has("pytest.ini") || python_tests {
        add(&["python -m pytest -q"]);
    }
    if has_entry(root, |name| name.ends_with(".sln") || name.ends_with(".csproj")) {
        add(&["dotnet build", "dotnet test"]);
    }
    let make_test = std::fs::read_to_string(root.join("Makefile"))
        .is_ok_and(|makefile| makefile.lines().any(|line| line.starts_with("test:")));
    if make_test {
        add(&["make test"]);
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    #[test]
    fn each_ecosystem_brings_its_commands() {
        assert_eq!(
            verify_commands(project(&[("Cargo.toml", "")]).path()),
            ["cargo build --all-targets", "cargo test"]
        );
        assert_eq!(
            verify_commands(project(&[("go.mod", "")]).path()),
            ["go build ./...", "go vet ./...", "go test ./..."]
        );
        for marker in ["pyproject.toml", "pytest.ini", "tests/test_api.py"] {
            assert_eq!(
                verify_commands(project(&[(marker, "")]).path()),
                ["python -m pytest -q"],
                "{marker}"
            );
        }
        for marker in ["App.sln", "src.csproj"] {
            assert_eq!(
                verify_commands(project(&[(marker, "")]).path()),
                ["dotnet build", "dotnet test"],
                "{marker}"
            );
        }
        assert_eq!(
            verify_commands(project(&[("Makefile", "build:\n\tcc x.c\ntest: build\n\t./x\n")]).path()),
            ["make test"]
        );
        // A Makefile without a test target, and files that only look similar, bring nothing.
        assert!(
            verify_commands(project(&[("Makefile", "build:\n\tcc x.c\n"), ("tests/helper.py", "")]).path())
                .is_empty()
        );
        assert!(verify_commands(project(&[("README.md", "")]).path()).is_empty());
    }

    #[test]
    fn node_uses_the_lock_files_manager_and_only_the_scripts_that_exist() {
        let manifest = r#"{ "scripts": { "test": "vitest", "build": "tsc", "start": "node ." } }"#;
        assert_eq!(
            verify_commands(project(&[("package.json", manifest)]).path()),
            ["npm run build", "npm run test"]
        );
        let all = r#"{ "scripts": { "test": "x", "lint": "x", "typecheck": "x", "build": "x" } }"#;
        for (lock, manager) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lock", "bun"),
            ("bun.lockb", "bun"),
        ] {
            let commands = verify_commands(project(&[("package.json", all), (lock, "")]).path());
            let expected: Vec<String> = ["typecheck", "lint", "build", "test"]
                .iter()
                .map(|script| format!("{manager} run {script}"))
                .collect();
            assert_eq!(commands, expected);
        }
        assert!(verify_commands(project(&[("package.json", "{}")]).path()).is_empty());
        assert!(verify_commands(project(&[("package.json", "not json")]).path()).is_empty());
    }

    #[test]
    fn a_project_of_several_ecosystems_gets_all_of_them_in_order() {
        let dir = project(&[
            ("Makefile", "test:\n\ttrue\n"),
            ("go.mod", ""),
            ("package.json", r#"{ "scripts": { "test": "x" } }"#),
            ("Cargo.toml", ""),
        ]);
        assert_eq!(
            verify_commands(dir.path()),
            [
                "cargo build --all-targets",
                "cargo test",
                "npm run test",
                "go build ./...",
                "go vet ./...",
                "go test ./...",
                "make test"
            ]
        );
    }
}
