//! Moving values between JSON and Luau, and events between the bus and a handler.

use azazel_agent::AgentInfo;
use azazel_ai::{Block, Message, SystemPart};
use azazel_core::{Event, EventKind, Level, Outcome, StartSource};
use mlua::serde::SerializeOptions;
use mlua::{Lua, LuaSerdeExt, Table};
use serde_json::{Value, json};

/// The events a script can handle, by the name it uses for them.
pub(crate) static EVENTS: [(&str, EventKind); 15] = [
    ("session_start", EventKind::SessionStart),
    ("session_end", EventKind::SessionEnd),
    ("user_prompt", EventKind::UserPrompt),
    ("before_agent_start", EventKind::BeforeAgentStart),
    ("turn_start", EventKind::TurnStart),
    ("turn_end", EventKind::TurnEnd),
    ("tool_call", EventKind::ToolCall),
    ("tool_result", EventKind::ToolResult),
    ("context", EventKind::Context),
    ("agent_spawn", EventKind::AgentSpawn),
    ("agent_end", EventKind::AgentEnd),
    ("pre_compact", EventKind::PreCompact),
    ("post_compact", EventKind::PostCompact),
    ("stop", EventKind::Stop),
    ("notification", EventKind::Notification),
];

/// A JSON value as a Luau value: objects and arrays become tables, `null` becomes `nil`.
pub(crate) fn to_lua(lua: &Lua, value: &Value) -> mlua::Result<mlua::Value> {
    const OPTIONS: SerializeOptions = SerializeOptions::new()
        .serialize_none_to_null(false)
        .serialize_unit_to_null(false);
    lua.to_value_with(value, OPTIONS)
}

/// A Luau value as JSON. A table with a sequence part becomes an array, any other table an
/// object; functions and other values JSON cannot express are an error.
pub(crate) fn from_lua(lua: &Lua, value: mlua::Value) -> mlua::Result<Value> {
    lua.from_value(value)
}

pub(crate) fn level_name(level: Level) -> &'static str {
    match level {
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    }
}

fn agent_json(agent: &AgentInfo) -> Value {
    json!({
        "id": agent.id,
        "def": agent.def,
        "parent": agent.parent,
        "depth": agent.depth,
        "session_id": agent.session_id,
    })
}

/// The agent an event is about, which is the one whose identity `ctx` carries.
pub(crate) fn agent_of(event: &Event) -> Option<&AgentInfo> {
    match event {
        Event::SessionStart { .. }
        | Event::SessionEnd
        | Event::UserPrompt { .. }
        | Event::Notification { .. } => None,
        Event::AgentSpawn { parent, .. } => Some(parent),
        Event::BeforeAgentStart { agent, .. }
        | Event::TurnStart { agent }
        | Event::TurnEnd { agent }
        | Event::ToolCall { agent, .. }
        | Event::ToolResult { agent, .. }
        | Event::Context { agent, .. }
        | Event::BeforeRequest { agent, .. }
        | Event::AgentEnd { agent, .. }
        | Event::PreCompact { agent, .. }
        | Event::PostCompact { agent }
        | Event::Stop { agent, .. }
        | Event::PermissionRequest { agent, .. } => Some(agent),
    }
}

/// The `ev` argument of a handler: the event's fields plus `name`.
pub(crate) fn event_table(lua: &Lua, name: &str, event: &Event) -> mlua::Result<Table> {
    let fields = match event {
        Event::SessionStart { source } => {
            let source = match source {
                StartSource::Startup => "startup",
                StartSource::Resume => "resume",
                StartSource::Clear => "clear",
                StartSource::Compact => "compact",
            };
            json!({ "source": source })
        }
        Event::SessionEnd => json!({}),
        Event::UserPrompt { text, images } => json!({ "text": text, "images": images.len() }),
        Event::BeforeAgentStart { agent, system } => {
            let system: Vec<&str> = system.iter().map(|part| part.text.as_str()).collect();
            json!({ "agent": agent_json(agent), "system": system })
        }
        Event::TurnStart { agent }
        | Event::TurnEnd { agent }
        | Event::PostCompact { agent }
        | Event::BeforeRequest { agent, .. }
        | Event::PermissionRequest { agent, .. } => json!({ "agent": agent_json(agent) }),
        Event::ToolCall {
            agent,
            call_id,
            tool,
            input,
            skip_prompt,
        } => json!({
            "agent": agent_json(agent),
            "call_id": call_id,
            "tool": tool,
            "input": input,
            "skip_prompt": skip_prompt,
        }),
        Event::ToolResult {
            agent,
            call_id,
            tool,
            output,
        } => json!({
            "agent": agent_json(agent),
            "call_id": call_id,
            "tool": tool,
            "content": output.text_content(),
            "is_error": output.is_error,
        }),
        Event::Context { agent, messages } => json!({
            "agent": agent_json(agent),
            "messages": serde_json::to_value(messages).map_err(mlua::Error::external)?,
        }),
        Event::AgentSpawn { parent, def, prompt } => {
            json!({ "parent": agent_json(parent), "def": def, "prompt": prompt })
        }
        Event::AgentEnd { agent, status } => json!({ "agent": agent_json(agent), "status": status }),
        Event::PreCompact { agent, manual } => json!({ "agent": agent_json(agent), "manual": manual }),
        Event::Stop {
            agent,
            stop_hook_active,
        } => json!({ "agent": agent_json(agent), "stop_hook_active": stop_hook_active }),
        Event::Notification { level, text } => json!({ "level": level_name(*level), "text": text }),
    };
    let mlua::Value::Table(table) = to_lua(lua, &fields)? else {
        return Err(mlua::Error::runtime("an event did not convert to a table"));
    };
    table.set("name", name)?;
    Ok(table)
}

/// Whether blocking `kind` does anything.
fn blockable(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::UserPrompt
            | EventKind::ToolCall
            | EventKind::AgentSpawn
            | EventKind::PreCompact
            | EventKind::Stop
    )
}

/// Applies the table a handler returned: `{ block = true, reason }` blocks an event that can be
/// blocked, and the replacement fields of the event are written into it.
pub(crate) fn apply(lua: &Lua, event: &mut Event, returned: &Table, script: &str) -> mlua::Result<Outcome> {
    if returned.get::<Option<bool>>("block")? == Some(true) && blockable(event.kind()) {
        let reason = returned
            .get::<Option<String>>("reason")?
            .unwrap_or_else(|| format!("blocked by the extension `{script}`"));
        return Ok(Outcome::Block { reason });
    }
    match event {
        Event::UserPrompt { text, .. } => {
            if let Some(replacement) = returned.get::<Option<String>>("text")? {
                *text = replacement;
            }
        }
        Event::BeforeAgentStart { system, .. } => {
            if let Some(text) = returned.get::<Option<String>>("system_append")? {
                system.push(SystemPart {
                    text,
                    cache_boundary: false,
                });
            }
        }
        Event::ToolCall {
            input, skip_prompt, ..
        } => {
            let replacement: mlua::Value = returned.get("input")?;
            if !replacement.is_nil() {
                *input = from_lua(lua, replacement)?;
            }
            if let Some(skip) = returned.get::<Option<bool>>("skip_prompt")? {
                *skip_prompt = skip;
            }
        }
        Event::ToolResult { output, .. } => {
            if let Some(content) = returned.get::<Option<String>>("content")? {
                output.content = vec![Block::text(content)];
            }
            if let Some(is_error) = returned.get::<Option<bool>>("is_error")? {
                output.is_error = is_error;
            }
        }
        Event::Context { messages, .. } => {
            let replacement: mlua::Value = returned.get("messages")?;
            if !replacement.is_nil() {
                let replacement: Vec<Message> =
                    serde_json::from_value(from_lua(lua, replacement)?).map_err(|e| {
                        mlua::Error::runtime(format!("`messages` is not a list of messages: {e}"))
                    })?;
                *messages = replacement;
            }
        }
        Event::AgentSpawn { prompt, .. } => {
            if let Some(replacement) = returned.get::<Option<String>>("prompt")? {
                *prompt = replacement;
            }
        }
        _ => {}
    }
    Ok(Outcome::Continue)
}
