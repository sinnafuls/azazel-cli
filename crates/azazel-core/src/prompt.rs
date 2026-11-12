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
/// Parts that may change between requests. Parts with other names render between `context_files`
/// and `memory`, in registration order.
const VOLATILE_HEAD: [&str; 2] = ["environment", "context_files"];
const VOLATILE_TAIL: [&str; 2] = ["memory", "mode"];

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

    pub async fn render(&self, agent: &AgentInfo) -> Vec<SystemPart> {
        let registered: Vec<(String, Arc<dyn SystemPartProvider>)> = {
            let parts = self.parts.read().unwrap_or_else(PoisonError::into_inner);
            parts
                .iter()
                .map(|p| (p.name.clone(), p.provider.clone()))
                .collect()
        };
        let fixed = |name: &str| {
            STABLE_PARTS.contains(&name) || VOLATILE_HEAD.contains(&name) || VOLATILE_TAIL.contains(&name)
        };
        let mut order: Vec<&str> = Vec::new();
        order.extend(STABLE_PARTS);
        order.extend(VOLATILE_HEAD);
        for (name, _) in &registered {
            if !fixed(name) && !order.contains(&name.as_str()) {
                order.push(name);
            }
        }
        order.extend(VOLATILE_TAIL);

        let mut out: Vec<SystemPart> = Vec::new();
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
            if STABLE_PARTS.contains(&name) {
                boundary = Some(out.len());
            }
            out.push(SystemPart {
                text,
                cache_boundary: false,
            });
        }
        if let Some(i) = boundary {
            out[i].cache_boundary = true;
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

/// The lead agent's identity. Sub-agents get theirs from their definition.
pub struct LeadIdentity;

pub const LEAD_IDENTITY: &str = "You are azazel, a coding agent working in the user's terminal. You lead a team: \
you can do the work yourself or delegate parts of it to specialist agents and combine their results. Work \
towards what the user asked for, verify what you change, and report plainly what you did and what you could not do.";

#[async_trait]
impl SystemPartProvider for LeadIdentity {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        (agent.depth == 0).then(|| LEAD_IDENTITY.to_string())
    }
}

pub const RULES: &str = "# Rules\n\
- Read a file before you edit it, and edit the smallest region that does the job.\n\
- Prefer `edit` over `write` for files that already exist; use `write` for new files.\n\
- Follow the conventions already present in the code you touch.\n\
- Never commit unless the user asks you to.\n\
- Never run destructive git commands (`reset --hard`, `clean -f`, `checkout .`, `branch -D`, or anything \
with `--force`) unless the user asks you to.\n\
- Never push. Pushing is the user's decision and the user's action.\n\
- Do not say work is complete until you have run the project's build or tests and seen them pass. If you \
could not run them, say so.\n\
- When a tool call is denied, do not retry it in another form; adjust the plan or ask the user.\n\
- Keep answers short and concrete. Reference code as `path:line`.";

/// Operating system, shell, working directory, date and git branch.
pub struct Environment {
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    pub shell: String,
}

#[async_trait]
impl SystemPartProvider for Environment {
    async fn render(&self, _: &AgentInfo) -> Option<String> {
        let mut text = format!(
            "# Environment\n- OS: {} ({})\n- Shell: {}\n- Working directory: {}\n- Project root: {}\n- Date: {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.shell,
            self.cwd.display(),
            self.project_root.display(),
            jiff::Zoned::now().date(),
        );
        if let Some(branch) = git_branch(&self.project_root) {
            text.push_str(&format!("\n- Git branch: {branch}"));
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

/// Registers the parts every session has: the lead identity, the rules and the environment.
pub struct BasePromptExt {
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    /// Name of the shell agents' commands run in.
    pub shell: String,
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
                r.system_part("identity", Arc::new(LeadIdentity));
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

    #[tokio::test]
    async fn parts_render_in_fixed_order_with_the_boundary_on_the_last_stable_part() {
        let builder = PromptBuilder::default();
        // Registered out of order on purpose.
        builder.add("x", "mode", Arc::new(Maybe(Some("MODE"))));
        builder.add("x", "mcp", Arc::new(Maybe(Some("MCP"))));
        builder.add("x", "environment", Arc::new(Maybe(Some("ENV"))));
        builder.add("x", "skills", Arc::new(Maybe(None)));
        builder.add("x", "rules", Arc::new(Maybe(Some("RULES"))));
        builder.add("x", "identity", Arc::new(LeadIdentity));
        builder.add("y", "rules", Arc::new(Maybe(Some("MORE RULES\n"))));
        builder.add("y", "memory", Arc::new(Maybe(Some("  "))));

        let parts = builder.render(&info(0)).await;
        let texts: Vec<&str> = parts.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            [LEAD_IDENTITY, "RULES\n\nMORE RULES", "ENV", "MCP", "MODE"]
        );
        assert_eq!(
            parts.iter().map(|p| p.cache_boundary).collect::<Vec<_>>(),
            [false, true, false, false, false]
        );

        // A sub-agent has no lead identity; the boundary still closes the stable prefix.
        let sub = builder.render(&info(1)).await;
        assert_eq!(sub[0].text, "RULES\n\nMORE RULES");
        assert!(sub[0].cache_boundary);

        builder.remove_owner("y");
        assert_eq!(builder.render(&info(0)).await[1].text, "RULES");
    }

    #[test]
    fn git_branch_reads_head() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_branch(dir.path()), None);
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_branch(dir.path()).as_deref(), Some("feature/x"));
        std::fs::write(
            dir.path().join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(git_branch(dir.path()).as_deref(), Some("detached at 01234567"));
    }
}
