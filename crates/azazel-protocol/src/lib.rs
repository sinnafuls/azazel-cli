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

fn yes() -> bool {
    true
}

/// What a client can ask of the session. `type` names the command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Starts work on the input. While the lead is busy it is queued.
    Prompt {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
    },
    /// Hands the working lead input before its next model request.
    Steer { text: String },
    /// Queues input for when the lead would go idle.
    FollowUp { text: String },
    /// Stops the lead's current work.
    Abort,
    /// Answers with the session's [`State`].
    GetState,
    /// Changes the lead's model chain, e.g. `["provider/model", "@role"]`. Answers with
    /// [`Models`].
    SetModel { chain: Vec<String> },
    /// Changes the permission mode: `plan`, `ask`, `accept-edits` or `auto`.
    SetMode { mode: String },
    /// Messages a sub-agent by id, or `all`.
    AgentSend { to: String, message: String },
    /// Stops a sub-agent and the agents it started, or `all`.
    AgentStop { agent: String },
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
    /// Answers with a list of [`MessageRow`].
    GetMessages {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        leaf: Option<String>,
    },
    /// Every entry of the session file. Answers with a list of [`TreeRow`].
    GetTree,
    /// Continues the conversation from another entry of the tree. Answers with [`State`].
    SetLeaf { entry_id: String },
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
    /// The models that can be used. Answers with a list of [`ModelRow`].
    ListModels,
    /// What is known of each provider account's usage windows. Answers with a list of
    /// [`AccountUsage`].
    GetUsage,
    /// Runs a slash command by name (without the slash). Answers with [`CommandResult`].
    RunCommand {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        args: String,
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
}

impl Response {
    pub fn ok(id: Value, data: Value) -> Self {
        Response {
            id,
            success: true,
            data,
            error: None,
        }
    }

    pub fn err(id: Value, error: impl Into<String>) -> Self {
        Response {
            id,
            success: false,
            data: Value::Null,
            error: Some(error.into()),
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
        let mut out = serializer.serialize_struct("Response", 3)?;
        out.serialize_field("id", &self.id)?;
        out.serialize_field("success", &self.success)?;
        if self.success {
            out.serialize_field("data", &self.data)?;
        } else {
            out.serialize_field("error", self.error.as_deref().unwrap_or_default())?;
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
}

/// A question to the user. Answer with `ui_response` and a `value` of the kind shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiRequest {
    /// Expects a list of the chosen options, or one string when there are no options.
    Ask {
        question: String,
        options: Vec<String>,
        multi: bool,
    },
    /// Expects `true` or `false`.
    Confirm { prompt: String },
    /// Expects the index of the chosen option; the option's text is accepted too.
    Select { prompt: String, options: Vec<String> },
    /// Expects a string.
    Input { prompt: String },
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
    /// The roster.
    pub agents: Vec<Member>,
}

/// The data of `set_model`: the usable models the chain resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Models {
    pub models: Vec<String>,
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
    /// Whether the entry is on the branch that ends at the current leaf.
    pub current: bool,
}

/// One model of `list_models`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelRow {
    /// `provider/id`.
    pub model: String,
    pub context_window: u64,
    pub max_output: u64,
    pub vision: bool,
    pub reasoning: bool,
    /// USD per million tokens.
    pub cost: Option<ModelCost>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
}

/// One usage window of an account, as the provider last reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Window {
    /// `5h`, `7d`, `7d-opus`, `overage`.
    pub name: String,
    /// The share that is used, 0.0 to 1.0.
    pub used: f32,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// `allowed`, `allowed_warning`, `rejected`, or whatever else the provider said.
    pub status: String,
}

/// The usage windows of one provider account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AccountUsage {
    pub provider: String,
    pub account: String,
    pub windows: Vec<Window>,
    /// Unix seconds until which the account is out of allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_until: Option<u64>,
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
    generator.subschema_for::<AccountUsage>();
    generator.subschema_for::<ShellResult>();
    generator.subschema_for::<Exported>();
    generator.subschema_for::<BtwAnswer>();
    generator.subschema_for::<CommandResult>();
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
