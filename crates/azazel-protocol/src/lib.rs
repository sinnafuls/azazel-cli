//! The protocol of `azazel rpc`: the commands a client writes and the frames azazel answers
//! with, one JSON object per line in each direction.
//!
//! [`Command`] is what a client sends, wrapped in a [`Request`] that adds the `id` its
//! [`Response`] repeats. [`Frame`] is every line azazel writes: responses, the events of every
//! agent, and the requests agents make of the user. The types under "results" are the `data` of
//! the responses that carry more than `null`.
//!
//! The crate has no dependency on the rest of azazel, so a client can use it alone.

mod parse;

use schemars::JsonSchema;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

/// The protocol version the `hello` frame announces. It grows when a change would break a client
/// written for the version before.
pub const PROTOCOL: u32 = 1;

/// One line from the client: a command and the id its response repeats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    /// Any JSON value of the client's choosing; `null` when left out.
    #[serde(default)]
    pub id: Value,
    #[serde(flatten)]
    pub command: Command,
}

/// An image sent with a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Image {
    /// For example `image/png`.
    pub mime: String,
    /// The bytes, base64-encoded.
    pub data: String,
}

/// The answer to an `approval_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Allow this call once.
    Allow,
    /// Allow it, and whatever the request's `rule` covers until the session ends; for a
    /// `secret` request, the file. Nothing is saved.
    Session,
    /// Allow it and remember the request's `rule`.
    Always,
    /// Refuse; `feedback` is passed to the model.
    Deny,
}

/// What `rewind` takes back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RewindWhat {
    /// The project files only.
    Code,
    /// The conversation only.
    Conversation,
    #[default]
    Both,
}

/// The file format of `export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[serde(alias = "md")]
    Markdown,
    Html,
    Jsonl,
}

/// What a client that attaches to a background session is sent of what happened before.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Replay {
    /// The frames written since the last client left; everything for the first client. With
    /// another client attached already, the frames of the run in progress.
    #[default]
    Missed,
    /// Every frame since the background session started.
    All,
    /// Nothing but the requests that still wait for an answer.
    None,
}

fn yes() -> bool {
    true
}

/// What a client can ask of the session. `type` names the command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Starts work on the input. While the lead is busy it is queued. With a `request_id`,
    /// answers with [`RunState`], and a prompt whose `request_id` the session has seen before
    /// starts nothing: the answer says where the run the first one started stands.
    Prompt {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
        /// A name of the client's choosing for this prompt, to send it again safely.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// Goes on with the run that the end of the session's process cut short (`interrupted` of
    /// [`State`]): tool calls without a result get one, and the request the model never
    /// answered is sent again. Answers with [`RunState`].
    ContinueRun,
    /// Hands the working lead input before its next model request.
    Steer {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
    },
    /// Queues input for when the lead would go idle.
    FollowUp { text: String },
    /// Stops the lead's current work.
    Abort,
    /// Answers with the session's [`State`].
    GetState,
    /// Changes the lead's model chain, e.g. `["provider/model", "@role"]`. Answers with
    /// [`Models`].
    SetModel { chain: Vec<String> },
    /// Changes the permission mode of the session, for the lead and every sub-agent: `plan`,
    /// `manual`, `accept-edits`, `auto` or `bypass`. `bypass` lifts every guard but deny rules
    /// and the push guard; a client that offers it should have the user confirm first.
    SetMode { mode: String },
    /// Makes the lead another primary agent (`build`, `plan`, or one of the user's
    /// definitions with `mode: primary`): its identity, tools, limits, model and thinking level
    /// apply to the lead from the next request. `primary_agents` of [`State`] lists them.
    SetAgent { agent: String },
    /// Messages a sub-agent by id, or `all`.
    AgentSend { to: String, message: String },
    /// Stops a sub-agent and the agents it started, or `all`.
    AgentStop { agent: String },
    /// `@name text` as the user types it at a prompt: sends `text` to the sub-agent with the id
    /// `name`, or starts an agent from the definition of that name with `text` as its task (its
    /// report goes to the lead). Answers with [`Mentioned`], or `null` when `name` is neither.
    AgentMention { name: String, text: String },
    /// Answers an `approval_request`.
    ApprovalResponse {
        request_id: String,
        decision: Decision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    /// Answers a `ui_request`; a `null` value dismisses it.
    UiResponse {
        request_id: String,
        #[serde(default)]
        value: Value,
    },
    /// Leaves the open session and starts a new one. Answers with [`State`].
    NewSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Leaves the open session and opens a saved one, by `session_id` (a unique prefix is enough)
    /// or by the `path` of its file. Answers with [`State`].
    ResumeSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Continues in a copy of the open session, cut at `entry_id` when it is given. Answers
    /// with [`State`].
    ForkSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
    },
    /// The saved sessions of this project, most recently used first; with `query`, those whose
    /// messages contain it. Answers with a list of [`SessionRow`].
    ListSessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    /// The messages from the start of the session to `leaf` (the current leaf when left out).
    /// With `limit`, only the last `limit` of them, or with `before` (an entry id of the
    /// branch) the last `limit` messages before that entry, so that a client can load the
    /// history in pages. Answers with a list of [`MessageRow`], oldest first.
    GetMessages {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        leaf: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// The messages of a sub-agent's transcript, paged like `get_messages`. Answers with a
    /// list of [`MessageRow`], oldest first.
    GetAgentMessages {
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Every entry of the session file. Answers with a list of [`TreeRow`].
    GetTree,
    /// Continues the conversation from another entry of the tree; with `before`, from the
    /// entry before it, which is how a prompt is gone back to. Answers with [`State`].
    SetLeaf {
        entry_id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        before: bool,
    },
    /// Labels an entry of the tree; an empty label takes the label away.
    SetLabel { entry_id: String, label: String },
    /// The open session as its file holds it. Answers with [`SessionCopy`].
    GetSession,
    /// Names the session.
    SetName { name: String },
    /// Changes the thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.
    SetThinking { level: String },
    /// Changes the service tier: `priority`, `standard` or `economy`.
    SetTier { tier: String },
    /// Summarises the conversation so far to free context.
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    /// Goes back to before the last prompt and sends it again; the earlier attempt stays in the
    /// file as a branch.
    Retry,
    /// Goes back to a checkpoint. `entry_id` names a `checkpoint` entry of the tree, or the
    /// prompt that followed it.
    Rewind {
        entry_id: String,
        #[serde(default)]
        what: RewindWhat,
    },
    /// Runs a command in the user's shell, outside the permission rules. With `share` (the
    /// default) the lead is told what was run and what it printed. Answers with [`ShellResult`].
    Shell {
        command: String,
        #[serde(default = "yes")]
        share: bool,
    },
    /// Writes the open session to a file. The format comes from `format`, else from the file
    /// name, else it is Markdown; the default file is `azazel-session-<id>.<ext>` in the
    /// working directory. Answers with [`Exported`].
    Export {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<ExportFormat>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Asks a question on the side, without interrupting the lead. The answer arrives as
    /// `btw_delta` frames and then, whole, in the response as [`BtwAnswer`].
    Btw { question: String },
    /// Hands the work to a new session: keeps a draft brief made without a model, has the model
    /// write a structured brief (one request, to the lead's model), and switches to a new session
    /// whose first message is that brief. `focus` says what the brief should weigh. Answers with
    /// [`Handoff`] when the new session is open; refused while the lead works.
    Handoff {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
    },
    /// The models of the providers there is a credential for; with `all`, of every provider,
    /// each row saying whether it can be used. A provider whose list was asked for this
    /// session (`refresh_models`) has its rows from that answer, the others from the
    /// catalogue. Answers with a list of [`ModelRow`].
    ListModels {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
    /// Every provider: whether it can be used, how, and how many models it has. Answers with a
    /// list of [`ProviderRow`].
    ListProviders,
    /// Asks `provider` for the models it serves now. The answer stands for the rest of the
    /// session and is kept in the model cache for later ones. Answers with a list of
    /// [`ModelRow`] of that provider.
    RefreshModels { provider: String },
    /// Every model role: its chain, where that comes from and what it resolves to, plus the
    /// saved presets. Answers with [`Roles`].
    ListRoles,
    /// Points `role` at `chain` (`["provider/model:high", "@slow"]`), or with no chain takes the
    /// value `save` names away so the role falls back to the next layer or its built-in value.
    /// `session` changes it for this session only; `home` and `project` also write the config
    /// file (`project` only in a trusted project). Setting `default` or `lead` puts the lead on
    /// its role again. Answers with [`Roles`].
    SetRole {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chain: Option<Vec<String>>,
        #[serde(default)]
        save: SaveTo,
    },
    /// Writes every role as it is now under `[presets.<name>.roles]` in the home config.
    /// Answers with [`Roles`].
    SavePreset { name: String },
    /// Points every role of a saved preset at its models for this session. Answers with
    /// [`Roles`]. The old name of `set_profile`.
    ApplyPreset { name: String },
    /// Moves the session to profile `name` (a built-in one or one of the configuration): every
    /// role the profile names points at its models for this session, and the lead follows its
    /// role again. A profile whose providers have no login is refused. Sends a
    /// [`Frame::ProfileChanged`] and answers with the new [`State`].
    SetProfile { name: String },
    /// Writes every role as it is now as profile `name` under `[profiles.<name>.roles]` in the
    /// home config. Answers with the new [`State`].
    SaveProfile { name: String },
    /// The commands `run_command` takes: those of extensions, the built-in reports, and the
    /// skills as `skill:<name>`. Answers with a list of [`CommandRow`].
    ListCommands,
    /// The files of the working directory, as paths relative to it, for completing `@`
    /// mentions. Ignored files are left out and the list is cut at 20,000. Answers with a list
    /// of strings.
    ListFiles,
    /// What is known of each provider account's usage windows. Answers with a list of
    /// [`AccountUsage`].
    GetUsage,
    /// Runs a slash command by name (without the slash). Answers with [`CommandResult`].
    /// With `only`, the command runs only when it comes from one of those sources (see
    /// [`CommandRow::source`]); one that comes from another is refused. `azazel remote` sets it
    /// for a browser that may not run the project's own commands, so that the command it
    /// approved by name is the one that runs.
    RunCommand {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        args: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only: Option<Vec<CommandOrigin>>,
    },
    /// Every setting the settings screen of the terminal can change: where it is shown, how it
    /// is edited, its value, its default and who set it. Answers with [`SettingList`].
    GetSettings,
    /// Changes one setting (`path` as `table.key`, `value` as JSON of the setting's kind; `null`
    /// takes it out of the file `scope` names, back to what the layers below say). `session`
    /// changes it for this session only and takes only settings that apply at once (`live`);
    /// `home` writes the home config, `project` the project's (only in a trusted project, and a
    /// trust-gated key only there). The value is checked against the configuration first. A
    /// change of `tui.theme` is followed by a `theme` frame. Answers with [`SettingChanged`].
    SetSetting {
        path: String,
        value: Value,
        #[serde(default)]
        scope: SaveTo,
    },
    /// The colours of the configured terminal theme. A theme that follows the terminal's
    /// background takes its light palette when `light` is true. Answers with [`ThemeColors`].
    GetTheme {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        light: Option<bool>,
    },
    /// What the lead's last request was made of, by category. Answers with [`ContextReport`], or
    /// `null` when the session has sent no request yet.
    GetContext,
    /// Tokens and money spent in this session, with the prompt cache by model and the requests
    /// made on the side. Answers with [`CostReport`].
    GetCost,
    /// Trusts the project (or with `trusted: false` stops trusting it) from the next start of
    /// azazel, as `/trust yes|no` does. Answers with [`TrustState`].
    TrustProject {
        #[serde(default = "yes")]
        trusted: bool,
    },
    /// Lets agents work in another directory too, until azazel exits, as `/add-dir` does. A
    /// relative path is taken from the working directory. Answers with [`DirList`].
    AddDir { path: String },
    /// Forgets every remembered approval and every rule added in this session, as
    /// `/forget-approvals` does. Answers with [`Forgotten`].
    ForgetApprovals,
    /// Starts (`on`) or stops receiving `browser_frame` frames, a live picture of the tab the
    /// agents' browser shows. Frames are produced only while someone watches. With `width` and
    /// `height`, the room the client has for the picture in CSS pixels, the page is laid out for
    /// that shape while the client watches, so that the picture fills the room: at that size
    /// when both sides are between 320 and 1920, and otherwise at the nearest size of the same
    /// shape. Sending it again with another size changes it, and without one the client asks
    /// for none. The size of the client that has taken the browser over is used, and otherwise
    /// the one sent last; when the last client stops watching the page has its own size again.
    /// Answers with [`BrowserState`].
    BrowserWatch {
        on: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
    },
    /// Takes over the agents' browser (`on`), or hands it back. While it is taken over, the
    /// agents' next browser action waits, and `browser_input` is accepted from this client only;
    /// when it is handed back the agent is told what you did. Fails while another client has
    /// it, unless `force` takes it from them (a client that may control the session can; the
    /// other client is told through `browser_state`). A hold with no input for ten minutes is
    /// let go of. Answers with [`BrowserState`].
    BrowserTake {
        on: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    /// Clicks, types, scrolls or navigates in the agents' browser. Refused unless this client
    /// has taken the browser over with `browser_take`. Answers with [`BrowserState`].
    BrowserInput { input: BrowserInput },
    /// Shows the agents' own browser profile in a visible window on the machine that runs
    /// azazel, to sign in to sites, optionally opened on `url` (http or https). Answers with
    /// [`BrowserState`].
    BrowserSignIn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Starts the agents' browser when none runs, on its blank page or on `url` (http or https),
    /// so that there is a page to watch and take over before an agent has opened one. A browser
    /// that runs is left as it is, and `url` is opened in it when given. Answers with
    /// [`BrowserState`].
    BrowserOpen {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Background sessions only, and the first command on a connection to one: makes the
    /// connection a client of the session, beside any that are attached already. The frames
    /// `replay` asks for follow the response, then the requests that still wait for an answer,
    /// then what the session does from then on. Answers with [`Attached`].
    Attach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(default)]
        replay: Replay,
        /// Resumes after a frame: the `at` of the last frame the client saw. The frames
        /// logged after it follow the response, and nothing before. When the log has no frame
        /// with that `at`, `gap` is set in the answer and `replay` applies instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
    },
    /// Background sessions only, attached or not: ends the session once the lead has finished
    /// what it is doing; with `now` the lead is stopped first. The connection closes when the
    /// session has ended.
    Stop {
        #[serde(default)]
        now: bool,
    },
}

/// The answer to one command. `data` is present when `success` is true, `error` when it is not.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct Response {
    /// The `id` of the command; `null` for a line that was not a command.
    #[serde(default)]
    pub id: Value,
    pub success: bool,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub error: Option<String>,
    /// Why a failed command failed, when a client may want to tell: `refused` is the access of
    /// the device (`azazel remote` refuses a command it does not allow it).
    #[serde(default)]
    pub code: Option<String>,
}

/// The `code` of a response to a command the server does not allow this device.
pub const REFUSED: &str = "refused";

impl Response {
    pub fn ok(id: Value, data: Value) -> Self {
        Response {
            id,
            success: true,
            data,
            error: None,
            code: None,
        }
    }

    pub fn err(id: Value, error: impl Into<String>) -> Self {
        Response {
            id,
            success: false,
            data: Value::Null,
            error: Some(error.into()),
            code: None,
        }
    }

    /// A refusal: the device may not send this command (see [`REFUSED`]).
    pub fn refused(id: Value, error: impl Into<String>) -> Self {
        Response {
            code: Some(REFUSED.to_string()),
            ..Response::err(id, error)
        }
    }

    /// The data of a successful response, or the error of a failed one.
    pub fn into_result(self) -> Result<Value, String> {
        if self.success {
            Ok(self.data)
        } else {
            Err(self.error.unwrap_or_else(|| "the command failed".to_string()))
        }
    }
}

// Written by hand because a successful response always carries `data`, `null` included, and a
// failed one never does.
impl Serialize for Response {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("Response", 4)?;
        out.serialize_field("id", &self.id)?;
        out.serialize_field("success", &self.success)?;
        if self.success {
            out.serialize_field("data", &self.data)?;
        } else {
            out.serialize_field("error", self.error.as_deref().unwrap_or_default())?;
            if let Some(code) = &self.code {
                out.serialize_field("code", code)?;
            }
        }
        out.end()
    }
}

/// A part of the message the model is writing. Deltas are for display; `message_end` carries
/// the complete message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Start,
    TextDelta {
        index: u64,
        delta: String,
    },
    ThinkingDelta {
        index: u64,
        delta: String,
    },
    ToolCallStart {
        index: u64,
        id: String,
        name: String,
    },
    /// `delta` is a raw fragment of the call's JSON arguments.
    ToolCallDelta {
        index: u64,
        delta: String,
    },
    /// The finished content block at `index`.
    BlockEnd {
        index: u64,
        block: Value,
    },
    /// A stream event this version of the crate does not know.
    #[serde(other)]
    #[schemars(skip)]
    Unknown,
}

/// What a tool returned.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutput {
    /// `text` and `image` blocks.
    pub content: Vec<Value>,
    pub is_error: bool,
    /// Structured data for display (diffs, exit codes); the model never sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolOutput {
    /// The text blocks, joined.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect()
    }
}

/// One agent of the roster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Member {
    pub id: String,
    /// The agent definition it runs as; `lead` for the main agent.
    pub def: String,
    pub parent: Option<String>,
    pub depth: u32,
    /// `queued`, `running`, `waiting for approval`, `idle`, `done`, `failed` or `stopped`.
    pub state: String,
    pub elapsed_ms: u64,
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    /// What a running agent is doing, in a few words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
}

/// How an `ask` request is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// One of the options: a list with the chosen one.
    Select,
    /// Any number of the options: a list of the chosen ones.
    Multiselect,
    /// A typed answer: a string.
    Text,
    /// Yes or no: `true` or `false`.
    Confirm,
}

impl AskKind {
    /// The kind of an `ask` request: what it says it expects, or for a request of a session
    /// that does not say, what its options imply.
    pub fn of(expects: Option<AskKind>, options: &[String], multi: bool) -> AskKind {
        expects.unwrap_or(match (options.is_empty(), multi) {
            (true, _) => AskKind::Text,
            (false, true) => AskKind::Multiselect,
            (false, false) => AskKind::Select,
        })
    }
}

/// A question from an agent: an `ask` request, or one of the questions of a `form`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    pub question: String,
    /// A word or two naming the question among the others of a form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// What the user needs to know to answer, to be shown with the question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Code the question is about, to be shown with it as it is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// A unified diff the question is about, to be shown with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Always sent; a client reads a request without it with [`AskKind::of`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expects: Option<AskKind>,
    /// The choices of a `select` or `multiselect`; empty otherwise.
    #[serde(default)]
    pub options: Vec<String>,
    /// Whether `expects` is `multiselect`.
    #[serde(default)]
    pub multi: bool,
    /// The answer the agent would give: one of `options`, or `yes` or `no` for a `confirm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_freeform: bool,
}

impl Question {
    /// How the question is answered; see [`AskKind::of`].
    pub fn kind(&self) -> AskKind {
        AskKind::of(self.expects, &self.options, self.multi)
    }
}

/// A question to the user. Answer with `ui_response` and a `value` of the kind shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiRequest {
    /// A question from an agent. Expects what `expects` says: a list of the chosen options
    /// (`select`, `multiselect`), a string (`text`), or `true` or `false` (`confirm`). With
    /// `allow_freeform`, a string that is none of the options is an answer the user typed.
    Ask(Question),
    /// Several questions from an agent, to be answered together. Expects a list with one
    /// entry per question, in their order: what an `ask` of that question expects, or `null`
    /// for a question the user left open.
    Form { questions: Vec<Question> },
    /// Expects `true` or `false`.
    Confirm { prompt: String },
    /// Expects the index of the chosen option; the option's text is accepted too.
    Select { prompt: String, options: Vec<String> },
    /// Expects a string. A `secret` one (a key, a password) should not be shown as it is typed.
    Input {
        prompt: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        secret: bool,
    },
    /// Expects `true` to approve the plan, or a string of feedback to revise it.
    Plan { path: String, plan: String },
    /// Expects no answer. `level` is `info`, `warn` or `error`.
    Notify { level: String, text: String },
}

/// One line from azazel. `type` names the frame; frames of an agent carry its id as `agent`
/// (`lead` for the main agent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// The first line: which protocol and which azazel answer.
    Hello {
        protocol: u32,
        version: String,
    },
    Response(Response),
    AgentStart {
        agent: String,
    },
    /// `turn` counts the lead's model requests from 1 within each prompt.
    TurnStart {
        agent: String,
        turn: u32,
    },
    MessageStart {
        agent: String,
    },
    Stream {
        agent: String,
        event: StreamEvent,
    },
    /// The complete assistant message: `role`, `content` blocks and `meta`.
    MessageEnd {
        agent: String,
        message: Value,
    },
    ToolStart {
        agent: String,
        call_id: String,
        name: String,
        input: Value,
    },
    /// Progress output of a running tool.
    ToolUpdate {
        agent: String,
        call_id: String,
        text: String,
        /// `text` is the last lines the tool printed and replaces what was shown; otherwise it
        /// is one more line.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replace: bool,
    },
    ToolEnd {
        agent: String,
        call_id: String,
        output: ToolOutput,
        duration_ms: u64,
    },
    TurnEnd {
        agent: String,
    },
    /// The message in progress was discarded and the same model is asked again.
    Retry {
        agent: String,
        attempt: u32,
        model: String,
        /// The model had sent nothing for this many seconds; absent after any other failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stalled_secs: Option<u64>,
        /// How many times the model is asked in all before the chain moves on; absent when
        /// that is unlimited or not known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_attempts: Option<u32>,
        /// How long the router waits before it asks again, in milliseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay_ms: Option<u64>,
        /// Why the attempt failed, as the provider said it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The chain moved on to its next model.
    Fallback {
        agent: String,
        from: String,
        to: String,
    },
    /// The provider's account `from` ran out of allowance until `until` (Unix seconds);
    /// requests now use account `to`.
    Account {
        agent: String,
        provider: String,
        from: String,
        to: String,
        until: u64,
        limit: String,
    },
    /// The session moved to profile `to`. Not tied to an agent.
    ProfileChanged {
        /// The profile the session was on, when it was on one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        to: String,
        /// Why: `user` (a command, from a client or the terminal), `limit` (the provider
        /// reported a usage limit) or `reset` (the limit window has reset).
        reason: String,
        /// For `limit`: when the window resets, in Unix seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
        /// For `limit`: which limit it was, e.g. `5-hour limit`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<String>,
    },
    /// The model refused the service tier and is asked without one from now on.
    TierDropped {
        agent: String,
        model: String,
        tier: String,
    },
    /// A stream rule matched; the message in progress was discarded and the request is sent
    /// again.
    Rule {
        agent: String,
        name: String,
    },
    Compaction {
        agent: String,
        tokens_before: u64,
        tokens_after: u64,
    },
    /// The work a prompt started is over. `stop` is `stop`, `length`, `tool_use`, `error` or
    /// `aborted`.
    Idle {
        agent: String,
        stop: String,
    },
    Failed {
        agent: String,
        error: String,
    },
    /// Sent whenever a sub-agent appears or changes state.
    Roster {
        members: Vec<Member>,
    },
    /// A tool call needs the user's permission. The agent waits for the matching
    /// `approval_response`.
    ApprovalRequest {
        request_id: String,
        agent: String,
        tool: String,
        /// `read`, `write`, `exec` or `net`.
        access: String,
        /// What permission rules are matched against: paths, commands, `domain:<host>`.
        subjects: Vec<String>,
        input: Value,
        /// The allow rule that `always` would add.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        /// A remark to show, such as a cost warning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// The call would destroy work: approval should not be the answer a stray key gives.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        destructive: bool,
        /// The call reads or writes a secret file (`.env`, a private key): `session` allows that
        /// file until the session ends, and `always` is not on offer.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        secret: bool,
        /// The background job (`lua-1`) whose script is paused at this call; it runs again from
        /// the start once the request is answered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<String>,
    },
    /// An agent or extension asks the user something.
    UiRequest {
        request_id: String,
        #[serde(flatten)]
        request: UiRequest,
    },
    /// A piece of the answer to the `btw` command with this `id`.
    BtwDelta {
        id: Value,
        delta: String,
    },
    /// What an agent was handed from outside its own work. Without `agent` (background
    /// sessions only) it is what a client sent the lead: `kind` is `prompt`, `steer` or
    /// `follow_up`; it is in the log, in what a later client is replayed and in what the other
    /// clients read, never in what the sender itself reads. With `agent` it is what a sub-agent
    /// was given: `kind` is `prompt`, for the task it started with and for every message sent
    /// to it since.
    UserInput {
        kind: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    /// The colours of the terminal theme changed: the theme setting was changed with
    /// `set_setting`, or in the terminal UI a browser shares.
    Theme(ThemeColors),
    /// Background sessions only: the `approval_request` or `ui_request` with this id no longer
    /// waits. Another client answered it, or whoever asked gave up.
    RequestClosed {
        request_id: String,
    },
    /// One picture of the tab the agents' browser shows, sent to the clients that asked for it
    /// with `browser_watch`. Ephemeral: it has no `at`, is never written to the log and is never
    /// replayed. `data` is a base64 JPEG; `seq` counts the frames of the session.
    BrowserFrame {
        tab: String,
        data: String,
        width: u32,
        height: u32,
        seq: u64,
    },
    /// What the agents' browser is doing, sent to every attached client whenever the tab, its
    /// address, the hold or whether a browser runs changes. Ephemeral like `browser_frame`.
    BrowserState(BrowserState),
    /// A frame this version of the crate does not know.
    #[serde(other)]
    #[schemars(skip)]
    Unknown,
}

impl Frame {
    /// The agent an event frame belongs to.
    pub fn agent(&self) -> Option<&str> {
        match self {
            Frame::AgentStart { agent }
            | Frame::TurnStart { agent, .. }
            | Frame::MessageStart { agent }
            | Frame::Stream { agent, .. }
            | Frame::MessageEnd { agent, .. }
            | Frame::ToolStart { agent, .. }
            | Frame::ToolUpdate { agent, .. }
            | Frame::ToolEnd { agent, .. }
            | Frame::TurnEnd { agent }
            | Frame::Retry { agent, .. }
            | Frame::Fallback { agent, .. }
            | Frame::Account { agent, .. }
            | Frame::TierDropped { agent, .. }
            | Frame::Rule { agent, .. }
            | Frame::Compaction { agent, .. }
            | Frame::Idle { agent, .. }
            | Frame::Failed { agent, .. }
            | Frame::ApprovalRequest { agent, .. } => Some(agent),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------ results

/// Token and cost totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// Absent when no price is known for the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// The session as `get_state` and the session commands report it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct State {
    pub session_id: String,
    /// The session file.
    #[serde(default)]
    pub session_file: String,
    /// The session's name, once it has one.
    #[serde(default)]
    pub name: Option<String>,
    pub cwd: String,
    /// The permission mode.
    pub mode: String,
    /// Whether the lead is working.
    pub busy: bool,
    /// The lead's request number within the current prompt; 0 before the first.
    pub turn: u32,
    /// The model that answered most recently.
    pub model: Option<String>,
    /// The virtual model (`virtual/<name>`) that picked `model` for that request, when the
    /// chain names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The virtual models extensions define, as `virtual/<name>`; `set_model` takes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub virtual_models: Vec<String>,
    /// The lead's model chain as given, e.g. `["@default"]`.
    pub chain: Vec<String>,
    pub thinking: String,
    /// The service tier, when one is on.
    #[serde(default)]
    pub tier: Option<String>,
    pub context_tokens: u64,
    pub context_window: u64,
    /// Totals of the lead.
    pub usage: Usage,
    /// How many model requests the lead has made.
    #[serde(default)]
    pub requests: u32,
    /// The roster.
    pub agents: Vec<Member>,
    /// The checked-out git branch of the project.
    #[serde(default)]
    pub branch: Option<String>,
    /// Whether coordinator mode is on.
    #[serde(default)]
    pub coordinator: bool,
    /// The primary agent the lead is (`build`, `plan`, ...).
    #[serde(default)]
    pub agent: String,
    /// The primary agents `set_agent` takes, in the order a client steps through them.
    #[serde(default)]
    pub primary_agents: Vec<PrimaryAgent>,
    /// The open goal, in a few words.
    #[serde(default)]
    pub goal: Option<String>,
    /// The lead's task list.
    #[serde(default)]
    pub todo: Vec<Todo>,
    /// The model each role (`default`, `smol`, ...) resolves to first.
    #[serde(default)]
    pub roles: std::collections::BTreeMap<String, String>,
    /// The profile the session last moved to, or the one `--profile` named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The profiles `set_profile` takes: the built-in ones, then those of the configuration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProfileInfo>,
    /// Where the session's last run stopped, when the end of a process cut it short and
    /// `continue_run` can go on with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<String>,
    /// Whether the terminal shows the model's thinking (`tui.show_thinking`), so that a client
    /// can follow it.
    #[serde(default)]
    pub show_thinking: bool,
    /// Seconds until the provider drops the lead's prompt cache, when nothing reads it first;
    /// `0` once it has run out. Absent while the lead works, and when the cache is too small
    /// to matter. Counted from the moment the state was made: a client that shows a clock
    /// starts it when the answer arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_left_secs: Option<u64>,
    /// How long the provider keeps the cache, in seconds, with `cache_left_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_secs: Option<u64>,
    /// Whether the code was checked since it was last changed: absent while nothing was changed,
    /// `false` after a change to a file other than prose, `true` once a build, test or lint ran after
    /// it. Counts the changes and checks of every agent of the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
}

/// A profile: a named set of role chains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileInfo {
    pub name: String,
    /// `builtin`, `user` (the home config) or `project`.
    pub source: String,
    /// Whether every provider the profile names exists and has a login.
    pub usable: bool,
    /// The providers the profile names that have no login.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_logins: Vec<String>,
    /// The providers the profile names that this session has none of (a typo, or an extension
    /// that is not installed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_providers: Vec<String>,
    /// The first model of each role the profile sets, as `provider/id[:level]`.
    #[serde(default)]
    pub roles: std::collections::BTreeMap<String, String>,
}

/// A primary agent: a persona the lead can be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrimaryAgent {
    pub name: String,
    pub description: String,
    /// `#rrggbb`, when the definition gives the agent a colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// The data of a `prompt` that carries a `request_id`, and of `continue_run`: where a run
/// stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunState {
    /// The `request_id` of the prompt that started the run, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The run as the session file names it; absent until the lead has taken the prompt up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// `queued` (the lead has not taken it up yet), `running` or `ended`.
    pub state: String,
    /// Whether this answers a prompt that had been sent before: nothing was started.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub duplicate: bool,
    /// How an ended run ended: a stop reason of `idle`, or `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<String>,
    /// The last thing the lead said in the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The data of `agent_mention`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Mentioned {
    /// The agent the text went to.
    pub agent: String,
    /// Whether the agent was started for it.
    pub started: bool,
}

/// One item of the lead's task list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Todo {
    pub text: String,
    /// `pending`, `in_progress` or `done`.
    pub status: String,
}

/// The data of `set_model`: the usable models the chain resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Models {
    pub models: Vec<String>,
}

/// Where `set_role` keeps a change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SaveTo {
    /// This session only.
    #[default]
    Session,
    /// The home config, `~/.azazel/config.toml`.
    Home,
    /// The project's `.azazel/config.toml`; refused in a project that is not trusted.
    Project,
}

/// Where a role's chain comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// Azazel's own fallback (`smol` is `@default` until set).
    Builtin,
    Home,
    Project,
    ProjectLocal,
    /// Changed in this session and not saved.
    Session,
    /// Not set anywhere: the role resolves to nothing.
    Unset,
}

/// One model role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoleRow {
    pub role: String,
    /// As configured, e.g. `["anthropic/claude-opus-5-5:high", "@slow"]`.
    pub chain: Vec<String>,
    pub source: RoleSource,
    /// The built-in value the role falls back to when nothing sets it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin: Vec<String>,
    /// The usable models the chain resolves to, in order, as `provider/id[:level]`.
    pub resolved: Vec<String>,
    /// The thinking level of the first model, when the chain names one with the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

/// The data of `list_roles`, `set_role`, `save_preset` and `apply_preset`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Roles {
    pub roles: Vec<RoleRow>,
    /// The names of the saved presets.
    pub presets: Vec<String>,
    /// Whether `set_role` may write the project's config (the project is trusted).
    pub project: bool,
}

/// One saved session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRow {
    pub id: String,
    /// The session file.
    pub path: String,
    pub name: Option<String>,
    /// When it was created and when its last entry was written, RFC 3339.
    pub created: String,
    pub updated: String,
    /// The first message you sent.
    pub first_prompt: Option<String>,
    pub messages: u64,
    pub prompts: u64,
    pub pinned: bool,
    pub usage: Usage,
    /// With a `query`: the lines that matched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<String>,
}

/// One message of a branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MessageRow {
    pub entry_id: String,
    pub timestamp: String,
    /// `role` (`user`, `assistant`, `tool`), `content` blocks and, for replies, `meta`.
    pub message: Value,
}

/// One entry of the session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TreeRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    /// `message`, `checkpoint`, `model_change`, `compaction`, `label`, ...
    pub kind: String,
    /// For a message: `user`, `assistant` or `tool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// For a message: the start of its text. For a `label` or a `session_info`: the label or
    /// the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// For a `label`: the entry it labels. For a `checkpoint`: the entry it was taken after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// Whether the entry is on the branch that ends at the current leaf, or, for an entry of a
    /// run's journal (`run_started`, ...), hangs from that branch.
    pub current: bool,
}

/// One model of `list_models`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelRow {
    /// `provider/id`.
    pub model: String,
    pub provider: String,
    pub id: String,
    /// What the model is called in lists, when that is more than its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub context_window: u64,
    pub max_output: u64,
    pub vision: bool,
    pub reasoning: bool,
    /// The thinking levels the model takes; empty for every level when `reasoning` is set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_levels: Vec<String>,
    /// USD per million tokens.
    pub cost: Option<ModelCost>,
    /// Its provider has a credential, or needs none.
    pub usable: bool,
    /// Its provider is used through a subscription sign-in: it is not paid by the token.
    #[serde(default)]
    pub login: bool,
}

/// One provider of `list_providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    /// It has a credential, or needs none.
    pub usable: bool,
    /// The credential in use is a subscription sign-in.
    pub login: bool,
    /// Where the credential comes from (`OPENAI_API_KEY`, `auth.json (login)`), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The environment variables a key is read from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// It can be signed in to with an account.
    pub oauth: bool,
    /// How many models it has: as it listed them this session, else as the catalogue knows them.
    pub models: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
}

/// One usage window of an account, as the provider last reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Window {
    /// `5h`, `7d`, `7d-fable`, `overage`.
    pub name: String,
    /// The share that is used, 0.0 to 1.0.
    pub used: f32,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// `allowed`, `allowed_warning`, `rejected`, or whatever else the provider said.
    pub status: String,
    /// What the window counts when that is not every request: a model family such as `Fable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// An account that takes no requests until a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageBlock {
    /// Unix seconds until which the account is left alone.
    pub until: u64,
    /// Unix seconds at which the provider last refused.
    #[serde(default)]
    pub since: u64,
    /// The window that ran out, by name, when the provider named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// The models it holds for, as the scope of that window; absent for every model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// The usage windows of one provider account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AccountUsage {
    pub provider: String,
    pub account: String,
    pub windows: Vec<Window>,
    /// Unix seconds until which the account is out of allowance for every model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_until: Option<u64>,
    /// Every block of the account, those for the models of one scope included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<UsageBlock>,
}

/// The data of `shell`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShellResult {
    pub output: String,
    /// `exit code: 0`, `killed`, or why the command could not start.
    pub status: String,
}

/// The data of `export`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Exported {
    pub path: String,
}

/// The data of `btw`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BtwAnswer {
    pub answer: String,
    /// The model that answered.
    pub model: String,
    /// Tools the model tried to use anyway; those calls were dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_tools: Vec<String>,
}

/// The data of `handoff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Handoff {
    /// The session that is open now, which continues the work.
    pub session_id: String,
    /// The session the work was handed off from.
    pub from: String,
}

/// The data of `get_session`: the session file's first line and its entries, as they are
/// stored, and the entry the conversation continues from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCopy {
    /// The session file.
    pub path: String,
    pub header: Value,
    pub entries: Vec<Value>,
    pub leaf: Option<String>,
}

/// One command of `list_commands`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandRow {
    pub name: String,
    pub description: String,
    /// Where the command comes from.
    pub source: CommandOrigin,
}

/// Where a command of `list_commands` comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandOrigin {
    /// Azazel itself or one of its extensions: a report, `/fast`, `/memory`, ...
    Builtin,
    /// A skill, named `skill:<name>`.
    Skill,
    /// A prompt of an MCP server, named `mcp:<server>:<prompt>`.
    Mcp,
    /// A Markdown file of the user's or the project's `commands` directory.
    Custom,
    /// A command a Luau script registered.
    Script,
}

/// The data of `attach`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Attached {
    /// The id of the background session, as `azazel ps` lists it.
    pub host: String,
    /// The process that runs it.
    pub pid: u32,
    pub session_id: String,
    /// The session file.
    pub session_file: String,
    /// How many clients are attached, this one counted.
    #[serde(default)]
    pub clients: u32,
    /// Where the session stands at this moment.
    #[serde(default)]
    pub snapshot: Snapshot,
    /// The `at` the next frame will have: where live frames begin.
    #[serde(default)]
    pub cursor: u64,
    /// True when `since` named a frame the log does not hold; the client resynchronises with
    /// `get_messages`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gap: bool,
}

/// A background session at the moment a client attaches: what a client needs to show the work
/// in progress before the frames that follow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Snapshot {
    /// Whether the lead is working.
    pub busy: bool,
    /// Why the lead's latest run failed, cleared when its next run starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The last messages of the conversation, oldest first.
    pub tail: Vec<MessageRow>,
    /// The replies being written, one per agent that is writing one.
    pub streaming: Vec<Streaming>,
    /// The tool calls that are running.
    pub tools: Vec<RunningTool>,
    /// What clients sent the lead that it has not taken up yet, oldest first.
    pub queue: Vec<Queued>,
    /// The ids of the requests that wait for an answer; the requests follow as frames.
    pub pending: Vec<String>,
}

/// A reply in the making.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Streaming {
    pub agent: String,
    /// The text so far.
    pub text: String,
    /// The thinking so far.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
}

/// A tool call that has started and not ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunningTool {
    pub agent: String,
    pub call_id: String,
    pub name: String,
    pub input: Value,
    /// For how long it has run.
    pub elapsed_ms: u64,
}

/// Input the lead has not taken up yet. `kind` is `prompt`, `steer` or `follow_up`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Queued {
    pub kind: String,
    pub text: String,
}

/// The data of `run_command`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommandResult {
    /// The command finished with nothing to show.
    Done,
    /// The command produced this text for the user.
    Show { text: String },
    /// The command sent this text to the lead as a prompt.
    Prompt { text: String },
}

/// One setting of `get_settings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SettingRow {
    /// Where it lives in the config file, as `table.key`.
    pub path: String,
    /// The page of the settings screen it is on (`Appearance`), and the heading it stands under.
    pub page: String,
    pub group: String,
    pub label: String,
    /// How it is edited: `switch` (a boolean), `choice` (one of `choices`), `theme` (one of
    /// `choices`, the themes there are), `number`, `text`, `list` (of texts), `table` (of names
    /// to values) or `role` (a model chain, which `set_role` changes).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// The running session takes a new value at once; the others apply to the next session.
    pub live: bool,
    pub description: String,
    /// The value in effect; `null` when nothing sets it and it has no default.
    pub value: Value,
    /// `null` for a setting with no default (the `default` role).
    pub default: Value,
    /// Who set the value in effect: `session`, `flag` (the command line), `env`, `local`
    /// (`config.local.toml`), `project` or `home`; absent when it is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// A project's config may set it only once the project is trusted.
    pub gated: bool,
}

/// The data of `get_settings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SettingList {
    pub settings: Vec<SettingRow>,
    /// The pages in the order of the settings screen's tabs.
    pub pages: Vec<String>,
    /// Whether the project is trusted, which `set_setting` with `scope: project` needs.
    pub project: bool,
    /// The files `home` and `project` write.
    pub home_config: String,
    pub project_config: String,
}

/// The data of `set_setting`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SettingChanged {
    /// The setting as it is now.
    pub setting: SettingRow,
    /// What happened, as the terminal says it (`tui.theme = nord`).
    pub message: String,
}

/// The data of `get_theme` and the `theme` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ThemeColors {
    /// The theme as the setting names it (`auto`, `nord`, a file's name).
    pub name: String,
    /// Whether the palette is one for a light background.
    pub light: bool,
    /// Every colour role (`text`, `muted`, `accent`, `code_bg`, `user_bg`, `diff_add_bg`, ...) as
    /// `#rrggbb`. The terminal's own colours are given as a usual dark or light terminal has them.
    pub roles: std::collections::BTreeMap<String, String>,
}

/// One category of a [`ContextReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextCategory {
    /// `system`, `tools`, `messages`, `tool_results` or `images`.
    pub kind: String,
    /// The part, the MCP server or the kind of message; empty for the rest of its kind.
    pub name: String,
    /// The row's label as `/context` shows it.
    pub label: String,
    /// Parts, tools, blocks or images counted here.
    pub items: u64,
    /// Tokens by the counter compaction uses.
    pub estimate: u64,
    /// The estimate scaled so that the categories add up to what the provider counted.
    pub tokens: u64,
}

/// The data of `get_context`: the token breakdown of the lead's last request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextReport {
    pub agent: String,
    /// Which of the agent's requests this was, counted from 1.
    pub request: u32,
    /// The `provider/model` that answered; `null` until one has.
    #[serde(default)]
    pub model: Option<String>,
    /// The model's context window; 0 when unknown.
    pub window: u64,
    /// The sum of the categories' estimates.
    pub estimate: u64,
    /// Input tokens the provider counted, cached ones included.
    #[serde(default)]
    pub reported: Option<u64>,
    /// The sum of the categories' tokens: `reported` when there is one, else `estimate`.
    pub tokens: u64,
    pub categories: Vec<ContextCategory>,
}

/// One model's requests in a [`CostReport`], as the prompt cache counted them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelSpend {
    pub model: String,
    pub requests: u32,
    /// Input tokens that were neither read from the cache nor written to it.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The share of the input read from the cache, in percent; absent when there was no input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_rate: Option<u64>,
}

/// Requests made for one purpose outside the conversation (`btw`, `name`, `memory`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SideSpend {
    pub purpose: String,
    pub usage: Usage,
}

/// The data of `get_cost`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CostReport {
    /// The lead's requests and what they used; `usage.cost_usd` is absent when no price is known.
    pub requests: u32,
    pub usage: Usage,
    /// What the figure leaves out, when something is (a service tier without prices).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The share of every agent's input read from the prompt cache, in percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<u64>,
    /// Every agent's requests by model.
    pub models: Vec<ModelSpend>,
    /// Requests made outside the conversation, by purpose.
    pub side: Vec<SideSpend>,
    /// The requests that kept the prompt cache warm; absent when warming is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warming: Option<SideSpend>,
}

/// The data of `trust_project`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrustState {
    /// The project root the decision is about.
    pub root: String,
    /// Whether the project is trusted in this run.
    pub trusted: bool,
    /// The decision that is kept for the next start; `null` when there is none.
    pub saved: Option<bool>,
    pub message: String,
}

/// The data of `add_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DirList {
    /// Every directory agents may work in.
    pub dirs: Vec<String>,
    pub message: String,
}

/// The data of `forget_approvals`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Forgotten {
    /// How many rules and approvals were forgotten.
    pub count: u64,
    pub message: String,
}

/// What a client does in the agents' browser while it has taken it over. `x` and `y` are
/// fractions (0 to 1) of the frame the client sees, from its top left corner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserInput {
    /// A click, or with `count` 2 a double click: the host plays every press of it, the first
    /// and the second. With `continues` the presses before the last one were sent already (the
    /// second click of a slow double click, after its first went as a click of its own), and
    /// the host plays the last press only, as press number `count`.
    Click {
        x: f64,
        y: f64,
        #[serde(default)]
        button: MouseButton,
        #[serde(default)]
        count: u8,
        #[serde(default)]
        continues: bool,
    },
    /// Scrolls the wheel at the point by `dy` pixels (down is positive).
    Scroll { x: f64, y: f64, dy: f64 },
    /// Presses keys, as the browser tool's `press` does (`Enter`, `Control+A`, `Shift+Tab`).
    Key { keys: String },
    /// Types text into the focused field.
    Text { text: String },
    /// Goes to an http or https address (or `about:blank`).
    Navigate { url: String },
    /// One page back in the history.
    Back,
    /// One page forward in the history, after going back.
    Forward,
    /// Loads the page again.
    Reload,
}

/// The button of a click in [`BrowserInput`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Middle,
    Right,
}

/// The data of the browser commands and the `browser_state` frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct BrowserState {
    /// Whether a browser is running.
    pub running: bool,
    /// Whether its window is shown on the machine that runs azazel.
    pub visible: bool,
    /// The tab that is shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether a client has taken the browser over and the agents wait.
    pub held: bool,
    /// Whether that client is the one this state is sent to.
    pub held_by_you: bool,
    /// How many clients watch the browser.
    pub watchers: u32,
}

/// The JSON Schema of the protocol: a line is a [`Request`] or a [`Frame`]; the results are
/// among the definitions.
pub fn schema() -> Value {
    let mut generator = schemars::SchemaGenerator::default();
    let request = generator.subschema_for::<Request>();
    let frame = generator.subschema_for::<Frame>();
    // Results are reachable from no line, so they are named here to be defined.
    generator.subschema_for::<State>();
    generator.subschema_for::<Models>();
    generator.subschema_for::<SessionRow>();
    generator.subschema_for::<MessageRow>();
    generator.subschema_for::<TreeRow>();
    generator.subschema_for::<ModelRow>();
    generator.subschema_for::<ProviderRow>();
    generator.subschema_for::<Roles>();
    generator.subschema_for::<AccountUsage>();
    generator.subschema_for::<ShellResult>();
    generator.subschema_for::<Exported>();
    generator.subschema_for::<BtwAnswer>();
    generator.subschema_for::<Handoff>();
    generator.subschema_for::<CommandResult>();
    generator.subschema_for::<Attached>();
    generator.subschema_for::<RunState>();
    generator.subschema_for::<Mentioned>();
    generator.subschema_for::<SessionCopy>();
    generator.subschema_for::<CommandRow>();
    generator.subschema_for::<CommandOrigin>();
    generator.subschema_for::<SettingList>();
    generator.subschema_for::<SettingChanged>();
    generator.subschema_for::<ContextReport>();
    generator.subschema_for::<CostReport>();
    generator.subschema_for::<TrustState>();
    generator.subschema_for::<DirList>();
    generator.subschema_for::<Forgotten>();
    generator.subschema_for::<BrowserState>();
    let definitions = generator.take_definitions(true);
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "azazel RPC protocol",
        "description": format!(
            "Protocol {PROTOCOL} of `azazel rpc`. Each line is one JSON object: a Request from the \
             client, or a Frame from azazel."
        ),
        "oneOf": [request, frame],
        "$defs": definitions,
    })
}
