//! System prompt assembly from named parts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::AgentInfo;
use azazel_ai::SystemPart;

/// Renders one named part of the system prompt for an agent; `None` leaves the part out.
#[async_trait]
pub trait SystemPartProvider: Send + Sync {
    async fn render(&self, agent: &AgentInfo) -> Option<String>;
}

/// Parts that stay byte-identical from request to request, in order. The last one rendered carries
/// the cache boundary.
pub const STABLE_PARTS: [&str; 4] = ["identity", "rules", "team", "skills"];
/// Parts that change only when the user changes something on disk or in the session's setup.
/// Parts with other names follow them, in registration order.
const SESSION_PARTS: [&str; 2] = ["environment", "context_files"];
/// Parts that change while a session runs: the date and branch, what is remembered, the mode and
/// the goal. They are not part of the system prompt: they go in the note that follows the
/// conversation ([`azazel_ai::Request::tail`]), so a change in one of them leaves everything a
/// provider has cached as it was.
pub const TAIL_PARTS: [&str; 3] = ["now", "memory", "mode"];

/// What the parts make of one request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prompt {
    /// The system prompt.
    pub system: Vec<SystemPart>,
    /// The notes for [`azazel_ai::Request::tail`].
    pub tail: Vec<SystemPart>,
}

struct Part {
    owner: String,
    name: String,
    provider: Arc<dyn SystemPartProvider>,
}

/// Several providers may contribute to the same name; their texts are joined in registration order.
#[derive(Default)]
pub struct PromptBuilder {
    parts: RwLock<Vec<Part>>,
}

impl PromptBuilder {
    pub fn add(&self, owner: &str, name: &str, provider: Arc<dyn SystemPartProvider>) {
        self.parts
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Part {
                owner: owner.to_string(),
                name: name.to_string(),
                provider,
            });
    }

    pub fn remove_owner(&self, owner: &str) {
        self.parts
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|p| p.owner != owner);
    }

    /// The system prompt alone. A request also carries the tail: see [`PromptBuilder::prompt`].
    pub async fn render(&self, agent: &AgentInfo) -> Vec<SystemPart> {
        self.prompt(agent).await.system
    }

    pub async fn prompt(&self, agent: &AgentInfo) -> Prompt {
        let registered: Vec<(String, Arc<dyn SystemPartProvider>)> = {
            let parts = self.parts.read().unwrap_or_else(PoisonError::into_inner);
            parts
                .iter()
                .map(|p| (p.name.clone(), p.provider.clone()))
                .collect()
        };
        let fixed = |name: &str| {
            STABLE_PARTS.contains(&name) || SESSION_PARTS.contains(&name) || TAIL_PARTS.contains(&name)
        };
        let mut order: Vec<&str> = Vec::new();
        order.extend(STABLE_PARTS);
        order.extend(SESSION_PARTS);
        for (name, _) in &registered {
            if !fixed(name) && !order.contains(&name.as_str()) {
                order.push(name);
            }
        }
        order.extend(TAIL_PARTS);

        let mut out = Prompt::default();
        let mut boundary = None;
        for name in order {
            let mut text = String::new();
            for (_, provider) in registered.iter().filter(|(n, _)| n == name) {
                if let Some(piece) = provider.render(agent).await.filter(|p| !p.trim().is_empty()) {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(piece.trim_end());
                }
            }
            if text.is_empty() {
                continue;
            }
            let part = SystemPart {
                text,
                name: name.to_string(),
                cache_boundary: false,
            };
            if TAIL_PARTS.contains(&name) {
                out.tail.push(part);
                continue;
            }
            if STABLE_PARTS.contains(&name) {
                boundary = Some(out.system.len());
            }
            out.system.push(part);
        }
        if let Some(i) = boundary {
            out.system[i].cache_boundary = true;
        }
        out
    }
}

/// A part with fixed text for every agent.
pub struct StaticPart(pub String);

#[async_trait]
impl SystemPartProvider for StaticPart {
    async fn render(&self, _: &AgentInfo) -> Option<String> {
        Some(self.0.clone())
    }
}

/// The lead's identity: the body of the primary agent it is, with the skills that definition
/// names. Sub-agents get theirs from their own definition.
pub struct LeadIdentity {
    pub lead: Arc<crate::agents::LeadAgent>,
    pub services: Arc<azazel_agent::Services>,
}

#[async_trait]
impl SystemPartProvider for LeadIdentity {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        if agent.depth != 0 {
            return None;
        }
        let def = self.lead.def()?;
        let mut text = def.body.trim().to_string();
        if let Some(skills) = self.services.get::<crate::skills::SkillRegistry>() {
            for name in &def.skills {
                if let Some(skill) = skills.get(name) {
                    text.push_str(&format!("\n\n# Skill: {}\n{}", skill.name, skill.body.trim()));
                }
            }
        }
        Some(text)
    }
}

pub const RULES: &str = "# Rules\n\
- Read a file before you edit it, and edit the smallest region that does the job.\n\
- Prefer `edit` over `write` for files that already exist; use `write` for new files.\n\
- Follow the conventions already present in the code you touch.\n\
- Never commit unless the user asks you to.\n\
- Never run destructive git commands (`reset --hard`, `clean -f`, `checkout .`, `branch -D`, or anything \
with `--force`) unless the user asks you to.\n\
- Push only when the user asks you to. Sub-agents never push: the harness refuses it.\n\
- Do not say work is complete until you have run the project's build or tests and seen them pass. If you \
could not run them, say so.\n\
- When a tool call is denied, do not retry it in another form; adjust the plan or ask the user.\n\
- Keep answers short and concrete. Reference code as `path:line`.\n\
- Write like a person: lead with the answer; no openers (\"Great question\"), closing summaries or offers of \
more help; no inflated words (crucial, robust, seamless) or unsourced \"experts say\"; headings, bullets and \
bold only where prose will not do.\n\
- A reply that finishes a task ends with one line on what was not checked or run, and any risk; omit it \
when there is none.\n\
- A `<system-note>` after the conversation is from the harness, not from the user: the date, the \
branch, the mode you are in, what is remembered, your task list. It is current for this request; \
act on it and do not answer it.";

/// Operating system, shell, working directory and project root: what stays the same for a
/// whole session.
pub struct Environment {
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    pub shell: String,
}

#[async_trait]
impl SystemPartProvider for Environment {
    async fn render(&self, _: &AgentInfo) -> Option<String> {
        let mut text = format!(
            "# Environment\n- OS: {} ({})\n- Shell: {}\n- Working directory: {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.shell,
            self.cwd.display(),
        );
        if self.project_root == self.cwd {
            text.push_str(&format!("\n- Project root: {}", self.project_root.display()));
        } else {
            // Started in a folder of the project: files there are the work; the rest of the project
            // is read freely but changes outside the working directory are asked about.
            text.push_str(&format!(
                "\n- Project root: {} (the working directory is a folder inside it; changes outside the working directory need the user's approval)",
                self.project_root.display()
            ));
        }
        Some(text)
    }
}

/// The date and the git branch: what may be different by the next request.
pub struct Now {
    pub project_root: PathBuf,
}

#[async_trait]
impl SystemPartProvider for Now {
    async fn render(&self, _: &AgentInfo) -> Option<String> {
        let mut text = format!("Date: {}", jiff::Zoned::now().date());
        if let Some(branch) = git_branch(&self.project_root) {
            text.push_str(&format!("\nGit branch: {branch}"));
        }
        Some(text)
    }
}

/// Text rendered for the lead only (`depth` 0), or for every other agent only.
struct ForLead {
    text: String,
    lead: bool,
}

#[async_trait]
impl SystemPartProvider for ForLead {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        ((agent.depth == 0) == self.lead).then(|| self.text.clone())
    }
}

/// Registers the parts every session has: the lead identity, the rules, the environment, and the
/// date and branch.
pub struct BasePromptExt {
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    /// Name of the shell agents' commands run in.
    pub shell: String,
    /// The primary agent the lead is, whose body is the lead's identity.
    pub lead: Arc<crate::agents::LeadAgent>,
    /// Replaces the lead's identity and rules (`--system-prompt`); sub-agents keep the rules.
    pub system: Option<String>,
    /// Added to the lead's prompt after the context files (`--append-system-prompt`).
    pub append: Option<String>,
}

impl crate::ext::Extension for BasePromptExt {
    fn id(&self) -> &str {
        "base-prompt"
    }

    fn register(&self, r: &mut crate::ext::Registrar) -> Result<(), crate::ext::ExtError> {
        match &self.system {
            Some(text) => {
                r.system_part(
                    "identity",
                    Arc::new(ForLead {
                        text: text.clone(),
                        lead: true,
                    }),
                );
                r.system_part(
                    "rules",
                    Arc::new(ForLead {
                        text: RULES.to_string(),
                        lead: false,
                    }),
                );
            }
            None => {
                r.system_part(
                    "identity",
                    Arc::new(LeadIdentity {
                        lead: self.lead.clone(),
                        services: r.services().clone(),
                    }),
                );
                r.system_part("rules", Arc::new(StaticPart(RULES.to_string())));
            }
        }
        if let Some(text) = &self.append {
            r.system_part(
                "appended",
                Arc::new(ForLead {
                    text: text.clone(),
                    lead: true,
                }),
            );
        }
        r.system_part(
            "environment",
            Arc::new(Environment {
                cwd: self.cwd.clone(),
                project_root: self.project_root.clone(),
                shell: self.shell.clone(),
            }),
        );
        r.system_part(
            "now",
            Arc::new(Now {
                project_root: self.project_root.clone(),
            }),
        );
        Ok(())
    }
}

/// The checked-out branch, read from `.git/HEAD` without running git.
pub fn git_branch(project_root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(project_root.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None => Some(format!("detached at {}", head.get(..8)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(depth: u32) -> AgentInfo {
        AgentInfo {
            id: "a".into(),
            def: "lead".into(),
            parent: None,
            depth,
            session_id: "s".into(),
        }
    }

    struct Maybe(Option<&'static str>);

    #[async_trait]
    impl SystemPartProvider for Maybe {
        async fn render(&self, _: &AgentInfo) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    fn lead() -> Arc<crate::agents::LeadAgent> {
        let defs = crate::agents::discover(
            &[
                (
                    "build.md",
                    "---\nname: build\ndescription: b\nmode: primary\n---\nYou build.",
                ),
                (
                    "plan.md",
                    "---\nname: plan\ndescription: p\nmode: primary\n---\nYou plan.",
                ),
            ],
            &[],
        );
        Arc::new(crate::agents::LeadAgent::new(Arc::new(defs), "build").unwrap())
    }

    fn identity(lead: &Arc<crate::agents::LeadAgent>) -> Arc<LeadIdentity> {
        Arc::new(LeadIdentity {
            lead: lead.clone(),
            services: Arc::new(azazel_agent::Services::new()),
        })
    }

    #[tokio::test]
    async fn parts_render_in_fixed_order_with_the_boundary_on_the_last_stable_part_and_the_rest_in_the_tail()
    {
        let builder = PromptBuilder::default();
        // Registered out of order on purpose.
        builder.add("x", "mode", Arc::new(Maybe(Some("MODE"))));
        builder.add("x", "mcp", Arc::new(Maybe(Some("MCP"))));
        builder.add("x", "environment", Arc::new(Maybe(Some("ENV"))));
        builder.add("x", "skills", Arc::new(Maybe(None)));
        builder.add("x", "rules", Arc::new(Maybe(Some("RULES"))));
        builder.add("x", "identity", identity(&lead()));
        builder.add("y", "rules", Arc::new(Maybe(Some("MORE RULES\n"))));
        builder.add("y", "memory", Arc::new(Maybe(Some("  "))));

        builder.add("y", "now", Arc::new(Maybe(Some("NOW"))));
        builder.add("y", "mode", Arc::new(Maybe(Some("GOAL"))));

        let prompt = builder.prompt(&info(0)).await;
        let parts = &prompt.system;
        let texts: Vec<&str> = parts.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, ["You build.", "RULES\n\nMORE RULES", "ENV", "MCP"]);
        let names = |parts: &[SystemPart]| -> Vec<String> { parts.iter().map(|p| p.name.clone()).collect() };
        assert_eq!(names(parts), ["identity", "rules", "environment", "mcp"]);
        assert_eq!(
            parts.iter().map(|p| p.cache_boundary).collect::<Vec<_>>(),
            [false, true, false, false]
        );
        // What changes while a session runs is no part of the system prompt.
        assert_eq!(names(&prompt.tail), ["now", "mode"]);
        let notes: Vec<&str> = prompt.tail.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(notes, ["NOW", "MODE\n\nGOAL"]);
        assert_eq!(builder.render(&info(0)).await, prompt.system);

        // A sub-agent has no lead identity; the boundary still closes the stable prefix.
        let sub = builder.render(&info(1)).await;
        assert_eq!(sub[0].text, "RULES\n\nMORE RULES");
        assert!(sub[0].cache_boundary);

        builder.remove_owner("y");
        assert_eq!(builder.render(&info(0)).await[1].text, "RULES");
    }

    #[tokio::test]
    async fn a_primary_agent_replaces_the_identity_and_nothing_else() {
        let lead = lead();
        let builder = PromptBuilder::default();
        builder.add("x", "identity", identity(&lead));
        builder.add("x", "rules", Arc::new(StaticPart(RULES.to_string())));
        builder.add("x", "environment", Arc::new(Maybe(Some("ENV"))));
        builder.add("x", "context_files", Arc::new(Maybe(Some("AGENTS.md"))));
        let before = builder.render(&info(0)).await;
        lead.set("plan").unwrap();
        let after = builder.render(&info(0)).await;
        assert_eq!(before[0].text, "You build.");
        assert_eq!(after[0].text, "You plan.");
        assert_eq!(before[1..], after[1..]);
        // A sub-agent has its own identity, whichever agent the lead is.
        assert_eq!(builder.render(&info(1)).await[0].name, "rules");
    }
}
