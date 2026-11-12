//! The `azazel` global of a script and the `ctx.ui` table.
//!
//! Functions that register something are plain functions and work only while the script loads.
//! Functions that wait (processes, HTTP, files, agents, memory, questions to the user) are async
//! and work only afterwards, inside a tool, handler, command or shortcut.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use azazel_agent::{Access, AgentHandle, UserInput};
use azazel_ai::providers::http::http_client;
use azazel_ai::{ProviderConfig, WireApi};
use azazel_core::process::AgentEnv;
use azazel_core::{Level, McpServers, Memory, Ui, UiBridge};
use azazel_team::{SpawnRequest, Team};
use mlua::{FromLua, Function, Lua, LuaString, Table};
use serde_json::{Value, json};

use crate::convert::{EVENTS, from_lua, to_lua};
use crate::script::{CommandDef, Shared, ToolDef, describe};

/// Seconds `azazel.exec` waits for a program unless told otherwise.
const EXEC_TIMEOUT: f64 = 60.0;
/// Seconds `azazel.http` waits for a response unless told otherwise.
const HTTP_TIMEOUT: f64 = 30.0;
/// Memories `azazel.memory.search` returns unless told otherwise.
const MEMORY_LIMIT: usize = 8;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(http_client);

fn runtime(message: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(message)
}

/// The field `key` of the table passed to `function`, which must be there and of type `T`.
fn required<T: FromLua>(table: &Table, function: &str, key: &str) -> mlua::Result<T> {
    match table.get::<Option<T>>(key) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Err(runtime(format!("{function}: `{key}` is required"))),
        Err(e) => Err(runtime(format!("{function}: `{key}`: {}", describe(&e)))),
    }
}

/// The field `key` of the table passed to `function` when it is there, which must be of type `T`.
fn optional<T: FromLua>(table: &Table, function: &str, key: &str) -> mlua::Result<Option<T>> {
    table
        .get::<Option<T>>(key)
        .map_err(|e| runtime(format!("{function}: `{key}`: {}", describe(&e))))
}

/// A timeout given in seconds, or `default` seconds.
fn seconds(given: Option<f64>, default: f64, function: &str) -> mlua::Result<Duration> {
    let seconds = given.unwrap_or(default);
    if seconds.is_finite() && seconds > 0.0 {
        Ok(Duration::from_secs_f64(seconds))
    } else {
        Err(runtime(format!(
            "{function}: `timeout` must be a positive number of seconds"
        )))
    }
}

/// `error` with the errors that caused it.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    text
}

fn team(shared: &Shared, function: &str) -> mlua::Result<(Arc<Team>, AgentHandle)> {
    let unavailable = || runtime(format!("{function}: no session with a team of agents is running"));
    let team = shared.services.get::<Team>().ok_or_else(unavailable)?;
    let lead = team.lead().ok_or_else(unavailable)?;
    Ok((team, lead))
}

fn ui(shared: &Shared, function: &str) -> mlua::Result<Arc<dyn UiBridge>> {
    shared
        .services
        .get::<Ui>()
        .map(|ui| ui.0.clone())
        .ok_or_else(|| runtime(format!("{function}: no interactive user")))
}

/// Puts the `azazel` global into `lua`, to be called before the VM is sandboxed, and returns the
/// `ctx.ui` table.
pub(crate) fn install(lua: &Lua, shared: &Arc<Shared>) -> mlua::Result<Table> {
    let azazel = lua.create_table()?;
    registering(lua, shared, &azazel)?;
    interface(lua, shared, &azazel)?;
    azazel.set("exec", exec(lua, shared)?)?;
    azazel.set("http", http(lua, shared)?)?;
    azazel.set("fs", fs(lua, shared)?)?;
    azazel.set("json", json_table(lua)?)?;
    azazel.set("state", state(lua, shared)?)?;
    azazel.set("agent", agent(lua, shared)?)?;
    azazel.set("memory", memory(lua, shared)?)?;
    azazel.set_readonly(true);
    lua.globals().set("azazel", azazel)?;
    ui_table(lua, shared)
}

/// `tool`, `on`, `command`, `shortcut`, `provider` and `mcp`.
fn registering(lua: &Lua, shared: &Arc<Shared>, azazel: &Table) -> mlua::Result<()> {
    let script = shared.clone();
    let tool = lua.create_function(move |lua, spec: Table| {
        const NAME: &str = "azazel.tool";
        script.registering(NAME)?;
        let name: String = required(&spec, NAME, "name")?;
        let valid = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(runtime(format!(
                "{NAME}: `{name}` is not a tool name; use up to 64 letters, digits, `_` and `-`"
            )));
        }
        let description: String = required(&spec, NAME, "description")?;
        let schema = match spec.get::<mlua::Value>("parameters")? {
            mlua::Value::Nil => json!({ "type": "object", "properties": {} }),
            parameters => from_lua(lua, parameters)
                .ok()
                .filter(Value::is_object)
                .ok_or_else(|| runtime(format!("{NAME}: `parameters` must be a JSON Schema table")))?,
        };
        let access = match optional::<String>(&spec, NAME, "access")?.as_deref() {
            None | Some("exec") => Access::Exec,
            Some("read") => Access::Read,
            Some("write") => Access::Write,
            Some("net") => Access::Net,
            Some(other) => {
                return Err(runtime(format!(
                    "{NAME}: `access` is `{other}`; use \"read\", \"write\", \"exec\" or \"net\""
                )));
            }
        };
        let execute: Function = required(&spec, NAME, "execute")?;
        script.pending().tools.push(ToolDef {
            name,
            description,
            schema,
            access,
            execute,
        });
        Ok(())
    })?;
    azazel.set("tool", tool)?;

    let script = shared.clone();
    let on = lua.create_function(move |_, (event, handler): (String, Function)| {
        script.registering("azazel.on")?;
        let Some((name, kind)) = EVENTS.iter().find(|(name, _)| *name == event) else {
            let known: Vec<&str> = EVENTS.iter().map(|(name, _)| *name).collect();
            return Err(runtime(format!(
                "azazel.on: there is no event `{event}`; the events are {}",
                known.join(", ")
            )));
        };
        script.pending().handlers.push((*name, *kind, handler));
        Ok(())
    })?;
    azazel.set("on", on)?;

    let script = shared.clone();
    let command = lua.create_function(move |_, spec: Table| {
        const NAME: &str = "azazel.command";
        script.registering(NAME)?;
        let name: String = required(&spec, NAME, "name")?;
        let name = name.trim_start_matches('/').to_string();
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(runtime(format!(
                "{NAME}: `name` must be one word, such as \"deploy\""
            )));
        }
        let command = CommandDef {
            name,
            description: optional(&spec, NAME, "description")?.unwrap_or_default(),
            argument_hint: optional(&spec, NAME, "argument_hint")?,
            run: required(&spec, NAME, "run")?,
        };
        script.pending().commands.push(command);
        Ok(())
    })?;
    azazel.set("command", command)?;

    let script = shared.clone();
    let shortcut = lua.create_function(move |_, (key, handler): (String, Function)| {
        script.registering("azazel.shortcut")?;
        if key.trim().is_empty() {
            return Err(runtime(
                "azazel.shortcut: the key is empty; use a combination such as \"ctrl+k\"",
            ));
        }
        script.pending().shortcuts.push((key, handler));
        Ok(())
    })?;
    azazel.set("shortcut", shortcut)?;

    let script = shared.clone();
    let provider = lua.create_function(move |_, spec: Table| {
        const NAME: &str = "azazel.provider";
        script.registering(NAME)?;
        let id: String = required(&spec, NAME, "id")?;
        if id.is_empty() || id.contains(['/', ':']) || id.contains(char::is_whitespace) {
            return Err(runtime(format!(
                "{NAME}: `{id}` is not a provider id; use one word without `/` or `:`"
            )));
        }
        let api: String = required(&spec, NAME, "api")?;
        let api = [
            WireApi::AnthropicMessages,
            WireApi::OpenAiResponses,
            WireApi::OpenAiChat,
            WireApi::GoogleGenerative,
        ]
        .into_iter()
        .find(|known| known.as_str() == api)
        .ok_or_else(|| {
            runtime(format!(
                "{NAME}: `api` is `{api}`; use \"anthropic-messages\", \"openai-responses\", \"openai-chat\" or \
                 \"google-generative\""
            ))
        })?;
        let base_url: String = required(&spec, NAME, "base_url")?;
        let api_key_env: Option<String> = optional(&spec, NAME, "api_key_env")?;
        let headers: Option<BTreeMap<String, String>> = optional(&spec, NAME, "headers")?;
        script.pending().providers.push(ProviderConfig {
            requires_key: api_key_env.is_some(),
            api_key_env: api_key_env.into_iter().collect(),
            headers: headers.unwrap_or_default(),
            ..ProviderConfig::new(id, api, base_url.trim_end_matches('/'))
        });
        Ok(())
    })?;
    azazel.set("provider", provider)?;

    let script = shared.clone();
    let mcp = lua.create_function(move |lua, spec: Table| {
        const NAME: &str = "azazel.mcp";
        script.registering(NAME)?;
        let servers = script
            .services
            .get::<McpServers>()
            .ok_or_else(|| runtime("MCP unavailable"))?;
        let name: String = required(&spec, NAME, "name")?;
        let mut entry = from_lua(lua, mlua::Value::Table(spec))
            .map_err(|e| runtime(format!("{NAME}: {}", describe(&e))))?;
        if let Some(fields) = entry.as_object_mut() {
            fields.remove("name");
        }
        servers
            .0
            .add_server(&name, entry)
            .map_err(|e| runtime(format!("{NAME}: {e}")))
    })?;
    azazel.set("mcp", mcp)
}

/// `status`, `widget`, `notify` and `send`.
fn interface(lua: &Lua, shared: &Arc<Shared>, azazel: &Table) -> mlua::Result<()> {
    let script = shared.clone();
    let status = lua.create_function(move |_, (key, text): (String, Option<String>)| {
        script.set_status(&key, text);
        Ok(())
    })?;
    azazel.set("status", status)?;

    let script = shared.clone();
    let widget = lua.create_function(move |_, (key, lines): (String, mlua::Value)| {
        let lines = match lines {
            mlua::Value::Nil => None,
            mlua::Value::String(text) => Some(text.to_string_lossy().lines().map(str::to_string).collect()),
            mlua::Value::Table(lines) => {
                Some(lines.sequence_values::<String>().collect::<mlua::Result<_>>()?)
            }
            other => {
                return Err(runtime(format!(
                    "azazel.widget: the lines are a {}; pass a list of strings, or nil to remove the widget",
                    other.type_name()
                )));
            }
        };
        script.set_widget(&key, lines);
        Ok(())
    })?;
    azazel.set("widget", widget)?;

    let script = shared.clone();
    let notify = lua.create_function(move |_, (text, level): (String, Option<String>)| {
        let level = match level.as_deref() {
            None | Some("info") => Level::Info,
            Some("warn") => Level::Warn,
            Some("error") => Level::Error,
            Some(other) => {
                return Err(runtime(format!(
                    "azazel.notify: the level is `{other}`; use \"info\", \"warn\" or \"error\""
                )));
            }
        };
        match script.services.get::<Ui>() {
            Some(ui) => ui.0.notify(level, &text),
            None => tracing::info!(script = %script.id, "{text}"),
        }
        Ok(())
    })?;
    azazel.set("notify", notify)?;

    let script = shared.clone();
    let send = lua.create_function(move |_, (text, options): (String, Option<Table>)| {
        const NAME: &str = "azazel.send";
        let deliver = match &options {
            Some(options) => optional::<String>(options, NAME, "deliver")?,
            None => None,
        };
        let (_, lead) = team(&script, NAME)?;
        let delivered = match deliver.as_deref() {
            None | Some("follow_up") => lead.follow_up(UserInput::text(text)),
            Some("steer") => lead.steer(UserInput::text(text)),
            Some(other) => {
                return Err(runtime(format!(
                    "{NAME}: `deliver` is `{other}`; use \"steer\" or \"follow_up\""
                )));
            }
        };
        if delivered {
            Ok(())
        } else {
            Err(runtime(format!("{NAME}: the lead agent has shut down")))
        }
    })?;
    azazel.set("send", send)
}

fn exec(lua: &Lua, shared: &Arc<Shared>) -> mlua::Result<Function> {
    type Args = (String, Option<Vec<String>>, Option<Table>);
    let script = shared.clone();
    lua.create_async_function(move |lua, (program, args, options): Args| {
        let script = script.clone();
        async move {
            const NAME: &str = "azazel.exec";
            script.running(NAME)?;
            let (cwd, timeout) = match &options {
                Some(options) => (
                    optional::<String>(options, NAME, "cwd")?,
                    optional::<f64>(options, NAME, "timeout")?,
                ),
                None => (None, None),
            };
            let timeout = seconds(timeout, EXEC_TIMEOUT, NAME)?;
            let mut command = tokio::process::Command::new(&program);
            command
                .args(args.unwrap_or_default())
                .current_dir(script.roots.project.join(cwd.unwrap_or_default()))
                .stdin(Stdio::null())
                .kill_on_drop(true);
            if let Some(env) = script.services.get::<AgentEnv>() {
                command.envs(env.vars());
            }
            let output = tokio::time::timeout(timeout, command.output())
                .await
                .map_err(|_| {
                    runtime(format!(
                        "{NAME}: `{program}` did not finish within {} s and was stopped",
                        timeout.as_secs_f64()
                    ))
                })?
                .map_err(|e| runtime(format!("{NAME}: cannot run `{program}`: {e}")))?;
            let result = lua.create_table()?;
            result.set("code", output.status.code())?;
            result.set("stdout", lua.create_string(&output.stdout)?)?;
            result.set("stderr", lua.create_string(&output.stderr)?)?;
            Ok(result)
        }
    })
}

fn http(lua: &Lua, shared: &Arc<Shared>) -> mlua::Result<Function> {
    let script = shared.clone();
    lua.create_async_function(move |lua, request: Table| {
        let script = script.clone();
        async move {
            const NAME: &str = "azazel.http";
            script.running(NAME)?;
            let url: String = required(&request, NAME, "url")?;
            let method = optional::<String>(&request, NAME, "method")?.unwrap_or_else(|| "GET".to_string());
            let method = reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
                .map_err(|_| runtime(format!("{NAME}: `{method}` is not an HTTP method")))?;
            let headers: BTreeMap<String, String> = optional(&request, NAME, "headers")?.unwrap_or_default();
            let body = optional::<LuaString>(&request, NAME, "body")?.map(|body| body.as_bytes().to_vec());
            let timeout = seconds(optional(&request, NAME, "timeout")?, HTTP_TIMEOUT, NAME)?;

            let mut call = HTTP.request(method, url.as_str()).timeout(timeout);
            for (name, value) in &headers {
                call = call.header(name.as_str(), value.as_str());
            }
            if let Some(body) = body {
                call = call.body(body);
            }
            let failed = |e: reqwest::Error| runtime(format!("{NAME}: {url}: {}", with_causes(&e)));
            let response = call.send().await.map_err(failed)?;
            let result = lua.create_table()?;
            result.set("status", response.status().as_u16())?;
            let headers = lua.create_table()?;
            for (name, value) in response.headers() {
                headers.set(name.as_str(), lua.create_string(value.as_bytes())?)?;
            }
            result.set("headers", headers)?;
            let body = response.bytes().await.map_err(failed)?;
            result.set("body", lua.create_string(&body)?)?;
            Ok(result)
        }
    })
}

/// `azazel.fs`: files inside the directories the script may use.
fn fs(lua: &Lua, shared: &Arc<Shared>) -> mlua::Result<Table> {
    let fs = lua.create_table()?;
    fs.set("data_dir", &*shared.data_dir.to_string_lossy())?;

    let script = shared.clone();
    let read = lua.create_async_function(move |lua, path: String| {
        let script = script.clone();
        async move {
            const NAME: &str = "azazel.fs.read";
            script.running(NAME)?;
            let file = script
                .roots
                .resolve(&path)
                .map_err(|e| runtime(format!("{NAME}: {e}")))?;
            let bytes = tokio::fs::read(&file)
                .await
                .map_err(|e| runtime(format!("{NAME}: {path}: {e}")))?;
            lua.create_string(&bytes)
        }
    })?;
    fs.set("read", read)?;

    let script = shared.clone();
    let write = lua.create_async_function(move |_, (path, content): (String, LuaString)| {
        let script = script.clone();
        let content = content.as_bytes().to_vec();
        async move {
            const NAME: &str = "azazel.fs.write";
            script.running(NAME)?;
            let file = script
                .roots
                .resolve(&path)
                .map_err(|e| runtime(format!("{NAME}: {e}")))?;
            let failed = |e: std::io::Error| runtime(format!("{NAME}: {path}: {e}"));
            if let Some(dir) = file.parent() {
                tokio::fs::create_dir_all(dir).await.map_err(failed)?;
            }
            tokio::fs::write(&file, content).await.map_err(failed)
        }
    })?;
    fs.set("write", write)?;

    let script = shared.clone();
    let list = lua.create_async_function(move |lua, path: String| {
        let script = script.clone();
        async move {
            const NAME: &str = "azazel.fs.list";
            script.running(NAME)?;
            let dir = script
                .roots
                .resolve(&path)
                .map_err(|e| runtime(format!("{NAME}: {e}")))?;
            let failed = |e: std::io::Error| runtime(format!("{NAME}: {path}: {e}"));
            let mut reader = tokio::fs::read_dir(&dir).await.map_err(failed)?;
            let mut entries: Vec<(String, bool)> = Vec::new();
            while let Some(entry) = reader.next_entry().await.map_err(failed)? {
                let is_dir = entry.file_type().await.is_ok_and(|kind| kind.is_dir());
                entries.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
            }
            entries.sort();
            let listing = lua.create_table()?;
            for (name, is_dir) in entries {
                let entry = lua.create_table()?;
                entry.set("name", name)?;
                entry.set("is_dir", is_dir)?;
                listing.push(entry)?;
            }
            Ok(listing)
        }
    })?;
    fs.set("list", list)?;

    let script = shared.clone();
    let exists = lua.create_async_function(move |_, path: String| {
        let script = script.clone();
        async move {
            const NAME: &str = "azazel.fs.exists";
            script.running(NAME)?;
            let file = script
                .roots
                .resolve(&path)
                .map_err(|e| runtime(format!("{NAME}: {e}")))?;
            Ok(tokio::fs::try_exists(&file).await.unwrap_or(false))
        }
    })?;
    fs.set("exists", exists)?;

    fs.set_readonly(true);
    Ok(fs)
}

/// `azazel.json`.
fn json_table(lua: &Lua) -> mlua::Result<Table> {
    let json = lua.create_table()?;
    let encode = lua.create_function(|lua, (value, pretty): (mlua::Value, Option<bool>)| {
        let value =
            from_lua(lua, value).map_err(|e| runtime(format!("azazel.json.encode: {}", describe(&e))))?;
        let text = if pretty == Some(true) {
            serde_json::to_string_pretty(&value)
        } else {
            serde_json::to_string(&value)
        };
        text.map_err(|e| runtime(format!("azazel.json.encode: {e}")))
    })?;
    json.set("encode", encode)?;
    let decode = lua.create_function(|lua, text: String| {
        let value: Value =
            serde_json::from_str(&text).map_err(|e| runtime(format!("azazel.json.decode: {e}")))?;
        to_lua(lua, &value)
    })?;
    json.set("decode", decode)?;
    json.set_readonly(true);
    Ok(json)
}

/// `azazel.state`: the script's persistent key-value store.
fn state(lua: &Lua, shared: &Arc<Shared>) -> mlua::Result<Table> {
    let state = lua.create_table()?;
    let script = shared.clone();
    let get = lua.create_function(move |lua, key: String| {
        let stored = script
            .state_get(&key)
            .map_err(|e| runtime(format!("azazel.state.get: {e}")))?;
        match stored {
            Some(value) => to_lua(lua, &value),
            None => Ok(mlua::Value::Nil),
        }
    })?;
    state.set("get", get)?;
    let script = shared.clone();
    let set = lua.create_function(move |lua, (key, value): (String, mlua::Value)| {
        const NAME: &str = "azazel.state.set";
        let value = match value {
            mlua::Value::Nil => None,
            value => Some(from_lua(lua, value).map_err(|e| runtime(format!("{NAME}: {}", describe(&e))))?),
        };
        script
            .state_set(&key, value)
            .map_err(|e| runtime(format!("{NAME}: {e}")))
    })?;
    state.set("set", set)?;
    state.set_readonly(true);
    Ok(state)
}

/// `azazel.agent`: the team's operations, acting for the lead.
fn agent(lua: &Lua, shared: &Arc<Shared>) -> mlua::Result<Table> {
    let agent = lua.create_table()?;

    let script = shared.clone();
    let spawn = lua.create_async_function(move |_, spec: Table| {
        let script = script.clone();
        // Cancelling the tool call this runs in stops the agent it waits for.
        let cancel = script.current_cancel();
        async move {
            const NAME: &str = "azazel.agent.spawn";
            script.running(NAME)?;
            let def: String = required(&spec, NAME, "agent")?;
            let prompt: String = required(&spec, NAME, "prompt")?;
            let wait = optional::<bool>(&spec, NAME, "wait")?.unwrap_or(true);
            let (team, lead) = team(&script, NAME)?;
            let request = SpawnRequest {
                def,
                prompt,
                name: None,
                model: None,
                isolation: None,
            };
            let id = team
                .spawn(lead.info(), request, !wait)
                .await
                .map_err(|e| runtime(format!("{NAME}: {e}")))?;
            if !wait {
                return Ok((id, None));
            }
            let finished = team
                .wait_all(std::slice::from_ref(&id), &cancel)
                .await
                .ok_or_else(|| runtime(format!("{NAME}: cancelled while waiting for `{id}`")))?;
            let report = finished
                .into_iter()
                .next()
                .map(|done| done.report)
                .unwrap_or_default();
            Ok((report, Some(id)))
        }
    })?;
    agent.set("spawn", spawn)?;

    let script = shared.clone();
    let send = lua.create_function(move |_, (to, message): (String, String)| {
        const NAME: &str = "azazel.agent.send";
        let (team, _) = team(&script, NAME)?;
        team.send_from_user(&to, &message)
            .map_err(|e| runtime(format!("{NAME}: {e}")))
    })?;
    agent.set("send", send)?;

    let script = shared.clone();
    let stop = lua.create_function(move |_, id: String| {
        const NAME: &str = "azazel.agent.stop";
        let (team, _) = team(&script, NAME)?;
        if id == "all" {
            Ok(team.stop_all())
        } else {
            team.stop(&id).map_err(|e| runtime(format!("{NAME}: {e}")))
        }
    })?;
    agent.set("stop", stop)?;

    let script = shared.clone();
    let list = lua.create_function(move |lua, ()| {
        let (team, _) = team(&script, "azazel.agent.list")?;
        let members: Vec<Value> = team
            .list()
            .into_iter()
            .map(|member| {
                json!({
                    "id": member.id,
                    "def": member.def,
                    "parent": member.parent,
                    "depth": member.depth,
                    "state": member.state.label(),
                    "elapsed_secs": member.elapsed.as_secs(),
                    "tokens": member.tokens,
                    "cost_usd": member.cost_usd,
                })
            })
            .collect();
        to_lua(lua, &Value::Array(members))
    })?;
    agent.set("list", list)?;

    agent.set_readonly(true);
    Ok(agent)
}

/// `azazel.memory`: project memory, when the session has it.
fn memory(lua: &Lua, shared: &Arc<Shared>) -> mlua::Result<Table> {
    let memory = lua.create_table()?;
    let unavailable = || runtime(azazel_core::memory::UNAVAILABLE);

    type SaveArgs = (String, Option<String>, Option<Vec<String>>);
    let script = shared.clone();
    let save = lua.create_async_function(move |_, (text, kind, tags): SaveArgs| {
        let script = script.clone();
        async move {
            const NAME: &str = "azazel.memory.save";
            script.running(NAME)?;
            let memory = script.services.get::<Memory>().ok_or_else(unavailable)?;
            let kind = kind.unwrap_or_else(|| "fact".to_string());
            let tags = tags.unwrap_or_default();
            memory
                .0
                .save(&text, &kind, &tags, false)
                .await
                .map_err(|e| runtime(format!("{NAME}: {e}")))
        }
    })?;
    memory.set("save", save)?;

    let script = shared.clone();
    let search = lua.create_async_function(move |lua, (query, limit): (String, Option<usize>)| {
        let script = script.clone();
        async move {
            const NAME: &str = "azazel.memory.search";
            script.running(NAME)?;
            let memory = script.services.get::<Memory>().ok_or_else(unavailable)?;
            let hits = memory
                .0
                .search(&query, limit.unwrap_or(MEMORY_LIMIT))
                .await
                .map_err(|e| runtime(format!("{NAME}: {e}")))?;
            let hits: Vec<Value> = hits
                .into_iter()
                .map(|hit| {
                    json!({
                        "id": hit.id,
                        "kind": hit.kind,
                        "text": hit.text,
                        "tags": hit.tags,
                        "scope": hit.scope,
                    })
                })
                .collect();
            to_lua(&lua, &Value::Array(hits))
        }
    })?;
    memory.set("search", search)?;

    memory.set_readonly(true);
    Ok(memory)
}

/// `ctx.ui`: questions to the user.
fn ui_table(lua: &Lua, shared: &Arc<Shared>) -> mlua::Result<Table> {
    let table = lua.create_table()?;

    let script = shared.clone();
    let confirm = lua.create_async_function(move |_, prompt: String| {
        let script = script.clone();
        async move {
            const NAME: &str = "ctx.ui.confirm";
            script.running(NAME)?;
            let bridge = ui(&script, NAME)?;
            bridge
                .confirm(&prompt)
                .await
                .map_err(|e| runtime(format!("{NAME}: {e}")))
        }
    })?;
    table.set("confirm", confirm)?;

    let script = shared.clone();
    let select = lua.create_async_function(move |_, (prompt, options): (String, Vec<String>)| {
        let script = script.clone();
        async move {
            const NAME: &str = "ctx.ui.select";
            script.running(NAME)?;
            if options.is_empty() {
                return Err(runtime(format!("{NAME}: there are no options to choose from")));
            }
            let bridge = ui(&script, NAME)?;
            let index = bridge
                .select(&prompt, &options)
                .await
                .map_err(|e| runtime(format!("{NAME}: {e}")))?;
            let chosen = options
                .get(index)
                .cloned()
                .ok_or_else(|| runtime(format!("{NAME}: the answer is not one of the options")))?;
            Ok((chosen, index + 1))
        }
    })?;
    table.set("select", select)?;

    let script = shared.clone();
    let input = lua.create_async_function(move |_, prompt: String| {
        let script = script.clone();
        async move {
            const NAME: &str = "ctx.ui.input";
            script.running(NAME)?;
            let bridge = ui(&script, NAME)?;
            bridge
                .input(&prompt)
                .await
                .map_err(|e| runtime(format!("{NAME}: {e}")))
        }
    })?;
    table.set("input", input)?;

    table.set_readonly(true);
    Ok(table)
}
