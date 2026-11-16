/* Generated from docs/rpc.schema.json by `bun run gen:types`. Do not edit. */

/**
 * One line from the client: a command and the id its response repeats.
 */
export type Request = {
	/**
	 * Any JSON value of the client's choosing; `null` when left out.
	 */
	id?: {
		[k: string]: unknown;
	};
} & Request1;
export type Request1 =
	| {
			type: 'prompt';
			text: string;
			images?: Image[];
			/**
			 * A name of the client's choosing for this prompt, to send it again safely.
			 */
			request_id?: string | null;
	  }
	| {
			type: 'continue_run';
	  }
	| {
			type: 'steer';
			text: string;
			images?: Image[];
	  }
	| {
			type: 'follow_up';
			text: string;
	  }
	| {
			type: 'abort';
	  }
	| {
			type: 'get_state';
	  }
	| {
			type: 'set_model';
			chain: string[];
	  }
	| {
			type: 'set_mode';
			mode: string;
	  }
	| {
			type: 'set_agent';
			agent: string;
	  }
	| {
			type: 'agent_send';
			to: string;
			message: string;
	  }
	| {
			type: 'agent_stop';
			agent: string;
	  }
	| {
			type: 'agent_mention';
			name: string;
			text: string;
	  }
	| {
			type: 'approval_response';
			request_id: string;
			decision: Decision;
			feedback?: string | null;
	  }
	| {
			type: 'ui_response';
			request_id: string;
			value?: {
				[k: string]: unknown;
			};
	  }
	| {
			type: 'new_session';
			name?: string | null;
	  }
	| {
			type: 'resume_session';
			session_id?: string | null;
			path?: string | null;
	  }
	| {
			type: 'fork_session';
			entry_id?: string | null;
	  }
	| {
			type: 'list_sessions';
			query?: string | null;
	  }
	| {
			type: 'get_messages';
			leaf?: string | null;
			before?: string | null;
			limit?: number | null;
	  }
	| {
			type: 'get_agent_messages';
			agent: string;
			before?: string | null;
			limit?: number | null;
	  }
	| {
			type: 'get_tree';
	  }
	| {
			type: 'set_leaf';
			entry_id: string;
			before?: boolean;
	  }
	| {
			type: 'set_label';
			entry_id: string;
			label: string;
	  }
	| {
			type: 'get_session';
	  }
	| {
			type: 'set_name';
			name: string;
	  }
	| {
			type: 'set_thinking';
			level: string;
	  }
	| {
			type: 'set_tier';
			tier: string;
	  }
	| {
			type: 'compact';
			instructions?: string | null;
	  }
	| {
			type: 'retry';
	  }
	| {
			type: 'rewind';
			entry_id: string;
			/**
			 * What `rewind` takes back.
			 */
			what?: 'both' | 'code' | 'conversation';
	  }
	| {
			type: 'shell';
			command: string;
			share?: boolean;
	  }
	| {
			type: 'export';
			format?: ExportFormat | null;
			path?: string | null;
	  }
	| {
			type: 'btw';
			question: string;
	  }
	| {
			type: 'list_models';
			all?: boolean;
	  }
	| {
			type: 'list_providers';
	  }
	| {
			type: 'refresh_models';
			provider: string;
	  }
	| {
			type: 'list_roles';
	  }
	| {
			type: 'set_role';
			role: string;
			chain?: string[] | null;
			/**
			 * Where `set_role` keeps a change.
			 */
			save?: 'session' | 'home' | 'project';
	  }
	| {
			type: 'save_preset';
			name: string;
	  }
	| {
			type: 'apply_preset';
			name: string;
	  }
	| {
			type: 'list_commands';
	  }
	| {
			type: 'list_files';
	  }
	| {
			type: 'get_usage';
	  }
	| {
			type: 'run_command';
			name: string;
			args?: string;
	  }
	| {
			type: 'attach';
			pid?: number | null;
			/**
			 * What a client that attaches to a background session is sent of what happened before.
			 */
			replay?: 'missed' | 'all' | 'none';
			/**
			 * Resumes after a frame: the `at` of the last frame the client saw. The frames
			 * logged after it follow the response, and nothing before. When the log has no frame
			 * with that `at`, `gap` is set in the answer and `replay` applies instead.
			 */
			since?: number | null;
	  }
	| {
			type: 'stop';
			now?: boolean;
	  };
/**
 * The answer to an `approval_request`.
 */
export type Decision = 'allow' | 'session' | 'always' | 'deny';
/**
 * The file format of `export`.
 */
export type ExportFormat = 'markdown' | 'html' | 'jsonl';
/**
 * What `rewind` takes back.
 */
export type RewindWhat = 'both' | 'code' | 'conversation';
/**
 * Where `set_role` keeps a change.
 */
export type SaveTo = 'session' | 'home' | 'project';
/**
 * What a client that attaches to a background session is sent of what happened before.
 */
export type Replay = 'missed' | 'all' | 'none';
/**
 * One line from azazel. `type` names the frame; frames of an agent carry its id as `agent`
 * (`lead` for the main agent).
 */
export type Frame =
	| {
			type: 'hello';
			protocol: number;
			version: string;
	  }
	| Response
	| {
			type: 'agent_start';
			agent: string;
	  }
	| {
			type: 'turn_start';
			agent: string;
			turn: number;
	  }
	| {
			type: 'message_start';
			agent: string;
	  }
	| {
			type: 'stream';
			agent: string;
			event: StreamEvent;
	  }
	| {
			type: 'message_end';
			agent: string;
			message: unknown;
	  }
	| {
			type: 'tool_start';
			agent: string;
			call_id: string;
			name: string;
			input: unknown;
	  }
	| {
			type: 'tool_update';
			agent: string;
			call_id: string;
			text: string;
	  }
	| {
			type: 'tool_end';
			agent: string;
			call_id: string;
			output: ToolOutput;
			duration_ms: number;
	  }
	| {
			type: 'turn_end';
			agent: string;
	  }
	| {
			type: 'retry';
			agent: string;
			attempt: number;
			model: string;
			/**
			 * The model had sent nothing for this many seconds; absent after any other failure.
			 */
			stalled_secs?: number | null;
	  }
	| {
			type: 'fallback';
			agent: string;
			from: string;
			to: string;
	  }
	| {
			type: 'account';
			agent: string;
			provider: string;
			from: string;
			to: string;
			until: number;
			limit: string;
	  }
	| {
			type: 'tier_dropped';
			agent: string;
			model: string;
			tier: string;
	  }
	| {
			type: 'rule';
			agent: string;
			name: string;
	  }
	| {
			type: 'compaction';
			agent: string;
			tokens_before: number;
			tokens_after: number;
	  }
	| {
			type: 'idle';
			agent: string;
			stop: string;
	  }
	| {
			type: 'failed';
			agent: string;
			error: string;
	  }
	| {
			type: 'roster';
			members: Member[];
	  }
	| {
			type: 'approval_request';
			request_id: string;
			agent: string;
			tool: string;
			/**
			 * `read`, `write`, `exec` or `net`.
			 */
			access: string;
			/**
			 * What permission rules are matched against: paths, commands, `domain:<host>`.
			 */
			subjects: string[];
			input: unknown;
			/**
			 * The allow rule that `always` would add.
			 */
			rule?: string | null;
			/**
			 * A remark to show, such as a cost warning.
			 */
			note?: string | null;
			/**
			 * The call would destroy work: approval should not be the answer a stray key gives.
			 */
			destructive?: boolean;
			/**
			 * The call reads or writes a secret file (`.env`, a private key): `session` allows that
			 * file until the session ends, and `always` is not on offer.
			 */
			secret?: boolean;
	  }
	| ((
			| Question
			| {
					kind: 'form';
					questions: Question1[];
			  }
			| {
					kind: 'confirm';
					prompt: string;
			  }
			| {
					kind: 'select';
					prompt: string;
					options: string[];
			  }
			| {
					kind: 'input';
					prompt: string;
					secret?: boolean;
			  }
			| {
					kind: 'plan';
					path: string;
					plan: string;
			  }
			| {
					kind: 'notify';
					level: string;
					text: string;
			  }
	  ) & {
			type: 'ui_request';
			request_id: string;
	  })
	| {
			type: 'btw_delta';
			id: unknown;
			delta: string;
	  }
	| {
			type: 'user_input';
			kind: string;
			text: string;
			agent?: string | null;
	  }
	| {
			type: 'request_closed';
			request_id: string;
	  };
/**
 * A part of the message the model is writing. Deltas are for display; `message_end` carries
 * the complete message.
 */
export type StreamEvent =
	| {
			type: 'start';
	  }
	| {
			type: 'text_delta';
			index: number;
			delta: string;
	  }
	| {
			type: 'thinking_delta';
			index: number;
			delta: string;
	  }
	| {
			type: 'tool_call_start';
			index: number;
			id: string;
			name: string;
	  }
	| {
			type: 'tool_call_delta';
			index: number;
			delta: string;
	  }
	| {
			type: 'block_end';
			index: number;
			block: unknown;
	  };
/**
 * How an `ask` request is answered.
 */
export type AskKind = 'select' | 'multiselect' | 'text' | 'confirm';
/**
 * Where a role's chain comes from.
 */
export type RoleSource = ('home' | 'project' | 'project_local') | 'builtin' | 'session' | 'unset';
/**
 * The data of `run_command`.
 */
export type CommandResult =
	| {
			outcome: 'done';
	  }
	| {
			outcome: 'show';
			text: string;
	  }
	| {
			outcome: 'prompt';
			text: string;
	  };

/**
 * Protocol 1 of `azazel rpc`. Each line is one JSON object: a Request from the client, or a Frame from azazel.
 */
export interface Rpc {
	Request?: Request;
	Image?: Image;
	Decision?: Decision;
	RewindWhat?: RewindWhat;
	ExportFormat?: ExportFormat;
	SaveTo?: SaveTo;
	Replay?: Replay;
	Frame?: Frame;
	Response?: Response1;
	StreamEvent?: StreamEvent;
	ToolOutput?: ToolOutput;
	Member?: Member;
	AskKind?: AskKind;
	Question?: Question1;
	State?: State;
	Usage?: Usage1;
	PrimaryAgent?: PrimaryAgent;
	Todo?: Todo;
	Models?: Models;
	SessionRow?: SessionRow;
	MessageRow?: MessageRow;
	TreeRow?: TreeRow;
	ModelRow?: ModelRow;
	ModelCost?: ModelCost;
	ProviderRow?: ProviderRow;
	Roles?: Roles;
	RoleRow?: RoleRow;
	RoleSource?: RoleSource;
	AccountUsage?: AccountUsage;
	Window?: Window;
	UsageBlock?: UsageBlock;
	ShellResult?: ShellResult;
	Exported?: Exported;
	BtwAnswer?: BtwAnswer;
	CommandResult?: CommandResult;
	Attached?: Attached;
	Snapshot?: Snapshot1;
	Streaming?: Streaming;
	RunningTool?: RunningTool;
	Queued?: Queued;
	RunState?: RunState;
	Mentioned?: Mentioned;
	SessionCopy?: SessionCopy;
	CommandRow?: CommandRow;
}
/**
 * An image sent with a prompt.
 */
export interface Image {
	/**
	 * For example `image/png`.
	 */
	mime: string;
	/**
	 * The bytes, base64-encoded.
	 */
	data: string;
}
/**
 * The answer to one command. `data` is present when `success` is true, `error` when it is not.
 */
export interface Response {
	type: 'response';
}
/**
 * What a tool returned.
 */
export interface ToolOutput {
	/**
	 * `text` and `image` blocks.
	 */
	content: unknown[];
	is_error: boolean;
	/**
	 * Structured data for display (diffs, exit codes); the model never sees it.
	 */
	details?: {
		[k: string]: unknown;
	};
}
/**
 * One agent of the roster.
 */
export interface Member {
	id: string;
	/**
	 * The agent definition it runs as; `lead` for the main agent.
	 */
	def: string;
	parent?: string | null;
	depth: number;
	/**
	 * `queued`, `running`, `waiting for approval`, `idle`, `done`, `failed` or `stopped`.
	 */
	state: string;
	elapsed_ms: number;
	tokens: number;
	cost_usd?: number | null;
	/**
	 * What a running agent is doing, in a few words.
	 */
	activity?: string | null;
}
/**
 * A question from an agent. Expects what `expects` says: a list of the chosen options
 * (`select`, `multiselect`), a string (`text`), or `true` or `false` (`confirm`). With
 * `allow_freeform`, a string that is none of the options is an answer the user typed.
 */
export interface Question {
	kind: 'ask';
}
/**
 * A question from an agent: an `ask` request, or one of the questions of a `form`.
 */
export interface Question1 {
	question: string;
	/**
	 * A word or two naming the question among the others of a form.
	 */
	header?: string | null;
	/**
	 * What the user needs to know to answer, to be shown with the question.
	 */
	context?: string | null;
	/**
	 * Code the question is about, to be shown with it as it is written.
	 */
	code?: string | null;
	/**
	 * A unified diff the question is about, to be shown with it.
	 */
	diff?: string | null;
	/**
	 * Always sent; a client reads a request without it with [`AskKind::of`].
	 */
	expects?: AskKind | null;
	/**
	 * The choices of a `select` or `multiselect`; empty otherwise.
	 */
	options?: string[];
	/**
	 * Whether `expects` is `multiselect`.
	 */
	multi?: boolean;
	/**
	 * The answer the agent would give: one of `options`, or `yes` or `no` for a `confirm`.
	 */
	recommended?: string | null;
	allow_freeform?: boolean;
}
/**
 * The answer to one command. `data` is present when `success` is true, `error` when it is not.
 */
export interface Response1 {
	/**
	 * The `id` of the command; `null` for a line that was not a command.
	 */
	id?: {
		[k: string]: unknown;
	};
	success: boolean;
	data?: {
		[k: string]: unknown;
	};
	error?: string | null;
}
/**
 * The session as `get_state` and the session commands report it.
 */
export interface State {
	session_id: string;
	/**
	 * The session file.
	 */
	session_file?: string;
	/**
	 * The session's name, once it has one.
	 */
	name?: string | null;
	cwd: string;
	/**
	 * The permission mode.
	 */
	mode: string;
	/**
	 * Whether the lead is working.
	 */
	busy: boolean;
	/**
	 * The lead's request number within the current prompt; 0 before the first.
	 */
	turn: number;
	/**
	 * The model that answered most recently.
	 */
	model?: string | null;
	/**
	 * The virtual model (`virtual/<name>`) that picked `model` for that request, when the
	 * chain names one.
	 */
	via?: string | null;
	/**
	 * The virtual models extensions define, as `virtual/<name>`; `set_model` takes them.
	 */
	virtual_models?: string[];
	/**
	 * The lead's model chain as given, e.g. `["@default"]`.
	 */
	chain: string[];
	thinking: string;
	/**
	 * The service tier, when one is on.
	 */
	tier?: string | null;
	context_tokens: number;
	context_window: number;
	usage: Usage;
	/**
	 * How many model requests the lead has made.
	 */
	requests?: number;
	/**
	 * The roster.
	 */
	agents: Member[];
	/**
	 * The checked-out git branch of the project.
	 */
	branch?: string | null;
	/**
	 * Whether coordinator mode is on.
	 */
	coordinator?: boolean;
	/**
	 * The primary agent the lead is (`build`, `plan`, ...).
	 */
	agent?: string;
	/**
	 * The primary agents `set_agent` takes, in the order a client steps through them.
	 */
	primary_agents?: PrimaryAgent[];
	/**
	 * The open goal, in a few words.
	 */
	goal?: string | null;
	/**
	 * The lead's task list.
	 */
	todo?: Todo[];
	/**
	 * The model each role (`default`, `smol`, ...) resolves to first.
	 */
	roles?: {
		[k: string]: string;
	};
	/**
	 * Where the session's last run stopped, when the end of a process cut it short and
	 * `continue_run` can go on with it.
	 */
	interrupted?: string | null;
}
/**
 * Totals of the lead.
 */
export interface Usage {
	input?: number;
	output?: number;
	cache_read?: number;
	cache_write?: number;
	reasoning?: number;
	/**
	 * Absent when no price is known for the model.
	 */
	cost_usd?: number | null;
}
/**
 * A primary agent: a persona the lead can be.
 */
export interface PrimaryAgent {
	name: string;
	description: string;
	/**
	 * `#rrggbb`, when the definition gives the agent a colour.
	 */
	color?: string | null;
}
/**
 * One item of the lead's task list.
 */
export interface Todo {
	text: string;
	/**
	 * `pending`, `in_progress` or `done`.
	 */
	status: string;
}
/**
 * Token and cost totals.
 */
export interface Usage1 {
	input?: number;
	output?: number;
	cache_read?: number;
	cache_write?: number;
	reasoning?: number;
	/**
	 * Absent when no price is known for the model.
	 */
	cost_usd?: number | null;
}
/**
 * The data of `set_model`: the usable models the chain resolves to.
 */
export interface Models {
	models: string[];
}
/**
 * One saved session.
 */
export interface SessionRow {
	id: string;
	/**
	 * The session file.
	 */
	path: string;
	name?: string | null;
	/**
	 * When it was created and when its last entry was written, RFC 3339.
	 */
	created: string;
	updated: string;
	/**
	 * The first message you sent.
	 */
	first_prompt?: string | null;
	messages: number;
	prompts: number;
	pinned: boolean;
	usage: Usage1;
	/**
	 * With a `query`: the lines that matched.
	 */
	matches?: string[];
}
/**
 * One message of a branch.
 */
export interface MessageRow {
	entry_id: string;
	timestamp: string;
	/**
	 * `role` (`user`, `assistant`, `tool`), `content` blocks and, for replies, `meta`.
	 */
	message: {
		[k: string]: unknown;
	};
}
/**
 * One entry of the session file.
 */
export interface TreeRow {
	id: string;
	parent_id?: string | null;
	timestamp: string;
	/**
	 * `message`, `checkpoint`, `model_change`, `compaction`, `label`, ...
	 */
	kind: string;
	/**
	 * For a message: `user`, `assistant` or `tool`.
	 */
	role?: string | null;
	/**
	 * For a message: the start of its text. For a `label` or a `session_info`: the label or
	 * the name.
	 */
	text?: string | null;
	/**
	 * For a `label`: the entry it labels. For a `checkpoint`: the entry it was taken after.
	 */
	target_id?: string | null;
	/**
	 * Whether the entry is on the branch that ends at the current leaf, or, for an entry of a
	 * run's journal (`run_started`, ...), hangs from that branch.
	 */
	current: boolean;
}
/**
 * One model of `list_models`.
 */
export interface ModelRow {
	/**
	 * `provider/id`.
	 */
	model: string;
	provider: string;
	id: string;
	/**
	 * What the model is called in lists, when that is more than its id.
	 */
	name?: string | null;
	context_window: number;
	max_output: number;
	vision: boolean;
	reasoning: boolean;
	/**
	 * The thinking levels the model takes; empty for every level when `reasoning` is set.
	 */
	thinking_levels?: string[];
	/**
	 * USD per million tokens.
	 */
	cost?: ModelCost | null;
	/**
	 * Its provider has a credential, or needs none.
	 */
	usable: boolean;
	/**
	 * Its provider is used through a subscription sign-in: it is not paid by the token.
	 */
	login?: boolean;
}
export interface ModelCost {
	input: number;
	output: number;
}
/**
 * One provider of `list_providers`.
 */
export interface ProviderRow {
	id: string;
	name: string;
	/**
	 * It has a credential, or needs none.
	 */
	usable: boolean;
	/**
	 * The credential in use is a subscription sign-in.
	 */
	login: boolean;
	/**
	 * Where the credential comes from (`OPENAI_API_KEY`, `auth.json (login)`), when there is one.
	 */
	source?: string | null;
	/**
	 * The environment variables a key is read from.
	 */
	env?: string[];
	/**
	 * It can be signed in to with an account.
	 */
	oauth: boolean;
	/**
	 * How many models it has: as it listed them this session, else as the catalogue knows them.
	 */
	models: number;
}
/**
 * The data of `list_roles`, `set_role`, `save_preset` and `apply_preset`.
 */
export interface Roles {
	roles: RoleRow[];
	/**
	 * The names of the saved presets.
	 */
	presets: string[];
	/**
	 * Whether `set_role` may write the project's config (the project is trusted).
	 */
	project: boolean;
}
/**
 * One model role.
 */
export interface RoleRow {
	role: string;
	/**
	 * As configured, e.g. `["anthropic/claude-opus-5-5:high", "@slow"]`.
	 */
	chain: string[];
	source: RoleSource;
	/**
	 * The built-in value the role falls back to when nothing sets it.
	 */
	builtin?: string[];
	/**
	 * The usable models the chain resolves to, in order, as `provider/id[:level]`.
	 */
	resolved: string[];
	/**
	 * The thinking level of the first model, when the chain names one with the model.
	 */
	thinking?: string | null;
}
/**
 * The usage windows of one provider account.
 */
export interface AccountUsage {
	provider: string;
	account: string;
	windows: Window[];
	/**
	 * Unix seconds until which the account is out of allowance for every model.
	 */
	blocked_until?: number | null;
	/**
	 * Every block of the account, those for the models of one scope included.
	 */
	blocks?: UsageBlock[];
}
/**
 * One usage window of an account, as the provider last reported it.
 */
export interface Window {
	/**
	 * `5h`, `7d`, `7d-fable`, `overage`.
	 */
	name: string;
	/**
	 * The share that is used, 0.0 to 1.0.
	 */
	used: number;
	/**
	 * Unix seconds.
	 */
	resets_at?: number | null;
	/**
	 * `allowed`, `allowed_warning`, `rejected`, or whatever else the provider said.
	 */
	status: string;
	/**
	 * What the window counts when that is not every request: a model family such as `Fable`.
	 */
	scope?: string | null;
}
/**
 * An account that takes no requests until a time.
 */
export interface UsageBlock {
	/**
	 * Unix seconds until which the account is left alone.
	 */
	until: number;
	/**
	 * Unix seconds at which the provider last refused.
	 */
	since?: number;
	/**
	 * The window that ran out, by name, when the provider named one.
	 */
	window?: string | null;
	/**
	 * The models it holds for, as the scope of that window; absent for every model.
	 */
	scope?: string | null;
}
/**
 * The data of `shell`.
 */
export interface ShellResult {
	output: string;
	/**
	 * `exit code: 0`, `killed`, or why the command could not start.
	 */
	status: string;
}
/**
 * The data of `export`.
 */
export interface Exported {
	path: string;
}
/**
 * The data of `btw`.
 */
export interface BtwAnswer {
	answer: string;
	/**
	 * The model that answered.
	 */
	model: string;
	/**
	 * Tools the model tried to use anyway; those calls were dropped.
	 */
	dropped_tools?: string[];
}
/**
 * The data of `attach`.
 */
export interface Attached {
	/**
	 * The id of the background session, as `azazel ps` lists it.
	 */
	host: string;
	/**
	 * The process that runs it.
	 */
	pid: number;
	session_id: string;
	/**
	 * The session file.
	 */
	session_file: string;
	/**
	 * How many clients are attached, this one counted.
	 */
	clients?: number;
	snapshot?: Snapshot;
	/**
	 * The `at` the next frame will have: where live frames begin.
	 */
	cursor?: number;
	/**
	 * True when `since` named a frame the log does not hold; the client resynchronises with
	 * `get_messages`.
	 */
	gap?: boolean;
}
/**
 * Where the session stands at this moment.
 */
export interface Snapshot {
	/**
	 * Whether the lead is working.
	 */
	busy?: boolean;
	/**
	 * Why the lead's latest run failed, cleared when its next run starts.
	 */
	last_error?: string | null;
	/**
	 * The last messages of the conversation, oldest first.
	 */
	tail?: MessageRow[];
	/**
	 * The replies being written, one per agent that is writing one.
	 */
	streaming?: Streaming[];
	/**
	 * The tool calls that are running.
	 */
	tools?: RunningTool[];
	/**
	 * What clients sent the lead that it has not taken up yet, oldest first.
	 */
	queue?: Queued[];
	/**
	 * The ids of the requests that wait for an answer; the requests follow as frames.
	 */
	pending?: string[];
}
/**
 * A reply in the making.
 */
export interface Streaming {
	agent: string;
	/**
	 * The text so far.
	 */
	text: string;
	/**
	 * The thinking so far.
	 */
	thinking?: string;
}
/**
 * A tool call that has started and not ended.
 */
export interface RunningTool {
	agent: string;
	call_id: string;
	name: string;
	input: unknown;
	/**
	 * For how long it has run.
	 */
	elapsed_ms: number;
}
/**
 * Input the lead has not taken up yet. `kind` is `prompt`, `steer` or `follow_up`.
 */
export interface Queued {
	kind: string;
	text: string;
}
/**
 * A background session at the moment a client attaches: what a client needs to show the work
 * in progress before the frames that follow.
 */
export interface Snapshot1 {
	/**
	 * Whether the lead is working.
	 */
	busy?: boolean;
	/**
	 * Why the lead's latest run failed, cleared when its next run starts.
	 */
	last_error?: string | null;
	/**
	 * The last messages of the conversation, oldest first.
	 */
	tail?: MessageRow[];
	/**
	 * The replies being written, one per agent that is writing one.
	 */
	streaming?: Streaming[];
	/**
	 * The tool calls that are running.
	 */
	tools?: RunningTool[];
	/**
	 * What clients sent the lead that it has not taken up yet, oldest first.
	 */
	queue?: Queued[];
	/**
	 * The ids of the requests that wait for an answer; the requests follow as frames.
	 */
	pending?: string[];
}
/**
 * The data of a `prompt` that carries a `request_id`, and of `continue_run`: where a run
 * stands.
 */
export interface RunState {
	/**
	 * The `request_id` of the prompt that started the run, when it had one.
	 */
	request_id?: string | null;
	/**
	 * The run as the session file names it; absent until the lead has taken the prompt up.
	 */
	run_id?: string | null;
	/**
	 * `queued` (the lead has not taken it up yet), `running` or `ended`.
	 */
	state: string;
	/**
	 * Whether this answers a prompt that had been sent before: nothing was started.
	 */
	duplicate?: boolean;
	/**
	 * How an ended run ended: a stop reason of `idle`, or `failed`.
	 */
	stop?: string | null;
	/**
	 * The last thing the lead said in the run.
	 */
	text?: string | null;
}
/**
 * The data of `agent_mention`.
 */
export interface Mentioned {
	/**
	 * The agent the text went to.
	 */
	agent: string;
	/**
	 * Whether the agent was started for it.
	 */
	started: boolean;
}
/**
 * The data of `get_session`: the session file's first line and its entries, as they are
 * stored, and the entry the conversation continues from.
 */
export interface SessionCopy {
	/**
	 * The session file.
	 */
	path: string;
	header: unknown;
	entries: unknown[];
	leaf?: string | null;
}
/**
 * One command of `list_commands`.
 */
export interface CommandRow {
	name: string;
	description: string;
}
