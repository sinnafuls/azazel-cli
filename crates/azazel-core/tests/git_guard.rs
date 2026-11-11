#![allow(clippy::unwrap_used)]

use std::path::Path;

use azazel_core::permissions::shell::guard_env;
use azazel_core::process::{RunOptions, Shell, run};

async fn sh(dir: &Path, script: &str, env: Vec<(String, String)>) -> (Option<i32>, String) {
    let mut options = RunOptions::new(dir.to_path_buf());
    options.env = env;
    let result = run(&Shell::resolve(""), script, options).await.unwrap();
    (result.code, result.output.render())
}

/// The environment layer of the push guard, on its own: command analysis is not involved here, so
/// this is what stops a push started by a script or a tool the agent runs.
#[tokio::test]
async fn a_push_started_with_the_guard_environment_fails_at_the_transport() {
    if which_git().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let setup =
        "git init -q . && git -c user.name=t -c user.email=t@example.invalid commit -q --allow-empty -m init";
    for remote in [
        "https://example.invalid/x.git",
        "git@example.invalid:team/x.git",
        "ssh://example.invalid/x.git",
    ] {
        let (code, out) = sh(dir.path(), setup, Vec::new()).await;
        assert_eq!(code, Some(0), "{out}");
        sh(
            dir.path(),
            &format!("git remote remove origin; git remote add origin {remote}"),
            Vec::new(),
        )
        .await;

        let env = guard_env(&|name| std::env::var(name).ok());
        let (code, out) = sh(dir.path(), "git push origin HEAD", env.clone()).await;
        assert_ne!(code, Some(0), "{remote}: push must fail\n{out}");
        assert!(out.contains("azazel-push-blocked"), "{remote}: {out}");

        // Fetch-side commands are not rewritten: the guard is about pushing only.
        let (_, out) = sh(dir.path(), "git remote get-url origin", env).await;
        assert_eq!(out.trim(), remote);
    }
}

fn which_git() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .flat_map(|dir| [dir.join("git"), dir.join("git.exe")])
            .find(|candidate| candidate.is_file())
    })
}
