//! What scripts do with models: name them, have one judge a question, and pick the model of a
//! request for a virtual model.

use azazel_agent::{AgentInfo, Services};
use azazel_ai::{ModelSpec, Role, Route, RouteRequest, ThinkingLevel};
use azazel_core::classify::classify;
use azazel_core::side::Side;
use mlua::{Lua, Table};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::convert::to_lua;
use crate::script::describe;

/// Characters of each message a router is shown; a router decides on the gist.
const ROUTED_TEXT_CHARS: usize = 2000;

/// The models `value` names: `"provider/model"`, `"@role"`, or a list of them.
pub(crate) fn chain_of(value: mlua::Value) -> Result<Vec<ModelSpec>, String> {
    let names: Vec<String> = match value {
        mlua::Value::String(one) => vec![one.to_string_lossy()],
        mlua::Value::Table(many) => many
            .sequence_values::<String>()
            .collect::<mlua::Result<_>>()
            .map_err(|e| describe(&e))?,
        other => {
            return Err(format!(
                "the model is a {}; pass \"provider/model\", \"@role\" or a list of them",
                other.type_name()
            ));
        }
    };
    if names.is_empty() {
        return Err("the list of models is empty".to_string());
    }
    names
        .iter()
        .map(|name| name.trim().parse::<ModelSpec>().map_err(|e| e.to_string()))
        .collect()
}

/// `classify(question, choices, { model })` as `function` of a script: the answer as
/// `{ choice, index, confidence, reason, model }`, with `index` counted from 1.
pub(crate) async fn judge(
    lua: &Lua,
    services: &Services,
    agent: &AgentInfo,
    function: &str,
    (question, choices, options): (String, Vec<String>, Option<Table>),
    cancel: CancellationToken,
) -> mlua::Result<Table> {
    let failed = |why: String| mlua::Error::runtime(format!("{function}: {why}"));
    let chain = match options {
        Some(options) => match options.get::<mlua::Value>("model")? {
            mlua::Value::Nil => None,
            model => Some(chain_of(model).map_err(failed)?),
        },
        None => None,
    };
    let side = services
        .get::<Side>()
        .ok_or_else(|| failed("no model can be asked in this session".to_string()))?;
    let answer = classify(&side, agent, &question, &choices, chain, cancel)
        .await
        .map_err(failed)?;
    let table = lua.create_table()?;
    table.set("choice", answer.choice)?;
    table.set("index", answer.index + 1)?;
    table.set("confidence", answer.confidence)?;
    table.set("reason", answer.reason)?;
    table.set("model", answer.model.to_string())?;
    Ok(table)
}

fn cut(text: String) -> String {
    match text.char_indices().nth(ROUTED_TEXT_CHARS) {
        Some((end, _)) => text[..end].to_string(),
        None => text,
    }
}

/// The `request` argument of a router function.
pub(crate) fn route_table(lua: &Lua, request: &RouteRequest<'_>) -> mlua::Result<mlua::Value> {
    let messages: Vec<Value> = request
        .request
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let tools: Vec<&str> = message.tool_calls().map(|(_, name, _)| name).collect();
            json!({ "role": role, "text": cut(message.text()), "tools": tools })
        })
        .collect();
    let failed = request
        .failed
        .map(|(model, error)| json!({ "model": model.to_string(), "error": error.to_string() }));
    let table = json!({
        "name": request.name,
        "reason": request.reason.as_str(),
        "thinking": request.thinking.to_string(),
        "previous": request.previous.as_ref().map(ToString::to_string),
        "failed": failed,
        "text": request.last_user_text().map(cut),
        "messages": messages,
    });
    to_lua(lua, &table)
}

/// What a router function returned: a model, a list of models, or
/// `{ model = ..., thinking = "high" }`.
pub(crate) fn routed(returned: mlua::Value) -> Result<Route, String> {
    let (models, thinking) = match returned {
        mlua::Value::Nil => return Err("the router returned nothing".to_string()),
        mlua::Value::Table(table) if table.contains_key("model").map_err(|e| describe(&e))? => {
            let model = table.get::<mlua::Value>("model").map_err(|e| describe(&e))?;
            let thinking = table
                .get::<Option<String>>("thinking")
                .map_err(|e| describe(&e))?
                .map(|level| level.parse::<ThinkingLevel>().map_err(|e| e.to_string()))
                .transpose()?;
            (model, thinking)
        }
        other => (other, None),
    };
    Ok(Route {
        chain: chain_of(models)?,
        thinking,
    })
}
