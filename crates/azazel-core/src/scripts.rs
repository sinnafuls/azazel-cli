//! Saved Luau scripts: the user keeps the source of a script that worked, and later `eval` scripts
//! run it with `tools.run(name, input)`. One file per script, `<name>.luau`, with a frontmatter
//! header, in `~/.azazel/scripts/` and (in a trusted project) `.azazel/scripts/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use azazel_ai::{Block, Message, Role};
use serde_json::{Map, Value};

use crate::ext::{Command, CommandHandler, CommandOutcome, ExtCtx, ExtError, Extension, Registrar};
use crate::frontmatter;
use crate::paths::Paths;
use crate::session::{Entry, EntryKind, SessionStore};

/// The id of the extension that registers the `/script` command and the [`Scripts`] service.
pub const OWNER: &str = "scripts";
/// The extension of a saved script's file.
const EXTENSION: &str = "luau";
/// The longest name a script may have.
const MAX_NAME: usize = 64;
/// The tool whose calls are saved.
const EVAL: &str = "eval";

/// Whether `name` may name a saved script: `[a-z0-9-]+`, at most 64 characters, not starting with
/// `-` (a command line would read that as a flag).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Where a saved script lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    User,
    Project,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::User => "user",
            Origin::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedScript {
    pub name: String,
    pub description: String,
    /// When it was saved, as written in the header.
    pub saved_at: Option<String>,
    /// What `input` is expected to hold, from the script's `-- input:` comment.
    pub input: Option<String>,
    pub source: String,
    pub origin: Origin,
    pub path: PathBuf,
}

/// The first `-- input: ...` comment line of a script.
fn input_notes(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        let note = line
            .trim()
            .strip_prefix("--")?
            .trim()
            .strip_prefix("input:")?
            .trim();
        (!note.is_empty()).then(|| note.to_string())
    })
}

fn render(description: &str, saved_at: &str, source: &str) -> Result<String, String> {
    let mut header = Map::new();
    header.insert("description".into(), Value::String(description.to_string()));
    header.insert("saved-at".into(), Value::String(saved_at.to_string()));
    if let Some(notes) = input_notes(source) {
        header.insert("input".into(), Value::String(notes));
    }
    let yaml = serde_yaml_ng::to_string(&header).map_err(|e| e.to_string())?;
    let newline = if source.ends_with('\n') { "" } else { "\n" };
    Ok(format!("---\n{yaml}---\n{source}{newline}"))
}

fn parse(name: &str, origin: Origin, path: &Path, text: &str) -> SavedScript {
    let (meta, source) = match frontmatter::split(text) {
        Some((yaml, body)) => (frontmatter::parse(yaml).unwrap_or_default(), body),
        None => (Map::new(), text),
    };
    let field = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    SavedScript {
        name: name.to_string(),
        description: field("description").unwrap_or_default(),
        saved_at: field("saved-at"),
        input: field("input"),
        source: source.to_string(),
        origin,
        path: path.to_path_buf(),
    }
}

fn read_dir(dir: &Path, origin: Origin) -> Vec<SavedScript> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != EXTENSION) || !path.is_file() {
            continue;
        }
        let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if !valid_name(&name) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            found.push(parse(&name, origin, &path, &text));
        }
    }
    found
}

/// The saved scripts of this user and, when the project is trusted, of this project.
#[derive(Debug, Clone)]
pub struct Scripts {
    paths: Paths,
    trusted: bool,
}

impl Scripts {
    pub fn new(paths: Paths, trusted: bool) -> Scripts {
        Scripts { paths, trusted }
    }

    fn user_dir(&self) -> PathBuf {
        self.paths.home.join("scripts")
    }

    fn project_dir(&self) -> PathBuf {
        self.paths.project_dir().join("scripts")
    }

    /// Every script by name; a project script replaces a user script of the same name. A
    /// project that is not trusted contributes none.
    pub fn list(&self) -> Vec<SavedScript> {
        let mut scripts: HashMap<String, SavedScript> = HashMap::new();
        let mut sources = vec![(self.user_dir(), Origin::User)];
        if self.trusted {
            sources.push((self.project_dir(), Origin::Project));
        }
        for (dir, origin) in sources {
            for script in read_dir(&dir, origin) {
                scripts.insert(script.name.clone(), script);
            }
        }
        let mut scripts: Vec<SavedScript> = scripts.into_values().collect();
        scripts.sort_by(|a, b| a.name.cmp(&b.name));
        scripts
    }

    pub fn find(&self, name: &str) -> Option<SavedScript> {
        if !valid_name(name) {
            return None;
        }
        self.list().into_iter().find(|script| script.name == name)
    }

    /// Writes `source` as the user's script `name`, replacing one of that name. Returns the file.
    pub fn save(&self, name: &str, description: &str, source: &str) -> Result<PathBuf, String> {
        if !valid_name(name) {
            return Err(format!(
                "`{name}` is not a script name; use lowercase letters, digits and `-` (at most {MAX_NAME}, not starting with `-`)"
            ));
        }
        let text = render(description.trim(), &jiff::Timestamp::now().to_string(), source)?;
        let dir = self.user_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(format!("{name}.{EXTENSION}"));
        let temp = dir.join(format!(".{name}.{EXTENSION}.tmp"));
        std::fs::write(&temp, text)
            .and_then(|()| std::fs::rename(&temp, &path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&temp);
                format!("{}: {e}", path.display())
            })?;
        Ok(path)
    }

    /// Deletes the script `name` that [`Scripts::find`] resolves.
    pub fn delete(&self, name: &str) -> Result<SavedScript, String> {
        let script = self
            .find(name)
            .ok_or_else(|| format!("there is no saved script `{name}`"))?;
        std::fs::remove_file(&script.path).map_err(|e| format!("{}: {e}", script.path.display()))?;
        Ok(script)
    }
}

/// The scripts as `name  (user|project)  description`, one per line.
pub fn list_text(scripts: &[SavedScript]) -> String {
    if scripts.is_empty() {
        return "no saved scripts; run a Luau script with `eval`, then `/script save <name> [description]`"
            .into();
    }
    scripts
        .iter()
        .map(|s| {
            let description = if s.description.is_empty() {
                String::new()
            } else {
                format!("  {}", s.description)
            };
            format!("{}  ({}){description}", s.name, s.origin.label())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One script: its header lines, then its source.
pub fn show_text(script: &SavedScript) -> String {
    let mut out = format!(
        "{} ({}, {})\n",
        script.name,
        script.origin.label(),
        script.path.display()
    );
    if !script.description.is_empty() {
        out.push_str(&format!("description: {}\n", script.description));
    }
    if let Some(at) = &script.saved_at {
        out.push_str(&format!("saved: {at}\n"));
    }
    if let Some(input) = &script.input {
        out.push_str(&format!("input: {input}\n"));
    }
    out.push('\n');
    out.push_str(script.source.trim_end());
    out
}

/// The source of the newest `eval` call in `branch` that ran Luau in the foreground and
/// succeeded.
pub fn last_luau_eval(branch: &[Entry]) -> Option<String> {
    let mut failed: HashMap<&str, bool> = HashMap::new();
    for entry in branch {
        if let EntryKind::Message {
            message:
                Message {
                    role: Role::Tool,
                    content,
                    ..
                },
        } = &entry.kind
        {
            for block in content {
                if let Block::ToolResult {
                    call_id, is_error, ..
                } = block
                {
                    failed.insert(call_id, *is_error);
                }
            }
        }
    }
    branch.iter().rev().find_map(|entry| {
        let EntryKind::Message { message } = &entry.kind else {
            return None;
        };
        message.content.iter().rev().find_map(|block| {
            let Block::ToolCall { id, name, input, .. } = block else {
                return None;
            };
            let luau = input.get("language").and_then(Value::as_str) == Some("luau");
            let background = input.get("background").and_then(Value::as_bool) == Some(true);
            (name == EVAL && luau && !background && failed.get(id.as_str()) == Some(&false))
                .then(|| input.get("code").and_then(Value::as_str).map(str::to_string))
                .flatten()
        })
    })
}

const USAGE: &str = "usage: /script save <name> [description] | list | show <name> | delete <name>";

struct ScriptCommand {
    scripts: Arc<Scripts>,
}

#[async_trait]
impl CommandHandler for ScriptCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let args = args.trim();
        let (action, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let rest = rest.trim();
        let (name, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        match action {
            "list" => CommandOutcome::Show(list_text(&self.scripts.list())),
            "show" if !name.is_empty() => match self.scripts.find(name) {
                Some(script) => CommandOutcome::Show(show_text(&script)),
                None => CommandOutcome::Error(format!("there is no saved script `{name}`")),
            },
            "delete" if !name.is_empty() => match self.scripts.delete(name) {
                Ok(script) => CommandOutcome::Show(format!("deleted {}", script.path.display())),
                Err(e) => CommandOutcome::Error(e),
            },
            "save" if !name.is_empty() => {
                let Some(session) = ctx.services.get::<SessionStore>() else {
                    return CommandOutcome::Error("no session is open".into());
                };
                let Some(source) = last_luau_eval(&session.branch(None)) else {
                    return CommandOutcome::Error(
                        "this session has no successful Luau `eval` to save; run one first".into(),
                    );
                };
                match self.scripts.save(name, tail, &source) {
                    Ok(path) => CommandOutcome::Show(format!(
                        "saved the last Luau eval as `{name}` ({}); scripts call it with tools.run(\"{name}\", input)",
                        path.display()
                    )),
                    Err(e) => CommandOutcome::Error(e),
                }
            }
            _ => CommandOutcome::Error(USAGE.into()),
        }
    }
}

/// Registers [`Scripts`] as a service and the `/script` command.
pub struct ScriptsExt {
    pub scripts: Arc<Scripts>,
}

impl Extension for ScriptsExt {
    fn id(&self) -> &str {
        OWNER
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.service(self.scripts.clone());
        r.command(Command {
            name: "script".into(),
            description: "Save the last Luau eval as a script that later scripts run with tools.run".into(),
            argument_hint: Some("save <name> [description] | list | show <name> | delete <name>".into()),
            handler: Arc::new(ScriptCommand {
                scripts: self.scripts.clone(),
            }),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scripts_in(root: &Path, trusted: bool) -> Scripts {
        let (home, project) = (root.join("home"), root.join("project"));
        std::fs::create_dir_all(project.join(".azazel")).unwrap();
        let paths = Paths::discover(&project, Some(home.to_str().unwrap()));
        Scripts::new(paths, trusted)
    }

    fn write(dir: &Path, file: &str, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), text).unwrap();
    }

    #[test]
    fn names_are_lowercase_words_joined_by_dashes() {
        for good in ["a", "list-files", "v2", "0-1"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "Upper",
            "a_b",
            "a b",
            "a/b",
            "..",
            "a.luau",
            &"x".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_saved_script_round_trips_and_keeps_its_input_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = scripts_in(tmp.path(), true);
        let source = "-- input: { dir: string }\nreturn input.dir";
        let path = scripts.save("dirs", "List: the \"dirs\"", source).unwrap();
        assert!(path.ends_with("dirs.luau"));
        let found = scripts.find("dirs").unwrap();
        assert_eq!(found.description, "List: the \"dirs\"");
        assert_eq!(found.input.as_deref(), Some("{ dir: string }"));
        assert_eq!(found.source.trim_end(), source);
        assert!(found.saved_at.is_some() && found.origin == Origin::User);
        assert!(scripts.save("Bad Name", "", source).is_err());
        assert_eq!(scripts.delete("dirs").unwrap().name, "dirs");
        assert!(scripts.find("dirs").is_none() && scripts.delete("dirs").is_err());
    }

    #[test]
    fn project_scripts_replace_user_scripts_and_load_only_in_a_trusted_project() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, project) = (
            tmp.path().join("home/scripts"),
            tmp.path().join("project/.azazel/scripts"),
        );
        write(&home, "both.luau", "---\ndescription: user\n---\nreturn 1");
        write(&home, "mine.luau", "return 2");
        write(&home, "Not Valid.luau", "return 3");
        write(&home, "notes.txt", "x");
        write(&project, "both.luau", "---\ndescription: project\n---\nreturn 4");
        write(&project, "shared.luau", "return 5");

        let trusted = scripts_in(tmp.path(), true).list();
        let names: Vec<_> = trusted.iter().map(|s| (s.name.as_str(), s.origin)).collect();
        assert_eq!(
            names,
            [
                ("both", Origin::Project),
                ("mine", Origin::User),
                ("shared", Origin::Project)
            ]
        );
        assert_eq!(trusted[0].description, "project");

        let untrusted = scripts_in(tmp.path(), false);
        let names: Vec<_> = untrusted.list().into_iter().map(|s| (s.name, s.origin)).collect();
        assert_eq!(
            names,
            [
                ("both".to_string(), Origin::User),
                ("mine".to_string(), Origin::User)
            ]
        );
        assert!(untrusted.find("shared").is_none());
    }

    fn entry(kind: EntryKind) -> Entry {
        Entry {
            id: "e".into(),
            parent_id: None,
            timestamp: String::new(),
            kind,
        }
    }

    fn call(id: &str, input: Value) -> Entry {
        let block = Block::ToolCall {
            id: id.into(),
            name: "eval".into(),
            input,
            signature: None,
        };
        entry(EntryKind::Message {
            message: Message {
                role: Role::Assistant,
                content: vec![block],
                meta: None,
            },
        })
    }

    fn result(id: &str, is_error: bool) -> Entry {
        entry(EntryKind::Message {
            message: Message::tool(vec![Block::tool_result(id, "out", is_error)]),
        })
    }

    #[test]
    fn the_last_successful_foreground_luau_eval_is_the_one_saved() {
        let luau = |code: &str| json!({ "language": "luau", "code": code });
        let branch = vec![
            call("a", luau("return 'first'")),
            result("a", false),
            call("b", json!({ "language": "python", "code": "print(1)" })),
            result("b", false),
            call("c", luau("return 'failed'")),
            result("c", true),
            call(
                "d",
                json!({ "language": "luau", "code": "return 'bg'", "background": true }),
            ),
            result("d", false),
            call("e", luau("return 'unanswered'")),
        ];
        assert_eq!(last_luau_eval(&branch).as_deref(), Some("return 'first'"));
        assert_eq!(last_luau_eval(&branch[2..]), None);
    }
}
