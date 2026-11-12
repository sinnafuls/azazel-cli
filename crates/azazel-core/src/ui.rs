//! The bridge between agents and whoever is watching: the TUI, an RPC client, or nobody.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo};
use serde::Serialize;
use serde_json::Value;

use crate::ext::Level;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UiError {
    #[error("no interactive user")]
    NoUser,
    #[error("cancelled")]
    Cancelled,
}

/// A tool call waiting for the user's decision.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRequest {
    pub agent: AgentInfo,
    pub tool: String,
    pub access: Access,
    pub subjects: Vec<String>,
    pub input: Value,
    /// The rule `Always` would add, when one can express this call.
    pub rule: Option<String>,
    /// Extra text for the prompt (for example that the call may cost money).
    pub note: Option<String>,
    /// The call would destroy work (`git reset --hard`, `rm -r` outside the project). An
    /// interface should not offer approval as the default answer.
    #[serde(default)]
    pub destructive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    /// Allow and remember `ApprovalRequest::rule`.
    Always,
    Deny {
        feedback: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    /// Leave plan mode and carry the plan out.
    Approve,
    /// Keep planning, with this feedback.
    Revise(String),
}

#[async_trait]
pub trait UiBridge: Send + Sync {
    /// A question from an agent. With `options`, returns the chosen ones (one unless `multi`);
    /// without, returns one free-text answer.
    async fn ask(&self, question: &str, options: &[String], multi: bool) -> Result<Vec<String>, UiError>;

    async fn confirm(&self, prompt: &str) -> Result<bool, UiError>;

    /// Index of the chosen option.
    async fn select(&self, prompt: &str, options: &[String]) -> Result<usize, UiError>;

    async fn input(&self, prompt: &str) -> Result<String, UiError>;

    /// Like [`UiBridge::input`] for a secret such as an API key: a UI that can hides what is
    /// typed.
    async fn secret(&self, prompt: &str) -> Result<String, UiError> {
        self.input(prompt).await
    }

    fn notify(&self, level: Level, text: &str);

    /// `None` when nobody can answer.
    async fn approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision>;

    /// Shows a plan written in plan mode and returns the user's decision on it.
    async fn review_plan(&self, _path: &std::path::Path, _plan: &str) -> Result<PlanDecision, UiError> {
        Err(UiError::NoUser)
    }
}

/// The service under which the active bridge is registered.
pub struct Ui(pub Arc<dyn UiBridge>);

/// The bridge when no user is attached: questions fail, notifications go to the log.
pub struct Headless;

#[async_trait]
impl UiBridge for Headless {
    async fn ask(&self, _: &str, _: &[String], _: bool) -> Result<Vec<String>, UiError> {
        Err(UiError::NoUser)
    }

    async fn confirm(&self, _: &str) -> Result<bool, UiError> {
        Err(UiError::NoUser)
    }

    async fn select(&self, _: &str, _: &[String]) -> Result<usize, UiError> {
        Err(UiError::NoUser)
    }

    async fn input(&self, _: &str) -> Result<String, UiError> {
        Err(UiError::NoUser)
    }

    fn notify(&self, level: Level, text: &str) {
        match level {
            Level::Info => tracing::info!("{text}"),
            Level::Warn => tracing::warn!("{text}"),
            Level::Error => tracing::error!("{text}"),
        }
    }

    async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
        None
    }
}

/// What a key bound by an extension does.
#[async_trait]
pub trait ShortcutHandler: Send + Sync {
    async fn run(&self, ctx: &crate::ext::ExtCtx);
}

type Keyed<T> = Vec<(String, String, T)>;

/// What extensions add to the interactive UI: status-line segments, lines above the input, and
/// key bindings. Registered as a service; a UI that has no place for them ignores it.
#[derive(Default)]
pub struct UiExtras {
    /// `(owner, key, text)`.
    status: std::sync::RwLock<Keyed<String>>,
    /// `(owner, key, lines)`.
    widgets: std::sync::RwLock<Keyed<Vec<String>>>,
    /// `(owner, key combination, handler)`.
    shortcuts: std::sync::RwLock<Keyed<Arc<dyn ShortcutHandler>>>,
}

fn put<T>(list: &std::sync::RwLock<Keyed<T>>, owner: &str, key: &str, value: Option<T>) {
    let mut list = list.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    let at = list.iter().position(|(o, k, _)| o == owner && k == key);
    match (at, value) {
        (Some(at), Some(value)) => list[at].2 = value,
        (Some(at), None) => {
            list.remove(at);
        }
        (None, Some(value)) => list.push((owner.to_string(), key.to_string(), value)),
        (None, None) => {}
    }
}

impl UiExtras {
    /// Sets the status-line segment `key` of `owner`; `None` removes it.
    pub fn set_status(&self, owner: &str, key: &str, text: Option<String>) {
        put(&self.status, owner, key, text);
    }

    /// The segments to show, in the order they were first set.
    pub fn status(&self) -> Vec<String> {
        let status = self
            .status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.iter().map(|(_, _, text)| text.clone()).collect()
    }

    /// Sets the widget `key` of `owner`, shown as lines above the input; `None` removes it.
    pub fn set_widget(&self, owner: &str, key: &str, lines: Option<Vec<String>>) {
        put(&self.widgets, owner, key, lines);
    }

    /// The lines of every widget, in the order the widgets were first set.
    pub fn widget_lines(&self) -> Vec<String> {
        let widgets = self
            .widgets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        widgets
            .iter()
            .flat_map(|(_, _, lines)| lines.iter().cloned())
            .collect()
    }

    /// Binds `key` (for example `ctrl+k` or `alt+shift+p`) to `handler`; a later binding of the
    /// same key by the same owner replaces it.
    pub fn bind(&self, owner: &str, key: &str, handler: Arc<dyn ShortcutHandler>) {
        put(
            &self.shortcuts,
            owner,
            &key.trim().to_ascii_lowercase(),
            Some(handler),
        );
    }

    /// The handler bound to `key`, written the same way as in [`UiExtras::bind`].
    pub fn shortcut(&self, key: &str) -> Option<Arc<dyn ShortcutHandler>> {
        let shortcuts = self
            .shortcuts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shortcuts
            .iter()
            .find(|(_, bound, _)| bound == key)
            .map(|(_, _, handler)| handler.clone())
    }

    /// Drops everything `owner` added.
    pub fn remove_owner(&self, owner: &str) {
        self.status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(o, _, _)| o != owner);
        self.widgets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(o, _, _)| o != owner);
        self.shortcuts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(o, _, _)| o != owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    #[async_trait]
    impl ShortcutHandler for Nothing {
        async fn run(&self, _: &crate::ext::ExtCtx) {}
    }

    #[test]
    fn extras_are_kept_per_owner_and_key_and_leave_with_their_owner() {
        let extras = UiExtras::default();
        extras.set_status("a", "branch", Some("main".into()));
        extras.set_status("b", "branch", Some("dev".into()));
        extras.set_status("a", "branch", Some("fix".into()));
        assert_eq!(extras.status(), ["fix", "dev"]);
        extras.set_status("b", "branch", None);
        extras.set_status("b", "never set", None);
        assert_eq!(extras.status(), ["fix"]);

        extras.set_widget("a", "todo", Some(vec!["one".into(), "two".into()]));
        extras.set_widget("b", "clock", Some(vec!["12:00".into()]));
        assert_eq!(extras.widget_lines(), ["one", "two", "12:00"]);

        extras.bind("a", " Ctrl+K ", Arc::new(Nothing));
        assert!(extras.shortcut("ctrl+k").is_some());
        assert!(extras.shortcut("ctrl+j").is_none());

        extras.remove_owner("a");
        assert_eq!(extras.status(), Vec::<String>::new());
        assert_eq!(extras.widget_lines(), ["12:00"]);
        assert!(extras.shortcut("ctrl+k").is_none());
    }
}
