//! The files that hold credentials: which paths they are, and which words of a command name one.

use std::path::Path;

use super::{MatchCtx, normal, path_matches};

/// Directories under the user's home whose every file is a secret.
const HOME_TREES: [&str; 4] = [".ssh", ".aws", ".config/gcloud", ".azure"];

/// Files under the user's home that are secrets.
const HOME_FILES: [&str; 8] = [
    ".kube/config",
    ".docker/config.json",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".azazel/auth.json",
    ".azazel/.env",
];

/// Names of `.env` files that hold placeholders, not secrets.
const ENV_SAMPLES: [&str; 3] = [".env.example", ".env.sample", ".env.template"];

/// Whether a file of this name is a secret wherever it lies: `.env`, `.env.*` (not the sample
/// files), `*.pem`, `*.key`, `id_rsa*`, `id_ed25519*`.
fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if ENV_SAMPLES.contains(&name.as_str()) {
        return false;
    }
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
}

fn same(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// `path` is `dir` or lies below it; both are normalised.
fn within(path: &str, dir: &str) -> bool {
    path.get(..dir.len()).is_some_and(|start| same(start, dir))
        && matches!(path.as_bytes().get(dir.len()), None | Some(b'/'))
}

/// What the built-in list and `permissions.secret_paths` make of an absolute path.
pub(super) fn is_secret(path: &Path, extra: &[String], azazel_home: &Path, ctx: &MatchCtx<'_>) -> bool {
    let text = normal(path);
    if secret_name(text.rsplit('/').next().unwrap_or_default()) {
        return true;
    }
    if let Some(home) = ctx.home {
        let home = normal(home);
        let below = |rest: &str| format!("{home}/{rest}");
        if HOME_TREES.iter().any(|dir| within(&text, &below(dir)))
            || HOME_FILES.iter().any(|file| same(&text, &below(file)))
        {
            return true;
        }
    }
    // The login file, wherever `AZAZEL_HOME` puts it.
    if same(&text, &format!("{}/auth.json", normal(azazel_home))) {
        return true;
    }
    extra.iter().any(|pattern| path_matches(pattern, &text, ctx))
}

/// The paths a word of a command line may stand for: the word, what follows `=` in an option
/// (`--data=@.env`) and what follows `@` (`curl -d @.env`, `scp host:…`), with the ways of
/// writing the home directory folded into `~`. Web addresses name no file.
pub(super) fn candidates(word: &str) -> Vec<String> {
    if word.contains("://") {
        return Vec::new();
    }
    let mut found = vec![word];
    if word.starts_with('-')
        && let Some((_, value)) = word.split_once('=')
    {
        found.push(value);
    }
    if let Some((_, file)) = word.split_once('@') {
        found.push(file);
    }
    found
        .into_iter()
        .filter(|text| !text.is_empty())
        .map(|text| {
            let home = ["$HOME", "${HOME}", "%USERPROFILE%", "$env:USERPROFILE"]
                .iter()
                .find_map(|name| text.strip_prefix(name))
                .filter(|rest| rest.is_empty() || rest.starts_with(['/', '\\']));
            match home {
                Some(rest) => format!("~/{}", rest.trim_start_matches(['/', '\\']).replace('\\', "/")),
                None => text.to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_are_secrets_anywhere() {
        for name in [
            ".env",
            ".env.local",
            ".env.production",
            ".ENV",
            "server.pem",
            "tls.key",
            "id_rsa",
            "id_rsa.pub",
            "id_ed25519",
            "id_ed25519_work",
        ] {
            assert!(secret_name(name), "{name}");
        }
        for name in [
            ".env.example",
            ".env.sample",
            ".env.template",
            ".envrc",
            "env",
            "prod.env",
            "keys.txt",
            "pem",
            "monkey",
            "README.md",
        ] {
            assert!(!secret_name(name), "{name}");
        }
    }

    #[test]
    fn words_of_a_command_that_may_be_files() {
        assert_eq!(candidates(".env"), [".env"]);
        assert_eq!(candidates("@.env"), ["@.env", ".env"]);
        assert_eq!(
            candidates("--data=@secrets/tls.key"),
            ["--data=@secrets/tls.key", "@secrets/tls.key", "secrets/tls.key"]
        );
        assert_eq!(
            candidates("me@host:.ssh/id_rsa"),
            ["me@host:.ssh/id_rsa", "host:.ssh/id_rsa"]
        );
        assert_eq!(candidates("$HOME/.ssh/config"), ["~/.ssh/config"]);
        assert_eq!(candidates("${HOME}/.netrc"), ["~/.netrc"]);
        assert_eq!(
            candidates("%USERPROFILE%\\.aws\\credentials"),
            ["~/.aws/credentials"]
        );
        assert_eq!(candidates("$HOMEWORK/.env"), ["$HOMEWORK/.env"]);
        assert!(candidates("https://example.com/ca.pem").is_empty());
    }
}
