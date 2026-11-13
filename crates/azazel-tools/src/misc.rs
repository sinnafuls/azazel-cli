//! `todo` and `ask_user`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::{Block, Message, Role, SystemPart};
use azazel_core::hooks::Wake;
use azazel_core::session::{Entry, EntryKind};
use azazel_core::{
    AskKind, Event, EventHandler, Excerpt, ExtCtx, Level, Outcome, Post, Question, Ui, UiBridge, UiError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::util::parse_input;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    pub status: TodoStatus,
}

/// Each agent's task list.
#[derive(Default)]
pub struct TodoStore {
    lists: Mutex<HashMap<String, Vec<TodoItem>>>,
}

impl TodoStore {
    pub fn get(&self, agent: &str) -> Vec<TodoItem> {
        self.lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(agent)
            .cloned()
            .unwrap_or_default()
    }

    /// Replaces `agent`'s list.
    pub fn set(&self, agent: &str, items: Vec<TodoItem>) {
        self.lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(agent.to_string(), items);
    }

    /// Makes the lists what a session's transcript says they are: `agent` gets the list `branch`
    /// last recorded (see [`recorded`]) and every other agent's list goes, because their agents
    /// ended with the session that was open before.
    pub fn rehydrate(&self, agent: &str, branch: &[Entry]) {
        let items = recorded(branch);
        let mut lists = self.lists.lock().unwrap_or_else(PoisonError::into_inner);
        lists.clear();
        if !items.is_empty() {
            lists.insert(agent.to_string(), items);
        }
    }

    /// What `agent` is reminded of before each request while work on its list remains: how far
    /// it is, and the list. `None` for an empty list and for one that is all done.
    pub fn reminder(&self, agent: &str) -> Option<String> {
        let lists = self.lists.lock().unwrap_or_else(PoisonError::into_inner);
        let items = lists.get(agent)?;
        let done = items
            .iter()
            .filter(|item| item.status == TodoStatus::Done)
            .count();
        (done < items.len()).then(|| {
            format!(
                "[Plan] {done}/{} done (your todo list; keep it current with `todo`)\n{}",
                items.len(),
                render(items)
            )
        })
    }
}

/// The session entry `/todo` writes when the user changes the list, so that the change is still
/// there when the session is opened again.
pub const TODO_ENTRY: &str = "todo";

/// What `/todo` records for a list the user changed.
pub fn todo_entry(items: &[TodoItem]) -> EntryKind {
    EntryKind::Custom {
        custom_type: TODO_ENTRY.to_string(),
        data: json!({"items": items}),
    }
}

fn items_of(value: &Value) -> Option<Vec<TodoItem>> {
    serde_json::from_value(value.get("items")?.clone()).ok()
}

/// The task list a branch of a session ends with: the items of the last `todo` call that
/// succeeded, or of the user's later change to them.
pub fn recorded(branch: &[Entry]) -> Vec<TodoItem> {
    // Calls whose result has not been seen yet, by call id.
    let mut asked: HashMap<&str, &Value> = HashMap::new();
    let mut current = Vec::new();
    for entry in branch {
        match &entry.kind {
            EntryKind::Custom { custom_type, data } if custom_type == TODO_ENTRY => {
                if let Some(items) = items_of(data) {
                    current = items;
                }
            }
            EntryKind::Message { message } => record_message(message, &mut asked, &mut current),
            _ => {}
        }
    }
    current
}

fn record_message<'a>(
    message: &'a Message,
    asked: &mut HashMap<&'a str, &'a Value>,
    current: &mut Vec<TodoItem>,
) {
    for block in &message.content {
        match block {
            Block::ToolCall { id, name, input, .. } if message.role == Role::Assistant && name == "todo" => {
                asked.insert(id, input);
            }
            Block::ToolResult {
                call_id, is_error, ..
            } => {
                if let Some(input) = asked.remove(call_id.as_str())
                    && !is_error
                    && let Some(items) = items_of(input)
                {
                    *current = items;
                }
            }
            _ => {}
        }
    }
}

/// Puts the plan in front of an agent with every request while it has work left on its list.
/// The note joins the request's tail: it is sent after everything else, outside what the
/// provider caches, and is never part of the transcript.
pub(crate) struct PlanReminder;

#[async_trait]
impl EventHandler for PlanReminder {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        if let Event::BeforeRequest { agent, request } = event
            && let Some(note) = ctx
                .services
                .get::<TodoStore>()
                .and_then(|store| store.reminder(&agent.id))
        {
            request.tail.push(SystemPart {
                text: note,
                name: "plan".to_string(),
                cache_boundary: false,
            });
        }
        Outcome::Continue
    }
}

pub struct TodoTool {
    store: Arc<TodoStore>,
}

impl TodoTool {
    pub fn new(store: Arc<TodoStore>) -> Self {
        TodoTool { store }
    }
}

#[derive(Deserialize)]
struct TodoInput {
    items: Vec<TodoItem>,
}

fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "todo list cleared".to_string();
    }
    items
        .iter()
        .map(|item| {
            let mark = match item.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Done => "[x]",
            };
            format!("{mark} {}", item.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Replace your task list. Send the full list every time, each item with `text` and `status` \
         (`pending`, `in_progress` or `done`); at most one item may be `in_progress`. Use it for work \
         with three or more steps and keep it current as you go."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}
                        },
                        "required": ["text", "status"]
                    }
                }
            },
            "required": ["items"]
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
        let input: TodoInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.items.iter().any(|item| item.text.trim().is_empty()) {
            return ToolOutput::error("every item needs text");
        }
        let active = input
            .items
            .iter()
            .filter(|item| item.status == TodoStatus::InProgress)
            .count();
        if active > 1 {
            return ToolOutput::error(format!("{active} items are in_progress; at most one may be"));
        }
        let text = render(&input.items);
        let details = json!({"items": input.items});
        self.store.set(&ctx.agent_id, input.items);
        ToolOutput::text(text).with_details(details)
    }
}

pub struct AskUserTool;

/// One question as the agent writes it.
#[derive(Deserialize)]
struct AskOne {
    question: String,
    #[serde(default)]
    header: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<AskKind>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    diff: Option<String>,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    multi: bool,
    #[serde(default)]
    recommended: Option<String>,
    #[serde(default)]
    allow_freeform: bool,
}

#[derive(Deserialize)]
struct AskInput {
    /// The fields of a single question.
    #[serde(flatten)]
    one: Value,
    #[serde(default)]
    questions: Vec<AskOne>,
    #[serde(default, rename = "async")]
    later: bool,
}

/// Fewest and most options a choice may offer.
const OPTIONS: std::ops::RangeInclusive<usize> = 2..=8;
/// The most questions one call may put.
const MAX_QUESTIONS: usize = 6;
/// The longest name a question has in the row of tabs.
const MAX_HEADER: usize = 24;

/// How an option is compared with the others: case and spacing make no difference.
fn folded(option: &str) -> String {
    option
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

impl AskOne {
    /// The question to put to the user, or what is wrong with it.
    fn question(self) -> Result<Question, String> {
        if self.question.trim().is_empty() {
            return Err("`question` is empty".to_string());
        }
        let kind = self.kind.unwrap_or(match (self.options.is_empty(), self.multi) {
            (true, _) => AskKind::Text,
            (false, true) => AskKind::Multiselect,
            (false, false) => AskKind::Select,
        });
        let name = match kind {
            AskKind::Select => "select",
            AskKind::Multiselect => "multiselect",
            AskKind::Text => "text",
            AskKind::Confirm => "confirm",
        };
        if self.allow_freeform && kind != AskKind::Select {
            return Err(format!("`allow_freeform` goes with `select` only, not `{name}`"));
        }
        match kind {
            AskKind::Text | AskKind::Confirm if !self.options.is_empty() => {
                return Err(format!("a `{name}` question takes no `options`"));
            }
            AskKind::Text | AskKind::Confirm => {}
            AskKind::Select | AskKind::Multiselect => {
                if !OPTIONS.contains(&self.options.len()) {
                    return Err(format!(
                        "a `{name}` question needs {} to {} options, not {}",
                        OPTIONS.start(),
                        OPTIONS.end(),
                        self.options.len()
                    ));
                }
                let mut seen: Vec<String> = Vec::with_capacity(self.options.len() + 1);
                if self.allow_freeform {
                    seen.push(folded(Question::OTHER));
                }
                for option in &self.options {
                    let folded = folded(option);
                    if folded.is_empty() {
                        return Err("an option is empty".to_string());
                    }
                    if seen.contains(&folded) {
                        return Err(format!("the option `{option}` is offered twice"));
                    }
                    seen.push(folded);
                }
            }
        }
        // The recommended answer is one the user can give, written as the option is.
        let recommended = match self.recommended.filter(|option| !option.trim().is_empty()) {
            None => None,
            Some(_) if kind == AskKind::Text => {
                return Err("a `text` question has no option to recommend".to_string());
            }
            Some(wanted) => {
                let yes_no = ["yes".to_string(), "no".to_string()];
                let offered = match kind {
                    AskKind::Confirm => &yes_no[..],
                    _ => &self.options[..],
                };
                let found = offered.iter().find(|option| folded(option) == folded(&wanted));
                Some(found.cloned().ok_or_else(|| match kind {
                    AskKind::Confirm => {
                        format!("`recommended` is `yes` or `no` for a `confirm` question, not `{wanted}`")
                    }
                    _ => format!("`recommended` names `{wanted}`, which is not one of the options"),
                })?)
            }
        };
        let filled = |text: Option<String>| text.filter(|text| !text.trim().is_empty());
        let excerpt = match (filled(self.code), filled(self.diff)) {
            (Some(_), Some(_)) => return Err("give `code` or `diff`, not both".to_string()),
            (Some(code), None) => Some(Excerpt::Code(code)),
            (None, Some(diff)) => Some(Excerpt::Diff(diff)),
            (None, None) => None,
        };
        let header = filled(self.header).map(|header| header.trim().to_string());
        if let Some(header) = &header
            && header.chars().count() > MAX_HEADER
        {
            return Err(format!(
                "the `header` `{header}` is longer than {MAX_HEADER} characters"
            ));
        }
        Ok(Question {
            text: self.question,
            header,
            context: filled(self.context),
            excerpt,
            kind,
            options: self.options,
            recommended,
            allow_freeform: self.allow_freeform,
        })
    }
}

impl AskInput {
    /// The questions to put to the user, or what is wrong with the call. A call with `question`
    /// asks that one; a call with `questions` asks them together.
    fn questions(self) -> Result<Vec<Question>, String> {
        let single = self.one.get("question").is_some();
        if self.questions.is_empty() {
            if !single {
                return Err("give `question`, or `questions` for several".to_string());
            }
            let one: AskOne =
                serde_json::from_value(self.one).map_err(|e| format!("invalid arguments: {e}"))?;
            return Ok(vec![one.question()?]);
        }
        if single {
            return Err("give `question` or `questions`, not both".to_string());
        }
        if self.questions.len() > MAX_QUESTIONS {
            return Err(format!(
                "`questions` takes at most {MAX_QUESTIONS}, not {}",
                self.questions.len()
            ));
        }
        let several = self.questions.len() > 1;
        self.questions
            .into_iter()
            .enumerate()
            .map(|(at, one)| {
                one.question().map_err(|wrong| match several {
                    true => format!("question {}: {wrong}", at + 1),
                    false => wrong,
                })
            })
            .collect()
    }
}

/// What the agent is told when nobody answered. Not an error: the user saying nothing is an
/// answer, and the agent goes on without one.
pub const DECLINED: &str = "the user declined to answer";
/// What stands for the answer to a question of a form that the user left open.
const NO_ANSWER: &str = "(no answer)";

fn ask_details(question: &Question, answers: Option<&[String]>, reason: Option<&str>) -> Value {
    json!({
        "question": question.text,
        "context": question.context,
        "type": question.kind,
        "options": question.options,
        "recommended": question.recommended,
        "answers": answers,
        // An answer the user typed where options were offered.
        "freeform": question.allow_freeform
            && answers.is_some_and(|answers| answers.iter().any(|answer| !question.options.contains(answer))),
        "cancelled": answers.is_none(),
        "reason": reason,
    })
}

/// The details of a call that put several questions: those of each, and whether the form as a
/// whole was dismissed.
fn form_details(questions: &[Question], answers: Option<&[Vec<String>]>, reason: Option<&str>) -> Value {
    let each: Vec<Value> = questions
        .iter()
        .enumerate()
        .map(|(at, question)| {
            let given = answers.and_then(|answers| answers.get(at)).map(Vec::as_slice);
            ask_details(question, given, reason)
        })
        .collect();
    json!({"questions": each, "cancelled": answers.is_none(), "reason": reason})
}

/// The answers to several questions as the agent reads them: each question, numbered, with what
/// was answered under it.
fn form_text(questions: &[Question], answers: &[Vec<String>]) -> String {
    questions
        .iter()
        .enumerate()
        .map(|(at, question)| {
            let given = match answers.get(at).filter(|given| !given.is_empty()) {
                Some(given) => given.join("\n"),
                None => NO_ANSWER.to_string(),
            };
            format!("{}. {}\n{given}", at + 1, question.text)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Puts `questions` to the user and says what came of it: the text for the agent, and the
/// details for whoever shows the call.
async fn put(ui: &dyn UiBridge, questions: &[Question]) -> (String, Value) {
    let reason = |error: UiError| match error {
        UiError::Cancelled => "the user dismissed the question".to_string(),
        other => other.to_string(),
    };
    if let [question] = questions {
        return match ui.ask(question).await {
            Ok(answers) => {
                let text = match answers.is_empty() {
                    true => "(the user gave no answer)".to_string(),
                    false => answers.join("\n"),
                };
                (text, ask_details(question, Some(&answers), None))
            }
            Err(error) => (
                DECLINED.to_string(),
                ask_details(question, None, Some(&reason(error))),
            ),
        };
    }
    match ui.ask_form(questions).await {
        Ok(answers) => (
            form_text(questions, &answers),
            form_details(questions, Some(&answers), None),
        ),
        Err(error) => (
            DECLINED.to_string(),
            form_details(questions, None, Some(&reason(error))),
        ),
    }
}

/// The message in which the answers to an `async` call reach the agent that asked.
fn answer_message(questions: &[Question], said: &str) -> String {
    let body = match (questions, said == DECLINED) {
        // A form names its questions itself.
        (_, false) if questions.len() > 1 => said.to_string(),
        ([question], false) => format!("{}\n{said}", question.text),
        _ => {
            let asked: Vec<&str> = questions.iter().map(|question| question.text.as_str()).collect();
            format!("{DECLINED}: {}", asked.join(" / "))
        }
    };
    format!("<user-answer>\n{body}\n</user-answer>")
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for the answer. `type` is `select` (one of 2 to 8 `options`; \
         `allow_freeform` lets the user type another answer), `multiselect` (several of the options), \
         `text` (a typed answer) or `confirm` (yes or no); left out, it follows from `options` and \
         `multi`. Put what the user needs to know to answer in `context`, the code or the diff the \
         question is about in `code` or `diff`, and the option you would take in `recommended`. To ask \
         several things at once give `questions` (up to 6, each with the same fields and a short \
         `header`): the user answers them in one form. With `async` the call returns at once and the \
         answers reach you later as a message, so you can go on with work that does not depend on \
         them. Ask only what you cannot find out yourself and what changes what you do next. A user \
         who declines gives no answer: go on without one."
    }

    fn schema(&self) -> Value {
        let fields = json!({
            "question": {"type": "string"},
            "type": {"type": "string", "enum": ["select", "multiselect", "text", "confirm"]},
            "context": {"type": "string", "description": "Background shown above the question"},
            "code": {"type": "string", "description": "Code the question is about, shown with it"},
            "diff": {"type": "string", "description": "A unified diff the question is about, shown with it"},
            "options": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 8},
            "multi": {"type": "boolean", "description": "Without `type`: several options may be chosen"},
            "recommended": {"type": "string", "description": "The option you would take (`yes` or `no` for `confirm`)"},
            "allow_freeform": {"type": "boolean", "description": "`select` only: offer \"Other…\" and take a typed answer"}
        });
        let mut one = fields.clone();
        one["header"] =
            json!({"type": "string", "description": "A word or two naming the question in the form"});
        let mut properties = fields;
        properties["questions"] = json!({
            "type": "array",
            "description": "Several questions answered in one form, instead of `question`",
            "minItems": 1,
            "maxItems": MAX_QUESTIONS,
            "items": {"type": "object", "properties": one, "required": ["question"]}
        });
        properties["async"] = json!({
            "type": "boolean",
            "description": "Return at once; the answers arrive later as a message"
        });
        json!({"type": "object", "properties": properties})
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
        let input: AskInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let later = input.later;
        let questions = match input.questions() {
            Ok(questions) => questions,
            Err(wrong) => return ToolOutput::error(wrong),
        };
        let nobody = |reason: String| {
            let details = match &questions[..] {
                [question] => ask_details(question, None, Some(&reason)),
                several => form_details(several, None, Some(&reason)),
            };
            ToolOutput::text(DECLINED).with_details(details)
        };
        let Some(ui) = ctx.services.get::<Ui>() else {
            return nobody(UiError::NoUser.to_string());
        };
        if later {
            return ask_later(ui, questions, ctx);
        }
        let (text, details) = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("aborted by user"),
            answered = put(ui.0.as_ref(), &questions) => answered,
        };
        ToolOutput::text(text).with_details(details)
    }
}

/// Puts the questions without waiting: the answers reach the agent that asked as a message,
/// which steers it while it works and starts it again when it has stopped.
fn ask_later(ui: Arc<Ui>, questions: Vec<Question>, ctx: &ToolCtx) -> ToolOutput {
    let services = ctx.services.clone();
    let (agent, lead) = (ctx.agent_id.clone(), ctx.agent.depth == 0);
    let details = match &questions[..] {
        [question] => json!({"question": question.text, "async": true}),
        several => json!({
            "questions": several.iter().map(|question| json!({"question": question.text})).collect::<Vec<_>>(),
            "async": true,
        }),
    };
    let count = questions.len();
    tokio::spawn(async move {
        let (said, _) = put(ui.0.as_ref(), &questions).await;
        let message = answer_message(&questions, &said);
        // Whoever runs the agents knows how to reach each; the lead alone is reached as a
        // `wake` hook reaches it.
        let mut posted = services
            .get::<Post>()
            .is_some_and(|post| (post.0)(&agent, message.clone()));
        if !posted
            && lead
            && let Some(wake) = services.get::<Wake>()
        {
            (wake.0)(message);
            posted = true;
        }
        // The call that asked ended long ago: this is where the user sees what came of it.
        match (posted, said == DECLINED) {
            (true, false) => {
                ui.0.notify(Level::Info, &format!("your answer was sent to {agent}"))
            }
            (true, true) => ui.0.notify(
                Level::Info,
                &format!("{agent} was told that you declined to answer"),
            ),
            (false, _) => ui.0.notify(
                Level::Warn,
                &format!("your answer was not delivered: {agent} is no longer there"),
            ),
        }
    });
    let text = match count {
        1 => "The question is with the user. The answer will reach you as a message; go on with what \
              does not depend on it."
            .to_string(),
        count => format!(
            "The {count} questions are with the user. The answers will reach you as a message; go on \
             with what does not depend on them."
        ),
    };
    ToolOutput::text(text).with_details(details)
}
