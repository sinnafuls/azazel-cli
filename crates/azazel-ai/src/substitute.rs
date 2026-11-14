//! `{env:VAR}` and `{file:path}` in the string values of the configuration files (`config.toml`,
//! `models.yml`, `mcp.json`), filled once when a file is read.
//!
//! A `{file:}` path is relative to the directory of the file that names it and `~/` is the home
//! directory; the line breaks that end the file are dropped. A variable that is not set, or a file
//! that cannot be read, is an error that names the key, never an empty value. What is filled in
//! is never written to a log or into an error.

use std::path::{Path, PathBuf};

const ENV: &str = "{env:";
const FILE: &str = "{file:";

/// Whether `text` holds a `{env:}` or `{file:}` reference.
pub fn mentions(text: &str) -> bool {
    text.contains(ENV) || text.contains(FILE)
}

/// Fills the references in the values of one file.
pub struct Substitute<'a> {
    dir: PathBuf,
    home: Option<PathBuf>,
    env: &'a dyn Fn(&str) -> Option<String>,
}

impl<'a> Substitute<'a> {
    /// For the values of `file`, with variables looked up in `env`.
    pub fn new(file: &Path, env: &'a dyn Fn(&str) -> Option<String>) -> Self {
        Substitute {
            dir: file.parent().map(Path::to_path_buf).unwrap_or_default(),
            home: std::env::home_dir(),
            env,
        }
    }

    /// With `home` as the directory `~/` stands for.
    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    /// `written` as a path: `~/` is the home directory, and a relative path is relative to the
    /// directory of the file.
    pub fn path(&self, written: &str) -> PathBuf {
        let written = written.trim();
        let under_home = written
            .strip_prefix("~/")
            .or_else(|| written.strip_prefix("~\\"))
            .or_else(|| (written == "~").then_some(""));
        match (under_home, &self.home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => self.dir.join(written),
        }
    }

    /// `text` with its references filled, or `None` when it has none. `key` names the value in
    /// an error (`providers.gateway.api_key`).
    pub fn text(&self, text: &str, key: &str) -> Result<Option<String>, String> {
        if !mentions(text) {
            return Ok(None);
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some((start, kind)) = next_reference(rest) {
            out.push_str(&rest[..start]);
            let after = &rest[start + kind.len()..];
            let Some(end) = after.find('}') else {
                rest = &rest[start..];
                break;
            };
            let name = after[..end].trim();
            if name.is_empty() {
                return Err(format!("{key}: `{kind}}}` names nothing"));
            }
            // What is filled in is not looked at again: a file may hold braces of its own.
            if kind == ENV {
                let value = (self.env)(name).ok_or_else(|| format!("{key}: {{env:{name}}} is not set"))?;
                out.push_str(&value);
            } else {
                let path = self.path(name);
                let value = std::fs::read_to_string(&path).map_err(|e| {
                    format!("{key}: {{file:{name}}} cannot be read ({}): {e}", path.display())
                })?;
                out.push_str(value.trim_end_matches(['\r', '\n']));
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(Some(out))
    }

    /// Fills every string of a JSON document; `at` is the key of `value`.
    pub fn json(&self, value: &mut serde_json::Value, at: &str) -> Result<(), String> {
        use serde_json::Value;
        match value {
            Value::String(text) => {
                if let Some(filled) = self.text(text, at)? {
                    *text = filled;
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter_mut().enumerate() {
                    self.json(item, &format!("{at}[{index}]"))?;
                }
            }
            Value::Object(entries) => {
                for (key, item) in entries.iter_mut() {
                    self.json(item, &join(at, key))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Fills every string of a YAML document; `at` is the key of `value`.
    pub fn yaml(&self, value: &mut serde_yaml_ng::Value, at: &str) -> Result<(), String> {
        use serde_yaml_ng::Value;
        match value {
            Value::String(text) => {
                if let Some(filled) = self.text(text, at)? {
                    *text = filled;
                }
            }
            Value::Sequence(items) => {
                for (index, item) in items.iter_mut().enumerate() {
                    self.yaml(item, &format!("{at}[{index}]"))?;
                }
            }
            Value::Mapping(entries) => {
                for (key, item) in entries.iter_mut() {
                    let name = match key {
                        Value::String(name) => name.clone(),
                        other => serde_yaml_ng::to_string(other)
                            .map(|text| text.trim().to_string())
                            .unwrap_or_default(),
                    };
                    self.yaml(item, &join(at, &name))?;
                }
            }
            Value::Tagged(tagged) => self.yaml(&mut tagged.value, at)?,
            _ => {}
        }
        Ok(())
    }
}

/// `at.key`, or `key` at the top.
pub fn join(at: &str, key: &str) -> String {
    match at.is_empty() {
        true => key.to_string(),
        false => format!("{at}.{key}"),
    }
}

/// Where the first reference in `text` begins, and which kind it is.
fn next_reference(text: &str) -> Option<(usize, &'static str)> {
    [ENV, FILE]
        .into_iter()
        .filter_map(|kind| text.find(kind).map(|at| (at, kind)))
        .min_by_key(|(at, _)| *at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("s3cret".to_string()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn variables_and_files_are_filled_and_other_braces_stay() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("keys")).unwrap();
        std::fs::write(dir.path().join("keys").join("api.txt"), "from-file\r\n\n").unwrap();
        std::fs::write(dir.path().join("braces.txt"), "{env:TOKEN}").unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("h.txt"), "at-home\n").unwrap();
        let fill = Substitute::new(&dir.path().join("config.toml"), &env).with_home(Some(home));

        let text = |value: &str| fill.text(value, "k").unwrap();
        assert_eq!(text("plain ${VAR} {session_id} {field}"), None);
        assert_eq!(text("Bearer {env:TOKEN}").as_deref(), Some("Bearer s3cret"));
        assert_eq!(text("[{env:EMPTY}]").as_deref(), Some("[]"));
        assert_eq!(text("{file:keys/api.txt}").as_deref(), Some("from-file"));
        assert_eq!(text("{file:~/h.txt}+{env:TOKEN}").as_deref(), Some("at-home+s3cret"));
        // A file's own braces are its content, not references.
        assert_eq!(text("{file:braces.txt}").as_deref(), Some("{env:TOKEN}"));
        // An opening without its close is left as written.
        assert_eq!(text("{env:TOKEN").as_deref(), Some("{env:TOKEN"));
    }

    #[test]
    fn a_missing_variable_or_file_is_an_error_naming_the_key_without_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let fill = Substitute::new(&dir.path().join("config.toml"), &env);

        let error = fill.text("{env:TOKEN} {env:MISSING}", "providers.gw.api_key").unwrap_err();
        assert!(error.starts_with("providers.gw.api_key: {env:MISSING}"), "{error}");
        assert!(!error.contains("s3cret"), "{error}");
        let error = fill.text("{file:absent.pem}", "providers.gw.tls.cert").unwrap_err();
        assert!(error.starts_with("providers.gw.tls.cert: {file:absent.pem}"), "{error}");
        assert!(fill.text("{env:}", "k").is_err());
    }

    #[test]
    fn documents_are_filled_everywhere_and_errors_name_the_nested_key() {
        let dir = tempfile::tempdir().unwrap();
        let fill = Substitute::new(&dir.path().join("mcp.json"), &env);
        let mut json = serde_json::json!({ "a": { "list": ["x", "{env:TOKEN}"], "n": 3 } });
        fill.json(&mut json, "").unwrap();
        assert_eq!(json, serde_json::json!({ "a": { "list": ["x", "s3cret"], "n": 3 } }));
        let mut json = serde_json::json!({ "a": { "list": ["{env:NOPE}"] } });
        let error = fill.json(&mut json, "root").unwrap_err();
        assert!(error.starts_with("root.a.list[0]: "), "{error}");

        let mut yaml: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("p:\n  headers: { X-Key: '{env:TOKEN}' }\n").unwrap();
        fill.yaml(&mut yaml, "").unwrap();
        assert_eq!(yaml["p"]["headers"]["X-Key"].as_str(), Some("s3cret"));
    }
}
