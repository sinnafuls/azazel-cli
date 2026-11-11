//! The bridge agents use to reach the person at the terminal: each call becomes a request the UI
//! loop shows as an overlay and answers.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use azazel_core::{ApprovalDecision, ApprovalRequest, Level, PlanDecision, UiBridge, UiError};
use tokio::sync::{mpsc, oneshot};

pub type Reply<T> = oneshot::Sender<T>;

pub enum UiRequest {
    Ask {
        question: String,
        options: Vec<String>,
        multi: bool,
        reply: Reply<Result<Vec<String>, UiError>>,
    },
    Confirm {
        prompt: String,
        reply: Reply<Result<bool, UiError>>,
    },
    Select {
        prompt: String,
        options: Vec<String>,
        reply: Reply<Result<usize, UiError>>,
    },
    Input {
        prompt: String,
        /// Shown as dots while typed: an API key, a password.
        secret: bool,
        reply: Reply<Result<String, UiError>>,
    },
    Notify {
        level: Level,
        text: String,
    },
    Approval {
        request: Box<ApprovalRequest>,
        reply: Reply<Option<ApprovalDecision>>,
    },
    Plan {
        path: PathBuf,
        plan: String,
        reply: Reply<Result<PlanDecision, UiError>>,
    },
}

impl UiRequest {
    /// Whether whoever asked has stopped waiting; a notice never is.
    pub fn abandoned(&self) -> bool {
        match self {
            UiRequest::Ask { reply, .. } => reply.is_closed(),
            UiRequest::Confirm { reply, .. } => reply.is_closed(),
            UiRequest::Select { reply, .. } => reply.is_closed(),
            UiRequest::Input { reply, .. } => reply.is_closed(),
            UiRequest::Notify { .. } => false,
            UiRequest::Approval { reply, .. } => reply.is_closed(),
            UiRequest::Plan { reply, .. } => reply.is_closed(),
        }
    }
}

pub struct TuiBridge {
    requests: mpsc::UnboundedSender<UiRequest>,
}

impl TuiBridge {
    pub fn new(requests: mpsc::UnboundedSender<UiRequest>) -> Self {
        TuiBridge { requests }
    }

    /// Sends a request and waits for the UI's answer; `gone` is the answer when the UI has exited.
    async fn ask_ui<T>(&self, make: impl FnOnce(Reply<T>) -> UiRequest, gone: T) -> T {
        let (reply, answer) = oneshot::channel();
        if self.requests.send(make(reply)).is_err() {
            return gone;
        }
        answer.await.unwrap_or(gone)
    }
}

#[async_trait]
impl UiBridge for TuiBridge {
    async fn ask(&self, question: &str, options: &[String], multi: bool) -> Result<Vec<String>, UiError> {
        let (question, options) = (question.to_string(), options.to_vec());
        self.ask_ui(
            |reply| UiRequest::Ask {
                question,
                options,
                multi,
                reply,
            },
            Err(UiError::NoUser),
        )
        .await
    }

    async fn confirm(&self, prompt: &str) -> Result<bool, UiError> {
        let prompt = prompt.to_string();
        self.ask_ui(|reply| UiRequest::Confirm { prompt, reply }, Err(UiError::NoUser))
            .await
    }

    async fn select(&self, prompt: &str, options: &[String]) -> Result<usize, UiError> {
        let (prompt, options) = (prompt.to_string(), options.to_vec());
        self.ask_ui(
            |reply| UiRequest::Select {
                prompt,
                options,
                reply,
            },
            Err(UiError::NoUser),
        )
        .await
    }

    async fn input(&self, prompt: &str) -> Result<String, UiError> {
        let prompt = prompt.to_string();
        self.ask_ui(
            |reply| UiRequest::Input {
                prompt,
                secret: false,
                reply,
            },
            Err(UiError::NoUser),
        )
        .await
    }

    async fn secret(&self, prompt: &str) -> Result<String, UiError> {
        let prompt = prompt.to_string();
        self.ask_ui(
            |reply| UiRequest::Input {
                prompt,
                secret: true,
                reply,
            },
            Err(UiError::NoUser),
        )
        .await
    }

    fn notify(&self, level: Level, text: &str) {
        let _ = self.requests.send(UiRequest::Notify {
            level,
            text: text.to_string(),
        });
    }

    async fn approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        self.ask_ui(
            |reply| UiRequest::Approval {
                request: Box::new(request),
                reply,
            },
            None,
        )
        .await
    }

    async fn review_plan(&self, path: &Path, plan: &str) -> Result<PlanDecision, UiError> {
        let (path, plan) = (path.to_path_buf(), plan.to_string());
        self.ask_ui(
            |reply| UiRequest::Plan { path, plan, reply },
            Err(UiError::NoUser),
        )
        .await
    }
}
