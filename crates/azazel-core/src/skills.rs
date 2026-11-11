//! Skills: instruction packs discovered on disk, listed in the prompt by name and description, and
//! loaded in full only when used.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Tool, ToolCtx, ToolOutput};
use azazel_ai::{Block, ModelSpec};
use serde_json::{Value, json};

use crate::frontmatter;
use crate::paths::Paths;
use crate::prompt::SystemPartProvider;
use crate::scheme::Scheme;
use crate::subagent::Subagents;
use crate::trust::project_skill_roots;

pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Above this size the prompt listing shortens every description.
const MAX_LISTING_CHARS: usize = 8000;
const SHORT_DESCRIPTION_CHARS: usize = 200;
const MAX_REFERENCE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    Builtin,
    /// `~/.claude/skills`, `~/.agents/skills` and their project counterparts.
    Compat,
    Home,
    Package(String),
    Project,
}

impl SkillSource {
    pub fn label(&self) -> String {
        match self {
            SkillSource::Builtin => "built-in".to_string(),
            SkillSource::Compat => "compat".to_string(),
            SkillSource::Home => "home".to_string(),
            SkillSource::Package(name) => format!("package {name}"),
            SkillSource::Project => "project".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillContext {
    /// The body is returned to the calling agent.
    Inline,
    /// The body is run by a sub-agent and its report returned.
    Fork,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    /// Listed in the system prompt so the model can pick it.
    pub model_invocable: bool,
    /// Available as `/skill:<name>`.
    pub user_invocable: bool,
    pub context: SkillContext,
    /// Agent definition used for `context: fork`.
    pub agent: Option<String>,
    pub model: Option<Vec<ModelSpec>>,
    pub allowed_tools: Option<Vec<String>>,
    /// Raw `hooks` frontmatter.
    pub hooks: Option<Value>,
    /// Directory holding `SKILL.md`; relative references in the body resolve against it.
    pub dir: PathBuf,
    pub body: String,
    pub source: SkillSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSkill {
    pub path: PathBuf,
    pub error: String,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name.split('-').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// Parses one `SKILL.md`. The frontmatter must be strict YAML; `name` is kebab-case of at most 64
/// characters and `description` is non-empty and at most 1024 characters.
pub fn parse_skill(file: &Path, text: &str, source: SkillSource) -> Result<Skill, String> {
    let (yaml, body) =
        frontmatter::split(text).ok_or("missing frontmatter: the file must start with `---`")?;
    let meta = frontmatter::parse(yaml)?;
    let text_of = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let flag = |key: &str, default: bool| meta.get(key).and_then(Value::as_bool).unwrap_or(default);

    let name = text_of("name").ok_or("`name` is missing")?;
    if !valid_name(&name) {
        return Err(format!(
            "`name` must be lowercase words joined by hyphens, at most {MAX_NAME_CHARS} characters"
        ));
    }
    let description = text_of("description").ok_or("`description` is missing or empty")?;
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "`description` is longer than {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    let model = frontmatter::string_list(meta.get("model"))
        .map(|specs| {
            specs
                .iter()
                .filter_map(|s| s.parse::<ModelSpec>().ok())
                .collect::<Vec<_>>()
        })
        .filter(|chain| !chain.is_empty());
    Ok(Skill {
        name,
        description,
        when_to_use: text_of("when_to_use"),
        argument_hint: text_of("argument-hint"),
        model_invocable: !flag("disable-model-invocation", false),
        user_invocable: flag("user-invocable", true),
        context: if text_of("context").as_deref() == Some("fork") {
            SkillContext::Fork
        } else {
            SkillContext::Inline
        },
        agent: text_of("agent"),
        model,
        allowed_tools: frontmatter::string_or_list(meta.get("allowed-tools")),
        hooks: meta.get("hooks").cloned(),
        dir: file.parent().map(Path::to_path_buf).unwrap_or_default(),
        body: body.trim_start_matches(['\r', '\n']).to_string(),
        source,
    })
}

/// A directory whose sub-directories hold `SKILL.md` files, or (`single`) one such directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub source: SkillSource,
    /// When set, only these skills of the root are offered to the model; the rest stay available
    /// to the user as `/skill:<name>`.
    pub model_invocable: Option<Vec<String>>,
    /// `path` is itself a skill's directory.
    pub single: bool,
    /// Whether the hooks its skills carry are kept. Off for packages whose code is not enabled.
    pub hooks: bool,
}

impl SkillRoot {
    pub fn new(path: PathBuf, source: SkillSource) -> Self {
        SkillRoot {
            path,
            source,
            model_invocable: None,
            single: false,
            hooks: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillSet {
    pub skills: BTreeMap<String, Skill>,
    pub invalid: Vec<InvalidSkill>,
}

/// Reads every root in order; a later root replaces an earlier skill of the same name.
pub fn discover(roots: &[SkillRoot]) -> SkillSet {
    let mut set = SkillSet::default();
    for root in roots {
        let mut dirs: Vec<PathBuf> = if root.single {
            vec![root.path.clone()]
        } else {
            std::fs::read_dir(&root.path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .collect()
        };
        dirs.sort();
        for dir in dirs {
            let file = dir.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            match parse_skill(&file, &text, root.source.clone()) {
                Ok(mut skill) => {
                    if let Some(allowed) = &root.model_invocable {
                        skill.model_invocable &= allowed.contains(&skill.name);
                    }
                    if !root.hooks {
                        skill.hooks = None;
                    }
                    set.skills.insert(skill.name.clone(), skill);
                }
                Err(error) => set.invalid.push(InvalidSkill { path: file, error }),
            }
        }
    }
    set
}

/// Skill roots, lowest precedence first: the built-in skills, then those of installed packages,
/// the user's and the project's.
pub fn standard_roots(
    paths: &Paths,
    claude_compat: bool,
    packages: &crate::packages::Loaded,
) -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    match crate::builtin::skill_root(&paths.cache_dir()) {
        Ok(root) => roots.push(SkillRoot::new(root, SkillSource::Builtin)),
        Err(e) => tracing::warn!("built-in skills are unavailable: {e}"),
    }
    roots.extend(packages.skill_roots.iter().cloned());
    if claude_compat && let Some(home) = std::env::home_dir() {
        roots.push(SkillRoot::new(
            home.join(".claude").join("skills"),
            SkillSource::Compat,
        ));
        roots.push(SkillRoot::new(
            home.join(".agents").join("skills"),
            SkillSource::Compat,
        ));
    }
    roots.push(SkillRoot::new(paths.home.join("skills"), SkillSource::Home));
    let project = project_skill_roots(paths, claude_compat);
    let last = project.len().saturating_sub(1);
    for (i, path) in project.into_iter().enumerate() {
        let source = if i == last {
            SkillSource::Project
        } else {
            SkillSource::Compat
        };
        roots.push(SkillRoot::new(path, source));
    }
    roots
}

/// What an agent receives when a skill is loaded: where it lives, then its body with `$ARGUMENTS`
/// filled in.
pub fn invocation_text(skill: &Skill, args: &str) -> String {
    let args = args.trim();
    let mut body = skill.body.replace("$ARGUMENTS", args);
    if !args.is_empty() && !skill.body.contains("$ARGUMENTS") {
        body.push_str(&format!("\n\nArguments: {args}"));
    }
    format!("Skill directory: {}\n\n{body}", skill.dir.display())
}

type InvokeWatcher = Arc<dyn Fn(&Skill) + Send + Sync>;

/// The session's skills. Registered as a service.
#[derive(Default)]
pub struct SkillRegistry {
    roots: RwLock<Vec<SkillRoot>>,
    set: RwLock<SkillSet>,
    invoked: Mutex<Vec<String>>,
    watchers: RwLock<Vec<InvokeWatcher>>,
}

impl SkillRegistry {
    /// Adds a root (higher precedence than those already present) and re-reads everything.
    pub fn add_root(&self, root: SkillRoot) {
        self.roots
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(root);
        self.reload();
    }

    /// Replaces all roots and re-reads everything.
    pub fn set_roots(&self, roots: Vec<SkillRoot>) {
        *self.roots.write().unwrap_or_else(PoisonError::into_inner) = roots;
        self.reload();
    }

    pub fn reload(&self) {
        let roots = self.roots.read().unwrap_or_else(PoisonError::into_inner).clone();
        *self.set.write().unwrap_or_else(PoisonError::into_inner) = discover(&roots);
    }

    pub fn get(&self, name: &str) -> Option<Skill> {
        self.set
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .skills
            .get(name)
            .cloned()
    }

    /// Valid skills, sorted by name.
    pub fn list(&self) -> Vec<Skill> {
        self.set
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .skills
            .values()
            .cloned()
            .collect()
    }

    pub fn invalid(&self) -> Vec<InvalidSkill> {
        self.set
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .invalid
            .clone()
    }

    /// `watcher` runs the first time each skill is used in the session.
    pub fn on_first_use(&self, watcher: InvokeWatcher) {
        self.watchers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(watcher);
    }

    /// Forgets which skills were used, so the next use of each counts as its first again. Call
    /// when a session ends.
    pub fn reset_used(&self) {
        self.invoked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Notes that `skill` was used; tells the watchers when it is the first time.
    pub fn mark_used(&self, skill: &Skill) {
        {
            let mut invoked = self.invoked.lock().unwrap_or_else(PoisonError::into_inner);
            if invoked.contains(&skill.name) {
                return;
            }
            invoked.push(skill.name.clone());
        }
        let watchers = self
            .watchers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for watcher in watchers {
            watcher(skill);
        }
    }

    /// The `skills` part of the system prompt: one line per model-invocable skill, by name.
    pub fn listing(&self) -> Option<String> {
        let skills: Vec<Skill> = self.list().into_iter().filter(|s| s.model_invocable).collect();
        if skills.is_empty() {
            return None;
        }
        let render = |limit: Option<usize>| {
            let mut out = String::from(
                "# Skills\nWhen a task matches one of these, load it with the `skill` tool and follow what it says.\n",
            );
            for skill in &skills {
                let description = match limit {
                    Some(limit) if skill.description.chars().count() > limit => {
                        let cut: String = skill.description.chars().take(limit).collect();
                        format!("{cut}…")
                    }
                    _ => skill.description.clone(),
                };
                out.push_str(&format!("- {}: {description}\n", skill.name));
            }
            out.trim_end().to_string()
        };
        let full = render(None);
        Some(if full.chars().count() > MAX_LISTING_CHARS {
            render(Some(SHORT_DESCRIPTION_CHARS))
        } else {
            full
        })
    }
}

pub struct SkillsPart(pub Arc<SkillRegistry>);

#[async_trait]
impl SystemPartProvider for SkillsPart {
    async fn render(&self, _: &AgentInfo) -> Option<String> {
        self.0.listing()
    }
}

pub struct SkillTool {
    registry: Arc<SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        SkillTool { registry }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a skill by name. Returns the skill's instructions and the directory its reference files \
         live in; read those files with `read` (or `skill://<name>/<file>`) when the instructions point \
         to them."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "args": {"type": "string", "description": "Arguments for the skill, if it takes any"}
            },
            "required": ["name"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(name) = input.get("name").and_then(Value::as_str) else {
            return ToolOutput::error("invalid arguments: `name` is required");
        };
        let Some(skill) = self.registry.get(name.trim()) else {
            let known: Vec<String> = self
                .registry
                .list()
                .into_iter()
                .map(|s| s.name)
                .take(40)
                .collect();
            return ToolOutput::error(format!(
                "no skill named `{name}`; available: {}",
                known.join(", ")
            ));
        };
        self.registry.mark_used(&skill);
        let text = invocation_text(
            &skill,
            input.get("args").and_then(Value::as_str).unwrap_or_default(),
        );
        if skill.context == SkillContext::Fork
            && let Some(runner) = ctx.services.get::<Subagents>()
        {
            let def = skill.agent.as_deref().unwrap_or("coder");
            match runner.0.run(def, text.clone(), &ctx.agent).await {
                Ok(report) => return ToolOutput::text(report),
                // An agent that may not start others follows the skill itself.
                Err(error) if error == crate::subagent::UNAVAILABLE => {}
                Err(error) => return ToolOutput::error(error),
            }
        }
        ToolOutput::text(text)
    }
}

/// `skill://<name>` is the skill body; `skill://<name>/<relative path>` a file beside `SKILL.md`.
pub struct SkillScheme(pub Arc<SkillRegistry>);

#[async_trait]
impl Scheme for SkillScheme {
    fn scheme(&self) -> &str {
        "skill"
    }

    async fn read(&self, rest: &str, _: &ToolCtx) -> Result<Vec<Block>, String> {
        let (name, relative) = rest.split_once('/').unwrap_or((rest, ""));
        let skill = self
            .0
            .get(name)
            .ok_or_else(|| format!("no skill named `{name}`"))?;
        if relative.is_empty() {
            return Ok(vec![Block::text(skill.body)]);
        }
        let root = dunce::canonicalize(&skill.dir).map_err(|e| format!("{}: {e}", skill.dir.display()))?;
        let target = dunce::canonicalize(root.join(relative))
            .map_err(|_| format!("skill `{name}` has no file `{relative}`"))?;
        if !target.starts_with(&root) {
            return Err(format!("`{relative}` is outside skill `{name}`"));
        }
        let meta = tokio::fs::metadata(&target).await.map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err(format!("`{relative}` is not a file"));
        }
        if meta.len() > MAX_REFERENCE_BYTES {
            return Err(format!(
                "`{relative}` is {} bytes; read it by path with offset and limit",
                meta.len()
            ));
        }
        let text = tokio::fs::read_to_string(&target)
            .await
            .map_err(|e| format!("{relative}: {e}"))?;
        Ok(vec![Block::text(text)])
    }
}

/// One entry per skill (name, where it comes from, whether the model is offered it, and its
/// description), then the files that are not valid skills.
pub fn listing(skills: &[Skill], invalid: &[InvalidSkill]) -> String {
    let mut out = String::new();
    for skill in skills {
        let flags = match (skill.model_invocable, skill.user_invocable) {
            (true, true) => "",
            (true, false) => " (model only)",
            (false, true) => " (manual: /skill:name)",
            (false, false) => " (disabled)",
        };
        out.push_str(&format!(
            "{} [{}]{flags}\n  {}\n",
            skill.name,
            skill.source.label(),
            skill.description
        ));
    }
    for invalid in invalid {
        out.push_str(&format!(
            "invalid: {}\n  {}\n",
            invalid.path.display(),
            invalid.error
        ));
    }
    if out.is_empty() {
        "no skills found".to_string()
    } else {
        out
    }
}

struct ListSkills(Arc<SkillRegistry>);

#[async_trait]
impl crate::ext::CommandHandler for ListSkills {
    async fn run(&self, _: &str, _: &crate::ext::ExtCtx) -> crate::ext::CommandOutcome {
        crate::ext::CommandOutcome::Show(listing(&self.0.list(), &self.0.invalid()))
    }
}

/// Registers skill roots, the `skills` prompt part, the `skill` tool, `skill://` and `/skills`.
pub struct SkillsExt {
    /// Lowest precedence first.
    pub roots: Vec<SkillRoot>,
}

impl crate::ext::Extension for SkillsExt {
    fn id(&self) -> &str {
        "skills"
    }

    fn register(&self, r: &mut crate::ext::Registrar) -> Result<(), crate::ext::ExtError> {
        let registry = r
            .services()
            .get::<SkillRegistry>()
            .ok_or_else(|| crate::ext::ExtError("the skill registry service is missing".to_string()))?;
        for root in &self.roots {
            registry.add_root(root.clone());
        }
        r.system_part("skills", Arc::new(SkillsPart(registry.clone())));
        r.tool(Arc::new(SkillTool::new(registry.clone())));
        r.scheme(Arc::new(SkillScheme(registry.clone())));
        r.command(crate::ext::Command {
            name: "skills".to_string(),
            description: "List skills and where they come from".to_string(),
            argument_hint: None,
            handler: Arc::new(ListSkills(registry)),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, frontmatter: &str, body: &str) -> PathBuf {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), format!("---\n{frontmatter}\n---\n{body}")).unwrap();
        path
    }

    fn roots(tmp: &Path) -> Vec<SkillRoot> {
        vec![
            SkillRoot::new(tmp.join("builtin"), SkillSource::Builtin),
            SkillRoot::new(tmp.join("home"), SkillSource::Home),
            SkillRoot::new(tmp.join("project"), SkillSource::Project),
        ]
    }

    #[test]
    fn later_roots_win_and_identity_is_the_frontmatter_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            &tmp.path().join("builtin"),
            "review",
            "name: code-review\ndescription: built-in review",
            "B",
        );
        write_skill(
            &tmp.path().join("home"),
            "my-review",
            "name: code-review\ndescription: my review",
            "H",
        );
        write_skill(
            &tmp.path().join("home"),
            "notes",
            "name: notes\ndescription: take notes",
            "N",
        );
        write_skill(
            &tmp.path().join("project"),
            "taste-skill",
            "name: design-taste\ndescription: project taste",
            "P",
        );

        let set = discover(&roots(tmp.path()));
        assert_eq!(
            set.skills.keys().collect::<Vec<_>>(),
            ["code-review", "design-taste", "notes"]
        );
        let review = &set.skills["code-review"];
        assert_eq!(
            (review.description.as_str(), review.body.as_str(), &review.source),
            ("my review", "H", &SkillSource::Home)
        );
        assert!(review.dir.ends_with("my-review"));
        assert!(set.invalid.is_empty());
    }

    #[test]
    fn invalid_skills_are_reported_with_the_reason_never_dropped_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        write_skill(&root, "a-bad-name", "name: Bad_Name\ndescription: x", "");
        write_skill(&root, "b-no-description", "name: fine", "");
        write_skill(
            &root,
            "c-long",
            &format!("name: long\ndescription: {}", "d".repeat(1025)),
            "",
        );
        write_skill(&root, "d-colon", "name: colon\ndescription: Use when: always", "");
        write_skill(&root, "e-dup", "name: dup\nname: dup2\ndescription: x", "");
        std::fs::create_dir_all(root.join("f-raw")).unwrap();
        std::fs::write(root.join("f-raw/SKILL.md"), "# No frontmatter\n").unwrap();
        write_skill(
            &root,
            "g-ok",
            &format!("name: ok\ndescription: {}", "d".repeat(1024)),
            "",
        );

        let set = discover(&[SkillRoot::new(root, SkillSource::Home)]);
        assert_eq!(set.skills.keys().collect::<Vec<_>>(), ["ok"]);
        let errors: Vec<&str> = set.invalid.iter().map(|i| i.error.as_str()).collect();
        assert_eq!(errors.len(), 6);
        assert!(errors[0].contains("lowercase words joined by hyphens"));
        assert!(errors[1].contains("`description` is missing"));
        assert!(errors[2].contains("longer than 1024"));
        assert!(errors[3].contains("invalid frontmatter"));
        assert!(errors[4].contains("invalid frontmatter"));
        assert!(errors[5].contains("missing frontmatter"));
        assert!(set.invalid[0].path.ends_with("a-bad-name/SKILL.md"));
    }

    #[test]
    fn frontmatter_keys_are_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_skill(
            tmp.path(),
            "s",
            "name: deploy\ndescription: ship it\nwhen_to_use: on release day\nargument-hint: \"[env]\"\n\
             disable-model-invocation: true\nuser-invocable: false\ncontext: fork\nagent: tester\n\
             model: [\"@slow\", \"not a model\"]\nallowed-tools: read, shell(git diff:*)\nhooks:\n  Stop: []\nunknown-key: ignored",
            "\nDeploy to $ARGUMENTS now.\n",
        );
        let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        let skill = parse_skill(&dir.join("SKILL.md"), &text, SkillSource::Project).unwrap();
        assert_eq!(skill.when_to_use.as_deref(), Some("on release day"));
        assert_eq!(skill.argument_hint.as_deref(), Some("[env]"));
        assert!(!skill.model_invocable && !skill.user_invocable);
        assert_eq!(
            (skill.context, skill.agent.as_deref()),
            (SkillContext::Fork, Some("tester"))
        );
        assert_eq!(skill.model, Some(vec![ModelSpec::Role("slow".into())]));
        assert_eq!(
            skill.allowed_tools,
            Some(vec!["read".to_string(), "shell(git diff:*)".to_string()])
        );
        assert_eq!(skill.hooks, Some(json!({"Stop": []})));
        assert_eq!(skill.body, "Deploy to $ARGUMENTS now.\n");

        assert_eq!(
            invocation_text(&skill, " staging "),
            format!("Skill directory: {}\n\nDeploy to staging now.\n", dir.display())
        );
        let plain = Skill {
            body: "No placeholder.".into(),
            ..skill
        };
        assert!(invocation_text(&plain, "x y").ends_with("No placeholder.\n\nArguments: x y"));
        assert!(invocation_text(&plain, "").ends_with("No placeholder."));
    }

    #[test]
    fn listing_is_sorted_hides_non_model_skills_and_shortens_when_large() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        write_skill(&root, "b", "name: beta\ndescription: second", "");
        write_skill(&root, "a", "name: alpha\ndescription: first", "");
        write_skill(
            &root,
            "h",
            "name: hidden\ndescription: not listed\ndisable-model-invocation: true",
            "",
        );
        let registry = SkillRegistry::default();
        assert_eq!(registry.listing(), None);
        registry.add_root(SkillRoot::new(root.clone(), SkillSource::Home));
        let listing = registry.listing().unwrap();
        assert!(listing.ends_with("- alpha: first\n- beta: second"), "{listing}");
        assert!(!listing.contains("hidden"));

        // A root may offer only some of its skills to the model.
        registry.set_roots(vec![SkillRoot {
            model_invocable: Some(vec!["beta".into()]),
            ..SkillRoot::new(root.clone(), SkillSource::Home)
        }]);
        assert!(!registry.listing().unwrap().contains("alpha"));
        assert!(registry.get("alpha").is_some());

        for i in 0..12 {
            write_skill(
                &root,
                &format!("big{i}"),
                &format!("name: big-{i}\ndescription: {}", "w".repeat(900)),
                "",
            );
        }
        registry.set_roots(vec![SkillRoot::new(root, SkillSource::Home)]);
        let listing = registry.listing().unwrap();
        assert!(listing.chars().count() < 4000, "{}", listing.chars().count());
        assert!(listing.contains(&format!("- big-0: {}…", "w".repeat(200))));
    }

    #[test]
    fn first_use_is_announced_once_per_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a", "name: alpha\ndescription: first", "");
        let registry = SkillRegistry::default();
        registry.add_root(SkillRoot::new(tmp.path().to_path_buf(), SkillSource::Home));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        registry.on_first_use(Arc::new(move |skill| {
            sink.lock().unwrap().push(skill.name.clone())
        }));
        let alpha = registry.get("alpha").unwrap();
        registry.mark_used(&alpha);
        registry.mark_used(&alpha);
        assert_eq!(*seen.lock().unwrap(), ["alpha"]);
    }
}
