// Types of the RPC protocol, from the generated schema, plus what the schema does not carry yet:
// the optional members of the remote protocol (`at`, `since`, `cursor`, `gap`, paged history).
import type { Frame, Request, Attached, Snapshot, MessageRow, ThemeColors } from './rpc';

export type {
	Frame,
	Request,
	State,
	Member,
	Question,
	ModelRow,
	AccountUsage,
	Window,
	Todo,
	Usage1 as Usage,
	PrimaryAgent,
	RoleRow,
	Roles,
	CommandRow,
	CommandResult,
	SaveTo,
	SettingRow,
	SettingList,
	SettingChanged,
	ContextReport,
	ContextCategory,
	CostReport,
	ModelSpend,
	SideSpend,
	TrustState,
	DirList,
	Forgotten,
	Image,
	Decision,
	MessageRow,
	Attached,
	Snapshot,
	Streaming,
	RunningTool,
	Queued,
	StreamEvent
} from './rpc';

/** What `get_theme` answers and the `theme` frame carries beside its `type`. */
export type TerminalTheme = ThemeColors;

/** One frame of a given `type`, with the log position the host added. */
export type FrameOf<T extends Frame['type']> = Extract<Frame, { type: T }> & { at?: number };
export type AnyFrame = Frame & { at?: number };

/** `Attached` as a remote host answers it: with a cursor and a gap flag. */
export type AttachedRemote = Attached & { cursor?: number; gap?: boolean };
export type SnapshotRemote = Snapshot;

/** Commands the page sends. `id` is added by the client. */
export type Command = Request extends infer R
	? R extends { type: string }
		? Omit<R, 'id'>
		: never
	: never;
export type CommandType = Command['type'];

/** Content blocks of a message, as `azazel-ai` writes them. */
export type Block =
	| { type: 'text'; text: string }
	| { type: 'thinking'; text: string; redacted?: boolean }
	| { type: 'image'; mime: string; data: string }
	| { type: 'tool_call'; id: string; name: string; input: unknown }
	| { type: 'tool_result'; call_id: string; content: Block[]; is_error?: boolean }
	| { type: 'opaque'; provider: string; item: unknown };

export interface WireMessage {
	role: 'user' | 'assistant' | 'tool' | 'system';
	content: Block[];
	meta?: {
		provider?: string;
		model?: string;
		usage?: { input: number; output: number; cache_read?: number; cache_write?: number };
		stop?: string;
		duration_ms?: number;
	};
}

export type Row = Omit<MessageRow, 'message'> & { message: WireMessage };

/** `GET /api/me`. */
export interface Me {
	access: 'view' | 'control';
	expires: string;
	allow_bypass: boolean;
	/** `[remote] full_access`: this device may do anything the terminal can. */
	full_access: boolean;
	/** Whether this device may start sessions (`POST /api/sessions`). */
	can_start?: boolean;
	protocol: number;
	version: string;
}

/** One folder of `POST /api/folders`. */
export interface Folder {
	path: string;
	/** The path as people read it, `~` for the home directory. */
	name: string;
	kind: 'project' | 'root' | 'dir';
	trusted: boolean;
	startable: boolean;
}

/** `POST /api/folders`: the folders to start in, or those inside `at`. */
export interface Folders {
	at: string | null;
	parent: string | null;
	folders: Folder[];
	/** Any folder of the machine may be typed in. */
	anywhere: boolean;
}

/** `POST /api/sessions`. */
export interface NewSession {
	cwd: string;
	prompt?: string;
	mode?: string;
	request_id?: string;
}

/** One entry of `GET /api/sessions`. */
export interface SessionInfo {
	id: string;
	name: string | null;
	kind: 'background' | 'terminal';
	state: string;
	cwd: string;
	cost_usd: number | null;
	mode: string | null;
	started: string;
}

export const MODES = ['plan', 'manual', 'accept-edits', 'auto', 'bypass'] as const;
export const THINKING_LEVELS = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const;
export const TIERS = ['priority', 'standard', 'economy'] as const;
