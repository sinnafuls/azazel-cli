-- Type declarations for the `azazel` global that Azazel gives every Luau extension.
-- `azazel ext types` writes this file to ~/.azazel/types/azazel.d.luau; point your editor's Luau
-- language server at it as a definitions file.
--
-- A script is ~/.azazel/extensions/<id>.luau or ~/.azazel/extensions/<id>/init.luau (the same two
-- forms under <project>/.azazel/extensions/ in trusted projects). Its top level runs once when it
-- loads. Functions marked "load only" work only there; functions marked "waits" work only
-- afterwards, inside a tool, handler, command or shortcut.

type Access = "read" | "write" | "exec" | "net"
type Level = "info" | "warn" | "error"

-- Questions to the user. Every function waits, and errors when nobody can answer.
type Ui = {
	confirm: (prompt: string) -> boolean,
	-- Returns the chosen option and its position in `options`.
	select: (prompt: string, options: { string }) -> (string, number),
	input: (prompt: string) -> string,
}

-- The second argument of every tool, handler, command and shortcut. Read-only.
type Ctx = {
	cwd: string,
	session_id: string,
	-- The agent the call is about; "lead" when it is about none.
	agent_id: string,
	-- The permission mode: "plan", "ask", "accept-edits" or "auto".
	mode: string?,
	ui: Ui,
}

type AgentInfo = {
	id: string,
	-- Name of the agent's definition; "lead" for the main agent.
	def: string,
	parent: string?,
	depth: number,
	session_id: string,
}

type ToolSpec = {
	name: string,
	description: string,
	-- A JSON Schema object describing `args`. Omitted means no arguments.
	parameters: { [string]: any }?,
	-- What the tool touches, which decides when the user is asked first. Default "exec".
	access: Access?,
	-- An error raised here becomes an error result the model sees.
	execute: (args: { [string]: any }, ctx: Ctx) -> string | { content: string, is_error: boolean? },
}

type CommandSpec = {
	-- Without the leading slash.
	name: string,
	description: string?,
	argument_hint: string?,
	-- `args` is the text after the command name. A returned string is shown to the user.
	run: (args: string, ctx: Ctx) -> string?,
}

-- Returned by a handler to stop what the event announces. Honoured by `user_prompt` (the prompt
-- is rejected), `tool_call` (the call is denied and the model sees the reason), `agent_spawn`
-- (the spawn is refused), `pre_compact` (a manual compaction is cancelled) and `stop` (the agent
-- continues with the reason as a user message).
type Block = { block: boolean, reason: string? }

type SessionStartEvent = { name: "session_start", source: "startup" | "resume" | "clear" | "compact" }
type SessionEndEvent = { name: "session_end" }
type UserPromptEvent = { name: "user_prompt", text: string, images: number }
type BeforeAgentStartEvent = { name: "before_agent_start", agent: AgentInfo, system: { string } }
type TurnEvent = { name: "turn_start" | "turn_end", agent: AgentInfo }
type ToolCallEvent = {
	name: "tool_call",
	agent: AgentInfo,
	call_id: string,
	tool: string,
	input: { [string]: any },
	skip_prompt: boolean,
}
type ToolResultEvent = {
	name: "tool_result",
	agent: AgentInfo,
	call_id: string,
	tool: string,
	content: string,
	is_error: boolean,
}
type Message = { role: "user" | "assistant" | "tool", content: { { [string]: any } }, meta: { [string]: any }? }
type ContextEvent = { name: "context", agent: AgentInfo, messages: { Message } }
type AgentSpawnEvent = { name: "agent_spawn", parent: AgentInfo, def: string, prompt: string }
type AgentEndEvent = { name: "agent_end", agent: AgentInfo, status: string }
type PreCompactEvent = { name: "pre_compact", agent: AgentInfo, manual: boolean }
type PostCompactEvent = { name: "post_compact", agent: AgentInfo }
type StopEvent = { name: "stop", agent: AgentInfo, stop_hook_active: boolean }
type NotificationEvent = { name: "notification", level: Level, text: string }

-- A handler returns nil, a `Block`, or a table with the fields to replace. A handler that raises
-- an error, or whose own code runs for more than 5 seconds, disables its script.
type On =
	((event: "session_start", handler: (ev: SessionStartEvent, ctx: Ctx) -> ()) -> ())
	& ((event: "session_end", handler: (ev: SessionEndEvent, ctx: Ctx) -> ()) -> ())
	& ((event: "user_prompt", handler: (ev: UserPromptEvent, ctx: Ctx) -> (Block | { text: string? })?) -> ())
	& ((
		event: "before_agent_start",
		handler: (ev: BeforeAgentStartEvent, ctx: Ctx) -> { system_append: string? }?
	) -> ())
	& ((event: "turn_start" | "turn_end", handler: (ev: TurnEvent, ctx: Ctx) -> ()) -> ())
	& ((
		event: "tool_call",
		handler: (ev: ToolCallEvent, ctx: Ctx) -> (Block | { input: { [string]: any }?, skip_prompt: boolean? })?
	) -> ())
	& ((
		event: "tool_result",
		handler: (ev: ToolResultEvent, ctx: Ctx) -> { content: string?, is_error: boolean? }?
	) -> ())
	& ((event: "context", handler: (ev: ContextEvent, ctx: Ctx) -> { messages: { Message }? }?) -> ())
	& ((event: "agent_spawn", handler: (ev: AgentSpawnEvent, ctx: Ctx) -> (Block | { prompt: string? })?) -> ())
	& ((event: "agent_end", handler: (ev: AgentEndEvent, ctx: Ctx) -> ()) -> ())
	& ((event: "pre_compact", handler: (ev: PreCompactEvent, ctx: Ctx) -> Block?) -> ())
	& ((event: "post_compact", handler: (ev: PostCompactEvent, ctx: Ctx) -> ()) -> ())
	& ((event: "stop", handler: (ev: StopEvent, ctx: Ctx) -> Block?) -> ())
	& ((event: "notification", handler: (ev: NotificationEvent, ctx: Ctx) -> ()) -> ())

type ExecResult = {
	-- nil when the program was ended by a signal.
	code: number?,
	stdout: string,
	stderr: string,
}

type HttpRequest = {
	url: string,
	-- Default "GET".
	method: string?,
	headers: { [string]: string }?,
	body: string?,
	-- Seconds. Default 30.
	timeout: number?,
}

type HttpResponse = { status: number, headers: { [string]: string }, body: string }

type DirEntry = { name: string, is_dir: boolean }

type Member = {
	id: string,
	def: string,
	parent: string?,
	depth: number,
	-- "queued", "running", "waiting for approval", "idle", "done", "failed" or "stopped".
	state: string,
	elapsed_secs: number,
	tokens: number,
	cost_usd: number?,
}

type MemoryHit = { id: string, kind: string, text: string, tags: { string }, scope: "project" | "global" }

type ProviderSpec = {
	id: string,
	api: "anthropic-messages" | "openai-responses" | "openai-chat" | "google-generative",
	base_url: string,
	-- Name of the environment variable holding the key. Omitted means the endpoint needs none.
	api_key_env: string?,
	headers: { [string]: string }?,
}

-- The fields of an entry in mcp.json, plus the server's name.
type McpSpec = {
	name: string,
	type: ("stdio" | "http" | "sse")?,
	command: string?,
	args: { string }?,
	env: { [string]: string }?,
	cwd: string?,
	url: string?,
	headers: { [string]: string }?,
	timeout: number?,
	enabled: boolean?,
	oauth: { clientId: string?, scope: string?, callbackPort: number? }?,
}

declare azazel: {
	-- Load only. Adds a tool the agents can call.
	tool: (spec: ToolSpec) -> (),
	-- Load only. Runs `handler` whenever `event` happens.
	on: On,
	-- Load only. Adds the slash command `/<name>`.
	command: (spec: CommandSpec) -> (),
	-- Load only. Binds a key combination such as "ctrl+k" in the interactive UI.
	shortcut: (key: string, handler: (ctx: Ctx) -> ()) -> (),
	-- Load only. Makes the models of an HTTP endpoint available as `<id>/<model>`.
	provider: (spec: ProviderSpec) -> (),
	-- Load only. Adds an MCP server; errors with "MCP unavailable" when MCP is off.
	mcp: (spec: McpSpec) -> (),

	-- Sets a segment of the status line; nil removes it.
	status: (key: string, text: string?) -> (),
	-- Sets lines shown above the input; nil removes them.
	widget: (key: string, lines: { string }?) -> (),
	-- Shows a message to the user. Default level "info".
	notify: (text: string, level: Level?) -> (),
	-- Sends `text` to the lead agent as the user: after its current step ("steer") or when it
	-- would otherwise go idle ("follow_up", the default).
	send: (text: string, options: { deliver: ("steer" | "follow_up")? }?) -> (),

	-- Waits. Runs a program directly, without a shell. `cwd` is relative to the project root;
	-- `timeout` is in seconds, default 60. Errors when the program cannot be started or runs out
	-- of time.
	exec: (program: string, args: { string }?, options: { cwd: string?, timeout: number? }?) -> ExecResult,
	-- Waits. Errors when no response arrives; an HTTP error status is a response.
	http: (request: HttpRequest) -> HttpResponse,

	-- Files in the project, the configured additional directories and `data_dir`. Relative paths
	-- start at the project root. A path that leads anywhere else, also through `..` or a symlink,
	-- is an error.
	fs: {
		-- This extension's own directory, ~/.azazel/extensions/.data/<id>.
		data_dir: string,
		-- Waits.
		read: (path: string) -> string,
		-- Waits. Creates the directories above the file.
		write: (path: string, content: string) -> (),
		-- Waits. Sorted by name.
		list: (path: string) -> { DirEntry },
		-- Waits.
		exists: (path: string) -> boolean,
	},

	json: {
		-- A table with a sequence part is an array, any other table an object.
		encode: (value: any, pretty: boolean?) -> string,
		-- `null` decodes to nil.
		decode: (text: string) -> any,
	},

	-- This extension's persistent values, kept in ~/.azazel/extensions/.state/<id>.json.
	state: {
		get: (key: string) -> any,
		-- `value` must be expressible as JSON; nil removes the key.
		set: (key: string, value: any) -> (),
	},

	agent: {
		-- Waits. Starts a sub-agent of the lead. With `wait` (the default) returns its report and
		-- its id once it has finished; with `wait = false` returns its id at once, and the lead is
		-- told when it finishes.
		spawn: (spec: { agent: string, prompt: string, wait: boolean? }) -> (string, string?),
		-- Messages an agent (an id, or "all") as the user. Returns the ids that received it.
		send: (to: string, message: string) -> { string },
		-- Stops an agent (an id, or "all") and the agents it started. Returns the ids stopped.
		stop: (id: string) -> { string },
		list: () -> { Member },
	},

	-- Project memory. Both functions wait, and error with "memory unavailable" when memory is off.
	memory: {
		-- `kind` is "fact" (the default), "decision", "preference", "convention" or "gotcha".
		-- Returns the id of the memory.
		save: (text: string, kind: string?, tags: { string }?) -> string,
		-- Best matches first. Default limit 8.
		search: (query: string, limit: number?) -> { MemoryHit },
	},
}
