// A stand-in for `azazel remote` so the page can be built and tested without Rust.
//
//   bun run dev:mock            serves the API on :7420 (and web/build when it exists)
//   bun run dev                 then the Vite dev server proxies /api here
//
// It speaks the contract of docs/remote.md: /api/pair, /api/me, /api/sessions, /api/links,
// /api/logout and /api/ws/{id}, and replays frames recorded from the real `azazel rpc` with the
// scripted provider (fixtures/*.jsonl). A secret starting with `view` pairs a view-only device, one
// starting with `full` a device with full access (`[remote] full_access`), which shows the red strip.
// The 0.14 commands (get_theme, get_settings, set_setting, get_context, get_cost, run_command) answer
// with made-up but realistic data; `MOCK_THEME=light` makes the terminal's theme a light one.
// Profiles (0.16): the state carries `profile` and `profiles`, `set_profile` and `save_profile` are
// answered, and a prompt of `/limit` or `/reset` plays the switch a usage limit makes and the way back.
// The agents' browser (0.16): the sessions `showcase` and `team` have one open, and a prompt of
// `/browser` opens or closes it in any session. `browser_watch` sends a picture (fixtures/
// browser-frames.json), `browser_take` and `browser_input` change what it shows, and a view link is
// refused unless `MOCK_VIEW_BROWSER=1`.
import { existsSync, readFileSync } from 'node:fs';
import { extname, join, normalize } from 'node:path';

const root = join(import.meta.dir, '..');
const fx = (name: string) => join(root, 'fixtures', name);
const lines = (name: string): Frame[] =>
	readFileSync(fx(name), 'utf8')
		.split('\n')
		.filter(Boolean)
		.map((l) => JSON.parse(l));
const json = (name: string) => JSON.parse(readFileSync(fx(name), 'utf8'));

type Frame = Record<string, any>; // eslint-disable-line @typescript-eslint/no-explicit-any

const port = Number(process.env.PORT ?? process.argv[2] ?? 7420);
const speed = Number(process.env.MOCK_SPEED ?? 1);
const ids = { counter: 0 };

const VIEW_COMMANDS = new Set([
	'attach',
	'get_state',
	'get_messages',
	'get_agent_messages',
	'get_tree',
	'get_usage',
	'list_models',
	'list_roles',
	'list_commands',
	'get_theme',
	'get_context',
	'get_cost'
]);

// --- sessions ------------------------------------------------------------------------------

interface Scenario {
	id: string;
	name: string | null;
	state: string;
	cwd: string;
	cost: number | null;
	mode: string;
	started: string;
	frames: string;
	prompt: string;
	/** Frames sent at once when the session is first opened; the run goes on live after them. */
	prefill: (frames: Frame[]) => number;
	autostart: boolean;
}

const minutesAgo = (m: number) => new Date(Date.now() - m * 60_000).toISOString();
const firstIndex = (frames: Frame[], f: (x: Frame) => boolean) => frames.findIndex(f);

const scenarios: Scenario[] = [
	{
		id: 'approve',
		name: 'Add rate limiting to /login',
		state: 'waiting for you',
		cwd: '/home/dev/api-server',
		cost: 0.84,
		mode: 'manual',
		started: minutesAgo(18),
		frames: 'write_read.jsonl',
		prompt: 'Write hello.txt, then read it back to make sure it is there.',
		prefill: (f) => firstIndex(f, (x) => x.type === 'approval_request') + 1,
		autostart: true
	},
	{
		id: 'team',
		name: 'Fix the add() regression',
		state: 'working',
		cwd: '/home/dev/crate-add',
		cost: 1.92,
		mode: 'auto',
		started: minutesAgo(41),
		frames: 'team_goal.jsonl',
		prompt: 'Fix add(): it subtracts. Use three scouts first, then make the change.',
		prefill: (f) =>
			firstIndex(
				f,
				(x) =>
					x.type === 'roster' &&
					x.members.some((m: Frame) => m.id === 'scout-1' && m.state === 'running')
			) + 6,
		autostart: true
	},
	{
		id: 'showcase',
		name: 'Exponential retry backoff',
		state: 'idle',
		cwd: '/home/dev/net-client',
		cost: 0.31,
		mode: 'auto',
		started: minutesAgo(130),
		frames: 'showcase.jsonl',
		prompt: 'Make the retry backoff exponential, capped at 30 s.',
		prefill: (f) => f.length,
		autostart: true
	},
	{
		id: 'forms',
		name: 'Pick a session store',
		state: 'waiting for you',
		cwd: '/home/dev/sessions-svc',
		cost: 0.12,
		mode: 'manual',
		started: minutesAgo(7),
		frames: 'forms.jsonl',
		prompt: 'Set up persistence for the session service.',
		prefill: (f) => firstIndex(f, (x) => x.type === 'ui_request') + 1,
		autostart: true
	},
	{
		id: 'ended',
		name: null,
		state: 'interrupted',
		cwd: '/home/dev/docs-site',
		cost: null,
		mode: 'plan',
		started: minutesAgo(60 * 26),
		frames: 'mention.jsonl',
		prompt: 'Summarise the changelog.',
		prefill: (f) => f.length,
		autostart: true
	},
	// The states the recorded runs do not reach: a failed and a running tool call with a
	// destructive approval, a plan to review, and every level of toast.
	{
		id: 'states',
		name: 'Clean rebuild of net-client',
		state: 'waiting for you',
		cwd: '/home/dev/net-client',
		cost: 0.47,
		mode: 'manual',
		started: minutesAgo(4),
		frames: 'states.jsonl',
		prompt: 'Run the retry tests, then rebuild from a clean tree.',
		prefill: (f) => firstIndex(f, (x) => x.type === 'approval_request') + 1,
		autostart: true
	},
	{
		id: 'plan',
		name: 'Move sessions to SQLite',
		state: 'waiting for you',
		cwd: '/home/dev/sessions-svc',
		cost: 0.09,
		mode: 'plan',
		started: minutesAgo(2),
		frames: 'plan.jsonl',
		prompt: 'Plan how to keep sessions across restarts.',
		prefill: (f) => firstIndex(f, (x) => x.type === 'ui_request') + 1,
		autostart: true
	},
	{
		id: 'docs',
		name: 'Rewrite the getting-started guide',
		state: 'idle',
		cwd: '/home/dev/docs-site',
		cost: 0.58,
		mode: 'accept-edits',
		started: minutesAgo(60 * 49),
		frames: 'mention.jsonl',
		prompt: 'Rewrite the getting-started guide for 0.14.',
		prefill: (f) => f.length,
		autostart: true
	},
	{
		id: 'toasts',
		name: 'Nightly dependency bump',
		state: 'interrupted',
		cwd: '/home/dev/api-server',
		cost: 0.22,
		mode: 'auto',
		started: minutesAgo(300),
		frames: 'toasts.jsonl',
		prompt: 'Bump the dependencies and open a pull request.',
		prefill: (f) => f.length,
		autostart: true
	}
];

class Client {
	constructor(readonly ws: Bun.ServerWebSocket<WsData>) {}
	send(o: object) {
		this.ws.send(JSON.stringify(o));
	}
}
interface WsData {
	session: string;
	access: 'view' | 'control';
	full: boolean;
}

const OPUS = 'bedrock/anthropic.claude-opus-4-8';

// Declared before the sessions: each is built with the profiles in its state.
// The profiles of `State.profiles`: the built-in ones as azazel 0.16 has them, then those of the
// configuration. `gpt` lacks its login, so the page can show a profile that cannot be chosen.
const tiers = (def: string, slow: string, work: string, smol: string) => ({
	coder: work,
	default: def,
	reviewer: slow,
	scout: work,
	slow,
	smol,
	tester: work,
	vision: work
});
const A = {
	opus: 'anthropic/claude-opus-5-5',
	fable: 'anthropic/claude-fable-5-2',
	sonnet: 'anthropic/claude-sonnet-5-5',
	haiku: 'anthropic/claude-haiku-5-5'
};
const profile = (
	name: string,
	source: string,
	roles: Record<string, string>,
	missing: string[] = []
) => ({ name, source, usable: !missing.length, missing_logins: missing, roles });
const profiles = [
	profile('claude', 'builtin', tiers(A.opus, A.fable, A.sonnet, A.haiku)),
	profile(
		'gpt',
		'builtin',
		tiers(
			'openai-codex/gpt-6.1-sol',
			'openai-codex/gpt-6-astra',
			'openai-codex/gpt-6.1-terra',
			'openai-codex/gpt-6-luna'
		),
		['openai-codex']
	),
	profile('cheap', 'builtin', tiers(A.sonnet, A.sonnet, A.haiku, A.haiku)),
	profile('thorough', 'builtin', tiers(A.fable, A.fable, A.opus, A.sonnet)),
	profile('frontend', 'builtin', {
		...tiers(A.opus, A.fable, A.sonnet, A.haiku),
		designer: A.fable
	}),
	profile('research', 'builtin', {
		...tiers(A.opus, A.fable, A.sonnet, A.haiku),
		researcher: A.opus,
		scout: A.opus
	}),
	profile(
		'bedrock',
		'user',
		tiers(OPUS, OPUS, 'bedrock/anthropic.claude-sonnet-4-8', 'bedrock/anthropic.claude-haiku-4-8')
	)
];

/** Moves a session to a profile, as `set_profile` and a usage limit both do. */
function switchProfile(
	s: MockSession,
	to: string,
	reason: 'user' | 'limit' | 'reset',
	extra: Frame = {}
) {
	const p = profiles.find((x) => x.name === to);
	if (!p) return;
	const from = s.state.profile ?? null;
	s.state.profile = p.name;
	s.state.roles = { ...p.roles };
	s.state.model = p.roles.default;
	s.state.chain = [p.roles.default];
	s.emit({ type: 'profile_changed', from, to: p.name, reason, ...extra });
}

// --- the agents' browser -------------------------------------------------------------------

const shots = json('browser-frames.json') as Record<
	'page' | 'login' | 'tall',
	{ width: number; height: number; data: string }
>;
const viewBrowser = process.env.MOCK_VIEW_BROWSER === '1';

/**
 * A stand-in for the browser of a session: still pictures and the state the host reports. An
 * address with `login` in its path shows the form, one with `mobile` a page in portrait.
 */
class MockBrowser {
	visible = false;
	url = 'https://example.com/';
	title = 'Example Domain';
	shot: keyof typeof shots = 'page';
	holder: Client | null = null;
	watchers = new Set<Client>();
	seq = 0;
	private history: string[] = [];

	constructor(
		private readonly clients: Set<Client>,
		public running: boolean
	) {}

	state(c: Client) {
		const held = this.holder !== null;
		const base = {
			running: this.running,
			visible: this.visible,
			held,
			held_by_you: held && this.holder === c,
			watchers: this.watchers.size
		};
		return this.running ? { ...base, tab: 't1', url: this.url, title: this.title } : base;
	}
	/** Every attached client is told, each with its own `held_by_you`. */
	tell() {
		for (const c of this.clients) c.send({ type: 'browser_state', ...this.state(c) });
	}
	/** One picture, to those who watch. Never logged, never replayed. */
	frame(to: Iterable<Client> = this.watchers) {
		if (!this.running) return;
		this.seq++;
		for (const c of to)
			c.send({ type: 'browser_frame', tab: 't1', ...shots[this.shot], seq: this.seq });
	}
	private show(url: string) {
		const u = new URL(url);
		this.url = u.href;
		this.shot = /login|sign/i.test(u.pathname) ? 'login' : /mobile/i.test(u.href) ? 'tall' : 'page';
		this.title = this.shot === 'login' ? `Sign in · ${u.hostname}` : 'Example Domain';
	}
	toggle() {
		this.running = !this.running;
		if (!this.running) this.holder = null;
		this.tell();
		this.frame();
	}
	watch(c: Client, on: boolean) {
		if (on) this.watchers.add(c);
		else this.watchers.delete(c);
		if (on) this.frame([c]);
	}
	/** Why the client may not take the browser over, or nothing when it did. */
	take(c: Client, on: boolean): string | undefined {
		if (!this.running) return 'no browser is open in this session';
		if (on && this.holder && this.holder !== c) return 'another device has taken the browser over';
		if (!on && this.holder !== c) return undefined;
		this.holder = on ? c : null;
		this.tell();
	}
	input(c: Client, input: Frame): string | undefined {
		if (this.holder !== c) return 'take the browser over before you use it';
		const before = this.url;
		if (input.kind === 'navigate') {
			if (!/^https?:\/\//.test(input.url) && input.url !== 'about:blank')
				return `\`${input.url}\` is not an http or https URL`;
			this.history.push(this.url);
			this.show(input.url === 'about:blank' ? 'https://example.com/' : input.url);
		} else if (input.kind === 'back') {
			const last = this.history.pop();
			if (last) this.show(last);
		} else if (input.kind === 'click') {
			if (![input.x, input.y].every((n) => typeof n === 'number' && n >= 0 && n <= 1))
				return 'a click is at fractions of the frame, 0 to 1';
			// The "Sign in" link at the top right of the page leads to the form.
			if (this.shot === 'page' && input.x > 0.9 && input.y < 0.08) {
				this.history.push(this.url);
				this.show(new URL('/login', this.url).href);
			}
		}
		if (this.url !== before) this.tell();
		this.frame();
	}
	signIn() {
		this.running = true;
		this.visible = true;
		this.tell();
		this.frame();
	}
	/** A client went away: it watches nothing and holds nothing any more. */
	leave(c: Client) {
		this.watchers.delete(c);
		if (this.holder !== c) return;
		this.holder = null;
		this.tell();
	}
}

class MockSession {
	log: Frame[] = [];
	rows: Frame[] = [];
	clients = new Set<Client>();
	waiting = new Map<string, () => void>();
	pendingFrames = new Map<string, Frame>();
	/** Tool calls that have started and not ended, with when they started. */
	running = new Map<string, { frame: Frame; since: number }>();
	state: Frame;
	runId = 0;
	started = false;
	aborted = false;
	busy = false;
	browser: MockBrowser;

	constructor(readonly s: Scenario) {
		this.browser = new MockBrowser(this.clients, s.id === 'showcase' || s.id === 'team');
		const st = json('state.json');
		st.session_id = s.id;
		st.name = s.name;
		st.cwd = s.cwd;
		st.mode = s.mode;
		st.busy = false;
		st.model = 'bedrock/anthropic.claude-opus-4-8';
		st.chain = ['bedrock/anthropic.claude-opus-4-8'];
		st.thinking = 'medium';
		st.context_tokens = 38_400;
		st.context_window = 1_000_000;
		st.usage = {
			input: 51_200,
			output: 6_410,
			cache_read: 31_000,
			cache_write: 4_000,
			reasoning: 0,
			cost_usd: s.cost
		};
		st.requests = 9;
		st.branch = 'main';
		st.agents = [
			{
				id: 'lead',
				def: 'lead',
				parent: null,
				depth: 0,
				state: 'idle',
				elapsed_ms: 0,
				tokens: 0,
				cost_usd: null
			}
		];
		st.profile = 'claude';
		st.profiles = profiles;
		st.roles = { ...profiles[0].roles };
		st.todo = [
			{ text: 'Read the current retry policy', status: 'done' },
			{ text: 'Make backoff exponential with a 30 s cap', status: 'in_progress' },
			{ text: 'Run the retry tests', status: 'pending' }
		];
		this.state = st;
	}

	get at() {
		return this.log.length;
	}

	emit(frame: Frame, except?: Client) {
		const f = { ...frame, at: this.log.length + 1 };
		this.log.push(f);
		this.index(f);
		for (const c of this.clients) if (c !== except) c.send(f);
		return f;
	}

	/** Keeps what `attach` reports as the tail of the conversation. */
	private index(f: Frame) {
		const id = `e${this.rows.length + 1}`;
		const ts = new Date().toISOString();
		if (f.type === 'tool_start') this.running.set(f.call_id, { frame: f, since: Date.now() });
		if (f.type === 'tool_end') this.running.delete(f.call_id);
		if (f.type === 'user_input' && !f.agent) {
			this.rows.push({
				entry_id: id,
				timestamp: ts,
				message: { role: 'user', content: [{ type: 'text', text: f.text }] }
			});
		} else if (f.type === 'message_end' && f.agent === 'lead') {
			this.rows.push({ entry_id: id, timestamp: ts, message: f.message });
		} else if (f.type === 'tool_end' && f.agent === 'lead') {
			this.rows.push({
				entry_id: id,
				timestamp: ts,
				message: {
					role: 'tool',
					content: [
						{
							type: 'tool_result',
							call_id: f.call_id,
							content: f.output.content,
							is_error: f.output.is_error
						}
					]
				}
			});
		} else if (f.type === 'roster') {
			this.state.agents = f.members;
		} else if (f.type === 'agent_start' && f.agent === 'lead') {
			this.busy = true;
		} else if ((f.type === 'idle' || f.type === 'failed') && f.agent === 'lead') {
			this.busy = false;
		}
	}

	snapshot() {
		return {
			busy: this.busy,
			tail: this.rows.slice(-20),
			streaming: [],
			tools: [...this.running.values()].map(({ frame, since }) => ({
				agent: frame.agent,
				call_id: frame.call_id,
				name: frame.name,
				input: frame.input,
				elapsed_ms: Date.now() - since
			})),
			queue: [],
			pending: [...this.pendingFrames.keys()]
		};
	}

	/** `from` is the client that typed the prompt: it has shown it already and gets no echo. */
	async run(frames: Frame[], prefill: number, prompt: string, from?: Client) {
		const my = ++this.runId;
		this.aborted = false;
		this.emit({ type: 'user_input', kind: 'prompt', text: prompt }, from);
		for (let i = 0; i < frames.length; i++) {
			if (my !== this.runId || this.aborted) return;
			let f = frames[i];
			if (f.type === 'user_input' && !f.agent && i === 0) continue;
			const live = i >= prefill;
			if (f.type === 'stream' && live && ['text_delta', 'thinking_delta'].includes(f.event.type)) {
				const text: string = f.event.delta;
				for (let p = 0; p < text.length && my === this.runId; p += 14) {
					this.emit({ ...f, event: { ...f.event, delta: text.slice(p, p + 14) } });
					await sleep(28);
				}
				continue;
			}
			const blocking =
				f.type === 'approval_request' || (f.type === 'ui_request' && f.kind !== 'notify');
			f = this.emit(f);
			if (blocking) {
				this.pendingFrames.set(f.request_id, f);
				await new Promise<void>((resolve) => this.waiting.set(f.request_id, resolve));
				if (my !== this.runId) return;
			} else if (live) {
				await sleep(
					f.type === 'tool_end' ? 350 : f.type === 'roster' ? 500 : f.type === 'stream' ? 12 : 70
				);
			}
		}
	}

	answer(requestId: string, from: Client) {
		if (!this.pendingFrames.has(requestId)) return false;
		this.pendingFrames.delete(requestId);
		for (const c of this.clients)
			if (c !== from) c.send({ type: 'request_closed', request_id: requestId });
		this.waiting.get(requestId)?.();
		this.waiting.delete(requestId);
		return true;
	}

	start(prefillAll = false, prompt?: string) {
		const frames = lines(this.s.frames);
		const pre = prefillAll ? this.s.prefill(frames) : 0;
		void this.run(frames, pre, prompt ?? this.s.prompt);
	}
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms / speed));
const sessions = new Map(scenarios.map((s) => [s.id, new MockSession(s)]));
for (const s of sessions.values()) if (s.s.autostart) s.start(true);

// --- devices -------------------------------------------------------------------------------

interface Device {
	id: string;
	access: 'view' | 'control';
	full: boolean;
	name: string;
	paired: string;
	seen: string;
}
const devices = new Map<string, Device>();
const usedSecrets = new Set<string>();
const links = new Map<string, 'view' | 'control'>();
const VERSION = '0.14.0-mock';

function token(req: Request): string | null {
	return /azazel_device=([^;]+)/.exec(req.headers.get('cookie') ?? '')?.[1] ?? null;
}
function device(req: Request): Device | null {
	const d = devices.get(token(req) ?? '');
	if (d) d.seen = new Date().toISOString();
	return d ?? null;
}
function deviceName(ua: string): string {
	const os = /iPhone/.test(ua)
		? 'iPhone'
		: /Android/.test(ua)
			? 'Android'
			: /Mac OS/.test(ua)
				? 'Mac'
				: /Windows/.test(ua)
					? 'Windows'
					: 'Linux';
	const browser = /Edg\//.test(ua)
		? 'Edge'
		: /Chrome\//.test(ua)
			? 'Chrome'
			: /Firefox\//.test(ua)
				? 'Firefox'
				: /Safari\//.test(ua)
					? 'Safari'
					: 'Browser';
	return `${browser} on ${os}`;
}

const me = (d: Device) => ({
	access: d.access,
	expires: new Date(Date.now() + 24 * 3600_000).toISOString(),
	allow_bypass: process.env.MOCK_BYPASS !== '0',
	can_start: d.access === 'control',
	full_access: d.full,
	protocol: 1,
	version: VERSION
});
const j = (body: unknown, init: ResponseInit = {}) =>
	new Response(JSON.stringify(body), {
		...init,
		headers: {
			'content-type': 'application/json',
			'cache-control': 'no-store',
			...(init.headers ?? {})
		}
	});

// --- commands ------------------------------------------------------------------------------

const roleRow = (
	role: string,
	chain: string[],
	source: string,
	thinking: string | null = null
) => ({
	role,
	chain,
	source,
	builtin: [OPUS],
	resolved: chain.length ? chain : [OPUS],
	thinking
});
const roles = {
	roles: [
		roleRow('default', [OPUS, 'anthropic/claude-sonnet-4-5'], 'home', 'medium'),
		roleRow('smol', ['anthropic/claude-haiku-4-5'], 'home'),
		roleRow('slow', [OPUS], 'builtin', 'high'),
		roleRow('plan', [], 'unset'),
		roleRow('judge', ['openai/gpt-5.1'], 'project'),
		roleRow('vision', [], 'unset')
	],
	presets: ['fast', 'careful', 'cheap'],
	project: true
};

const THEME_DARK = {
	name: 'tokyo-night',
	light: false,
	roles: {
		text: '#c0caf5',
		muted: '#9aa5ce',
		accent: '#7aa2f7',
		success: '#9ece6a',
		warning: '#e0af68',
		error: '#f7768e',
		border: '#7a84b0',
		code_bg: '#16161e',
		diff_add: '#9ece6a',
		diff_del: '#f7768e',
		user_bg: '#292e42',
		user_text: '#c0caf5',
		selected_bg: '#283457',
		border_muted: '#3b4261',
		diff_add_bg: '#20303b',
		diff_del_bg: '#37222c',
		thinking: '#9aa5ce',
		bash: '#bb9af7',
		mode_plan: '#7dcfff',
		mode_edits: '#9ece6a',
		mode_auto: '#e0af68',
		surface: '#1a1b26',
		backdrop: '#16161e'
	}
};
const THEME_LIGHT = {
	name: 'paper',
	light: true,
	roles: {
		text: '#2e3440',
		muted: '#5c6370',
		accent: '#2f5fb3',
		success: '#2f7d32',
		// On purpose too pale for text on white: the page must fall back for these two.
		warning: '#e8c06a',
		error: '#ff9aa2',
		border: '#8a8f98',
		code_bg: '#f2efe8',
		diff_add: '#2f7d32',
		diff_del: '#b3261e',
		user_bg: '#ece8df',
		user_text: '#2e3440',
		selected_bg: '#e4dfd3',
		border_muted: '#cfc9bb',
		diff_add_bg: '#e3f1e0',
		diff_del_bg: '#f8e3e0',
		thinking: '#5c6370',
		bash: '#6b3fa0',
		mode_plan: '#2f5fb3',
		mode_edits: '#2f7d32',
		mode_auto: '#8a5a00',
		surface: '#faf8f3',
		backdrop: '#e9e5db'
	}
};
let theme = process.env.MOCK_THEME === 'light' ? THEME_LIGHT : THEME_DARK;

type Kind = { type: string; choices?: string[] };
const sw: Kind = { type: 'switch' };
const num: Kind = { type: 'number' };
const txt: Kind = { type: 'text' };
const lst: Kind = { type: 'list' };
const pick = (...choices: string[]): Kind => ({ type: 'choice', choices });
const GATED = new Set([
	'remote.full_access',
	'remote.allow_bypass',
	'hooks.enabled',
	'shell.program'
]);
const setting = (
	page: string,
	group: string,
	path: string,
	label: string,
	kind: Kind,
	def: unknown,
	description: string,
	live = true,
	value: unknown = def,
	source?: string
) => ({
	path,
	page,
	group,
	label,
	kind: kind.type,
	...(kind.choices ? { choices: kind.choices } : {}),
	live,
	description,
	value,
	default: def,
	...(source ? { source } : {}),
	gated: GATED.has(path)
});
const settings = [
	setting(
		'Appearance',
		'Theme',
		'tui.theme',
		'Theme',
		{ type: 'theme', choices: ['auto', 'tokyo-night', 'paper', 'gruvbox', 'mono'] },
		'auto',
		'The colours of the terminal UI. "Match terminal" in the browser follows it.',
		true,
		'tokyo-night',
		'home'
	),
	setting(
		'Appearance',
		'Theme',
		'tui.symbols',
		'Symbols',
		pick('unicode', 'ascii', 'nerd'),
		'unicode',
		'The set of symbols drawn for marks, spinners and borders.'
	),
	setting(
		'Appearance',
		'Layout',
		'tui.popups',
		'Popups',
		sw,
		true,
		'Pickers and menus open as popups instead of inline.'
	),
	setting(
		'Appearance',
		'Layout',
		'tui.status_line',
		'Status line',
		lst,
		['model', 'mode', 'context', 'cost'],
		'What the status line shows, in order.'
	),
	setting(
		'Appearance',
		'Layout',
		'tui.max_width',
		'Maximum width',
		num,
		120,
		'Columns the transcript may use; 0 uses the whole terminal.'
	),
	setting(
		'Appearance',
		'Transcript',
		'tui.show_thinking',
		'Show thinking',
		sw,
		true,
		"Show the model's reasoning as it arrives.",
		true,
		false,
		'session'
	),
	setting(
		'Appearance',
		'Transcript',
		'tui.tool_output_lines',
		'Tool output lines',
		num,
		12,
		"Lines of a tool's output shown before it is folded."
	),
	setting(
		'Models',
		'Roles',
		'roles.default',
		'Default',
		{ type: 'role' },
		[OPUS],
		'The chain of models the lead uses, first that answers.',
		true,
		[OPUS, 'anthropic/claude-sonnet-4-5'],
		'home'
	),
	setting(
		'Models',
		'Roles',
		'roles.smol',
		'Small',
		{ type: 'role' },
		[],
		'For names, summaries and other small requests.',
		true,
		['anthropic/claude-haiku-4-5'],
		'home'
	),
	setting(
		'Models',
		'Profiles',
		'profiles.on_limit',
		'Profile on a usage limit',
		txt,
		'',
		'The profile a session moves to when its provider reports a usage limit; empty never switches.',
		true,
		'cheap',
		'home'
	),
	setting(
		'Models',
		'Profiles',
		'profiles.on_reset',
		'When the limit resets',
		pick('ask', 'auto', 'off'),
		'ask',
		'ask: offer to go back to the first profile; auto: go back; off: stay on the second.'
	),
	setting(
		'Models',
		'Thinking',
		'model.thinking',
		'Thinking level',
		pick('off', 'low', 'medium', 'high', 'max'),
		'medium',
		'How hard the model thinks before it answers.'
	),
	setting(
		'Models',
		'Thinking',
		'model.fast',
		'Fast tier',
		sw,
		false,
		"Ask for the provider's faster, dearer tier when it has one."
	),
	setting(
		'Models',
		'Requests',
		'model.max_retries',
		'Retries',
		num,
		4,
		'Times a failed request is sent again before the next model of the chain is tried.'
	),
	setting(
		'Models',
		'Requests',
		'model.cache',
		'Prompt cache',
		pick('auto', 'short', 'long', 'off'),
		'auto',
		'How long the provider keeps the prompt prefix.',
		false
	),
	setting(
		'Session',
		'History',
		'session.auto_name',
		'Name sessions',
		sw,
		true,
		'Name a session after its first prompt.'
	),
	setting(
		'Session',
		'History',
		'session.branch_summary',
		'Summarise branches',
		sw,
		true,
		'When you leave a branch, add a summary of it to the conversation.'
	),
	setting(
		'Session',
		'Compaction',
		'compaction.auto',
		'Compact automatically',
		sw,
		true,
		'Summarise the conversation when the context is nearly full.'
	),
	setting(
		'Session',
		'Compaction',
		'compaction.threshold',
		'Compact at',
		num,
		0.85,
		'The share of the context window at which compaction starts.'
	),
	setting(
		'Session',
		'Compaction',
		'compaction.keep_recent',
		'Keep recent turns',
		num,
		6,
		'Turns kept word for word after a compaction.'
	),
	setting(
		'Session',
		'Checkpoints',
		'checkpoints.enabled',
		'Checkpoints',
		sw,
		true,
		'Save the files before each prompt, so that a rewind can restore them.',
		false
	),
	setting(
		'Permissions',
		'Mode',
		'permissions.mode',
		'Default mode',
		pick('plan', 'manual', 'accept-edits', 'auto'),
		'manual',
		'The permission mode a session starts in.'
	),
	setting(
		'Permissions',
		'Rules',
		'permissions.allow',
		'Allow',
		lst,
		[],
		'Tool calls that never ask.',
		true,
		['read(**)', 'shell(cargo test *)', 'shell(git status)', 'edit(src/**)'],
		'project'
	),
	setting(
		'Permissions',
		'Rules',
		'permissions.ask',
		'Ask',
		lst,
		[],
		'Tool calls that always ask, whatever the mode.',
		true,
		['shell(git push *)'],
		'home'
	),
	setting(
		'Permissions',
		'Rules',
		'permissions.deny',
		'Deny',
		lst,
		[],
		'Tool calls that are always refused.',
		true,
		['shell(rm -rf *)', 'read(.env*)', 'web_fetch(http://*)'],
		'home'
	),
	setting(
		'Tools',
		'Shell',
		'shell.program',
		'Shell',
		txt,
		'bash',
		'The program that runs shell commands.',
		false
	),
	setting(
		'Tools',
		'Shell',
		'shell.timeout_secs',
		'Timeout',
		num,
		120,
		'Seconds a command may run before it is stopped.'
	),
	setting(
		'Tools',
		'Web',
		'web.search',
		'Web search',
		sw,
		true,
		'Offer the web search tool to agents.'
	),
	setting(
		'Tools',
		'Web',
		'web.allow_hosts',
		'Allowed hosts',
		lst,
		[],
		'Hosts the web tools may reach; empty allows all.'
	),
	setting(
		'Tools',
		'Checks',
		'checks.on_write',
		'Check on write',
		sw,
		true,
		"Run the language's checker after a file is written."
	),
	setting(
		'Team',
		'Agents',
		'team.max_agents',
		'Agents at once',
		num,
		4,
		'Sub-agents that may run at the same time.',
		true,
		6,
		'project'
	),
	setting(
		'Team',
		'Agents',
		'team.worktree_target',
		'Worktree target',
		pick('shared', 'own'),
		'shared',
		"Whether an agent's worktree shares the build directory.",
		false
	),
	setting(
		'Team',
		'Goals',
		'team.checker',
		'Checker',
		sw,
		true,
		'A second model judges whether a goal was met.'
	),
	setting(
		'Memory',
		'Memory',
		'memory.enabled',
		'Remember',
		sw,
		true,
		'Keep facts, decisions and preferences for later sessions.'
	),
	setting(
		'Memory',
		'Memory',
		'memory.extract',
		'Extract after a session',
		sw,
		true,
		'Read a finished session for things worth remembering.'
	),
	setting(
		'Memory',
		'Index',
		'memory.index',
		'Code index',
		sw,
		false,
		'Index the project for semantic search.',
		false
	),
	setting(
		'Remote',
		'Access',
		'remote.allow_bypass',
		'Allow bypass mode',
		sw,
		false,
		'Let a paired device switch a session to bypass mode.',
		false,
		true,
		'home'
	),
	setting(
		'Remote',
		'Access',
		'remote.full_access',
		'Full access',
		sw,
		false,
		'Let a paired device do anything the terminal can.',
		false
	),
	setting(
		'Remote',
		'Access',
		'remote.link_minutes',
		'Link lifetime',
		num,
		15,
		'Minutes a pairing link works.'
	),
	setting(
		'Hooks',
		'Hooks',
		'hooks.enabled',
		'Run hooks',
		sw,
		true,
		'Run the programs the configuration hangs on events.',
		false
	),
	setting(
		'Notifications',
		'Notifications',
		'notify.desktop',
		'Desktop notifications',
		sw,
		true,
		'Notify when a session needs you or is done.'
	),
	setting(
		'Notifications',
		'Notifications',
		'notify.sound',
		'Sound',
		pick('off', 'chime', 'bell'),
		'off',
		'What plays with a notification.'
	)
];

const contextReport = (s: MockSession) => {
	// The real host answers null until the first request has gone out.
	if (!s.rows.length && !s.busy) return null;
	const cat = (kind: string, name: string, label: string, items: number, estimate: number) => ({
		kind,
		name,
		label,
		items,
		estimate,
		tokens: estimate
	});
	// The kinds and labels the real host gives (`crates/azazel-cli/src/context.rs`).
	const categories = [
		cat('system', '', 'system prompt: other', 3, 4_180),
		cat('system', 'context_files', 'AGENTS and context files', 2, 2_870),
		cat('system', 'skills', 'skill index', 21, 1_450),
		cat('system', 'memory', 'memory', 1, 620),
		cat('tools', '', 'tool schemas', 34, 11_940),
		cat('tools', 'github', 'tool schemas: MCP github', 9, 3_260),
		cat('messages', 'user', 'user messages', s.rows.length, 9_730),
		cat('tool_results', '', 'tool results', 14, 4_350)
	];
	const estimate = categories.reduce((n, c) => n + c.estimate, 0);
	return {
		agent: 'lead',
		request: 3,
		model: s.state.model,
		window: s.state.context_window,
		estimate,
		reported: s.state.context_tokens,
		tokens: s.state.context_tokens,
		categories
	};
};
const costReport = (s: MockSession) => {
	const total = s.s.cost ?? 0;
	return {
		requests: total ? 7 : 0,
		usage: {
			input: 51_200,
			output: 6_410,
			cache_read: 31_000,
			cache_write: 4_000,
			reasoning: 0,
			...(total ? { cost_usd: total } : {})
		},
		cache_hit_rate: Math.round((31_000 / (51_200 + 31_000)) * 100),
		models: total
			? [
					{
						model: OPUS,
						requests: 6,
						input: 44_900,
						cache_read: 29_400,
						cache_write: 4_000,
						hit_rate: 38
					},
					{
						model: 'anthropic/claude-haiku-4-5',
						requests: 1,
						input: 6_300,
						cache_read: 1_600,
						cache_write: 0,
						hit_rate: 20
					}
				]
			: [],
		side: []
	};
};
const usd = (n: number) => `$${n.toFixed(2)}`;
const k = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));
function costText(s: MockSession): string {
	const c = costReport(s);
	const u = c.usage;
	const rows = c.models.map(
		(m) =>
			`${m.model.padEnd(36)} ${String(m.requests).padStart(4)} ${k(m.input).padStart(7)} ${k(m.cache_read).padStart(7)} ${`${m.hit_rate}%`.padStart(5)}`
	);
	return [
		`Session cost   ${u.cost_usd === undefined ? 'unknown' : usd(u.cost_usd)}`,
		`Tokens         ${k(u.input)} in, ${k(u.output)} out`,
		`Cache          ${k(u.cache_read)} read, ${k(u.cache_write)} written (${c.cache_hit_rate}% hit rate)`,
		'',
		`${'Model'.padEnd(36)} ${'req'.padStart(4)} ${'in'.padStart(7)} ${'cached'.padStart(7)} ${'hit'.padStart(5)}`,
		...rows
	].join('\n');
}
/** What `run_command` answers, by command. */
function runCommand(s: MockSession, name: string, args: string): Frame | string {
	const commands: Frame[] = json('commands.json');
	if (!commands.some((c) => c.name === name)) return `There is no command /${name}.`;
	if (name.startsWith('skill:'))
		return { outcome: 'prompt', text: `Use the ${name.slice(6)} skill${args ? `: ${args}` : '.'}` };
	switch (name) {
		case 'cost':
			return { outcome: 'show', text: costText(s) };
		case 'context': {
			const r = contextReport(s);
			return {
				outcome: 'show',
				text: [
					`${r.model}: about ${k(r.estimate)} of ${k(r.window)} tokens`,
					'',
					...r.categories.map(
						(c) =>
							`${c.label.padEnd(28)} ${String(c.items).padStart(3)}  ${k(c.estimate).padStart(6)}`
					)
				].join('\n')
			};
		}
		case 'session':
			return {
				outcome: 'show',
				text: `File      ~/.azazel/sessions/${s.s.id}.jsonl\nMessages  ${s.rows.length}\nRequests  ${s.state.requests}\nAgents    ${s.state.agents.length}\nCost      ${usd(s.s.cost ?? 0)}`
			};
		case 'tools':
			return {
				outcome: 'show',
				text: 'Built in   read write edit glob grep shell job web_fetch web_search lsp todo task\nMCP        github (6)  linear (3)\nExtensions (none)'
			};
		case 'dirs':
			return { outcome: 'show', text: `${s.s.cwd}\n/home/dev/shared-protos (added)` };
		case 'rules':
			return {
				outcome: 'show',
				text: 'allow  read(**)\nallow  shell(cargo test *)\nask    shell(git push *)\ndeny   shell(rm -rf *)'
			};
		case 'doctor':
			return {
				outcome: 'show',
				text: 'ok    git 2.47\nok    ripgrep 14.1\nok    3 providers signed in\nwarn  ast-grep not found: ast_grep is off'
			};
		case 'review':
		case 'security-review':
		case 'verify':
			return {
				outcome: 'prompt',
				text: `Review the changes on this branch${args ? `: ${args}` : '.'}`
			};
		default:
			return { outcome: 'done' };
	}
}
const usage = [
	{
		provider: 'anthropic',
		account: 'work',
		windows: [
			{
				name: '5h',
				used: 0.42,
				resets_at: Math.floor(Date.now() / 1000) + 3 * 3600,
				status: 'allowed'
			},
			{
				name: '7d',
				used: 0.81,
				resets_at: Math.floor(Date.now() / 1000) + 2 * 86400,
				status: 'allowed_warning'
			}
		]
	}
];
const files = [
	'Cargo.toml',
	'src/lib.rs',
	'src/retry.rs',
	'src/net.rs',
	'tests/retry.rs',
	'README.md'
];

function respond(c: Client, id: unknown, data: unknown = null) {
	c.send({ type: 'response', id, success: true, data });
}
function fail(c: Client, id: unknown, error: string, code?: string) {
	c.send({ type: 'response', id, success: false, error, ...(code ? { code } : {}) });
}

function handle(s: MockSession, c: Client, access: 'view' | 'control', full: boolean, msg: Frame) {
	const { id, type } = msg;
	// What a device is refused for its access carries the code `refused`, as the real server's does.
	if (access === 'view' && !VIEW_COMMANDS.has(type) && !(type === 'browser_watch' && viewBrowser))
		return fail(c, id, `A view link cannot send \`${type}\`. Ask for a control link.`, 'refused');
	switch (type) {
		case 'browser_watch':
			s.browser.watch(c, msg.on === true);
			return respond(c, id, s.browser.state(c));
		case 'browser_take': {
			const why = s.browser.take(c, msg.on === true);
			return why ? fail(c, id, why) : respond(c, id, s.browser.state(c));
		}
		case 'browser_input': {
			const why = s.browser.input(c, msg.input ?? {});
			return why ? fail(c, id, why) : respond(c, id, s.browser.state(c));
		}
		case 'browser_sign_in':
			s.browser.signIn();
			return respond(c, id, s.browser.state(c));
		case 'get_theme':
			return respond(c, id, theme);
		case 'get_settings':
			return respond(c, id, {
				settings,
				pages: [...new Set(settings.map((r) => r.page))],
				project: true,
				home_config: '~/.azazel/config.toml',
				project_config: `${s.s.cwd}/.azazel/config.toml`
			});
		case 'set_setting': {
			const row = settings.find((r) => r.path === msg.path);
			if (!row) return fail(c, id, `There is no setting \`${msg.path}\`.`);
			if (!full && msg.scope !== 'session')
				return fail(
					c,
					id,
					'This device may only change settings for the session. Full access is off.'
				);
			if (!full && GATED.has(msg.path))
				return fail(c, id, `\`${msg.path}\` can only be changed with full access.`);
			if (row.kind === 'role')
				return fail(c, id, `\`${msg.path}\` is a model role: use \`set_role\`.`);
			if (msg.scope === 'session' && !row.live)
				return fail(c, id, `\`${msg.path}\` takes effect when azazel starts again.`);
			if (row.kind === 'number' && msg.value !== null && typeof msg.value !== 'number')
				return fail(c, id, `\`${msg.path}\` takes a number.`);
			const removed = msg.value === null;
			row.value = removed ? row.default : msg.value;
			if (removed) delete (row as { source?: string }).source;
			else (row as { source?: string }).source = msg.scope;
			if (msg.path === 'tui.theme') {
				theme = row.value === 'paper' ? THEME_LIGHT : THEME_DARK;
				for (const x of s.clients) x.send({ type: 'theme', ...theme });
			}
			if (msg.path === 'permissions.mode') s.state.mode = msg.value;
			return respond(c, id, {
				setting: row,
				message: removed ? `Removed ${msg.path}.` : `${msg.path} = ${JSON.stringify(msg.value)}`
			});
		}
		case 'get_context':
			return respond(c, id, contextReport(s));
		case 'get_cost':
			return respond(c, id, costReport(s));
		case 'run_command': {
			const r = runCommand(s, String(msg.name), String(msg.args ?? ''));
			if (typeof r === 'string') return fail(c, id, r);
			respond(c, id, r);
			if (r.outcome === 'prompt') {
				if (s.busy) s.emit({ type: 'user_input', kind: 'follow_up', text: r.text });
				else void s.run(lines(s.s.frames), 0, r.text).catch(() => {});
			}
			return;
		}
		case 'compact':
			respond(c, id, null);
			s.state.context_tokens = Math.round(s.state.context_tokens * 0.4);
			s.emit({ type: 'compaction', agent: 'lead', before: 38_400, after: s.state.context_tokens });
			return;
		case 'retry': {
			const last = [...s.rows].reverse().find((r) => r.message.role === 'user');
			respond(c, id, null);
			if (last && !s.busy)
				void s.run(lines(s.s.frames), 0, last.message.content[0].text).catch(() => {});
			return;
		}
		case 'set_name':
			s.state.name = msg.name;
			s.s.name = msg.name;
			return respond(c, id, null);
		case 'fork_session': {
			const at = s.rows.findIndex((r) => r.entry_id === msg.entry_id);
			if (at >= 0) s.rows = s.rows.slice(0, at + 1);
			s.state.name = `${s.s.name ?? 'Session'} (fork)`;
			s.s.name = s.state.name;
			return respond(c, id, { ...s.state, busy: s.busy });
		}
		case 'new_session':
			s.rows = [];
			s.state.name = null;
			s.s.name = null;
			return respond(c, id, { ...s.state, busy: false });
		case 'export': {
			const ext = msg.format === 'html' ? 'html' : msg.format === 'jsonl' ? 'jsonl' : 'md';
			if (msg.path && !full) return fail(c, id, 'A path can only be given with full access.');
			return respond(c, id, { path: msg.path ?? `${s.s.cwd}/.azazel/exports/${s.s.id}.${ext}` });
		}
		case 'btw':
			return void setTimeout(
				() =>
					respond(c, id, {
						answer: `Briefly: ${String(msg.question).replace(/\?$/, '')} is covered by \`src/retry.rs\`. The cap is 30 s and the factor is 2, so the fourth attempt already waits 8 s.`,
						model: 'anthropic/claude-haiku-4-5'
					}),
				700 / speed
			);
		case 'get_tree':
			return respond(
				c,
				id,
				s.rows.map((r, i) => ({
					id: r.entry_id,
					parent_id: i ? s.rows[i - 1].entry_id : null,
					timestamp: r.timestamp,
					kind: 'message',
					role: r.message.role,
					text:
						(r.message.content as Frame[])
							.filter((b) => b.type === 'text')
							.map((b) => b.text)
							.join(' ')
							.slice(0, 80) || null,
					current: true
				}))
			);
		case 'rewind':
		case 'set_leaf': {
			const at = s.rows.findIndex((r) => r.entry_id === msg.entry_id);
			if (at < 0) return fail(c, id, 'That entry is not in this session.');
			s.rows = s.rows.slice(0, at);
			return respond(c, id, type === 'rewind' ? null : { ...s.state, busy: s.busy });
		}
		case 'stop':
			respond(c, id, null);
			sessions.delete(s.s.id);
			scenarios.splice(scenarios.indexOf(s.s), 1);
			for (const x of s.clients) x.ws.close(1000, 'the session ended');
			return;
		case 'set_role': {
			if (!full && msg.save && msg.save !== 'session')
				return fail(
					c,
					id,
					'This device may only change roles for the session. Full access is off.'
				);
			let row = roles.roles.find((r) => r.role === msg.role);
			if (!row) roles.roles.push((row = roleRow(msg.role, [], 'unset')));
			row.chain = msg.chain ?? [];
			row.resolved = row.chain.length ? row.chain : [OPUS];
			row.source = msg.chain ? (msg.save ?? 'session') : 'unset';
			if (msg.role === 'default') {
				s.state.model = row.resolved[0];
				s.state.chain = row.resolved;
			}
			return respond(c, id, roles);
		}
		case 'save_preset':
			if (!full) return fail(c, id, 'Saving a preset needs full access.');
			if (!roles.presets.includes(msg.name)) roles.presets.push(msg.name);
			return respond(c, id, roles);
		case 'apply_preset':
			if (!roles.presets.includes(msg.name))
				return fail(c, id, `There is no preset \`${msg.name}\`.`);
			return respond(c, id, roles);
		case 'set_profile': {
			const p = profiles.find((x) => x.name === msg.name);
			if (!p) return fail(c, id, `There is no profile \`${msg.name}\`.`);
			if (!p.usable)
				return fail(
					c,
					id,
					`The profile \`${p.name}\` needs a login: run \`azazel login ${p.missing_logins[0]}\`.`
				);
			switchProfile(s, p.name, 'user');
			return respond(c, id, { ...s.state, busy: s.busy });
		}
		case 'save_profile': {
			if (!full) return fail(c, id, 'Saving a profile needs full access.');
			const name = String(msg.name ?? '').trim();
			if (!/^[A-Za-z0-9_-]+$/.test(name))
				return fail(c, id, 'A profile name is letters, digits, `-` and `_`.');
			const saved = profile(name, 'user', { ...(s.state.roles ?? {}) });
			const at = profiles.findIndex((x) => x.name === name);
			if (at < 0) profiles.push(saved);
			else if (profiles[at].source === 'builtin')
				return fail(c, id, `\`${name}\` is a built-in profile: choose another name.`);
			else profiles[at] = saved;
			return respond(c, id, { ...s.state, busy: s.busy });
		}
		case 'attach': {
			const since: number | undefined = msg.since;
			const known =
				since === undefined || since === 0 || (since <= s.log.length && s.log[since - 1]);
			const missed = since !== undefined && known ? s.log.slice(since) : [];
			respond(c, id, {
				host: s.s.id,
				pid: 4242,
				session_id: s.s.id,
				session_file: `${s.s.id}.jsonl`,
				clients: s.clients.size,
				snapshot: s.snapshot(),
				cursor: s.log.length,
				gap: since !== undefined && !known
			});
			for (const f of missed) c.send(f);
			for (const f of s.pendingFrames.values()) if (!missed.includes(f)) c.send(f);
			// The browser's state is not in the log: a client is told when it attaches.
			c.send({ type: 'browser_state', ...s.browser.state(c) });
			return;
		}
		case 'get_state':
			return respond(c, id, { ...s.state, busy: s.busy });
		case 'list_models':
			return respond(c, id, json('models.json'));
		case 'get_usage':
			return respond(c, id, usage);
		case 'list_roles':
			return respond(c, id, roles);
		case 'list_commands':
			return respond(
				c,
				id,
				(json('commands.json') as Frame[])
					// A device without full access is not offered what it may not run.
					.filter(
						(r) =>
							full ||
							![
								'login',
								'logout',
								'trust',
								'add-dir',
								'forget-approvals',
								'reload',
								'ext'
							].includes(r.name)
					)
					.map((r) => ({
						...r,
						source: r.name.startsWith('skill:')
							? 'skill'
							: ['review', 'security-review', 'playtest', 'verify'].includes(r.name)
								? 'extension'
								: 'builtin'
					}))
			);
		case 'list_files':
			return respond(c, id, files);
		case 'get_messages': {
			const before = s.rows.findIndex((r) => r.entry_id === msg.before);
			const end = before < 0 ? s.rows.length : before;
			return respond(c, id, s.rows.slice(Math.max(0, end - (msg.limit ?? 50)), end));
		}
		case 'get_agent_messages': {
			const text = `Task for ${msg.agent}`;
			return respond(c, id, [
				{
					entry_id: 'x1',
					timestamp: minutesAgo(3),
					message: { role: 'user', content: [{ type: 'text', text }] }
				},
				{
					entry_id: 'x2',
					timestamp: minutesAgo(2),
					message: {
						role: 'assistant',
						content: [
							{
								type: 'text',
								text: 'I read the file and found the cause: `add` subtracts its second argument.'
							}
						]
					}
				}
			]);
		}
		case 'prompt':
		case 'steer':
		case 'follow_up': {
			respond(c, id, null);
			// What a usage limit does to a session, and the way back, on demand.
			if (type === 'prompt' && msg.text === '/limit')
				return switchProfile(s, 'cheap', 'limit', {
					until: Math.floor(Date.now() / 1000) + 2 * 3600 + 14 * 60,
					limit: '5-hour limit'
				});
			if (type === 'prompt' && msg.text === '/reset') return switchProfile(s, 'claude', 'reset');
			if (type === 'prompt' && msg.text === '/browser') return s.browser.toggle();
			if (type === 'prompt' && !s.busy) {
				// `run` emits the user_input itself, to everyone but the sender.
				void s.run(lines(s.s.frames), 0, msg.text, c).catch(() => {});
			} else {
				s.emit(
					{ type: 'user_input', kind: type === 'prompt' ? 'prompt' : type, text: msg.text },
					c
				);
			}
			return;
		}
		case 'abort':
			s.aborted = true;
			s.runId++;
			for (const [rid, wake] of s.waiting) {
				s.pendingFrames.delete(rid);
				wake();
			}
			s.waiting.clear();
			respond(c, id, null);
			s.emit({ type: 'idle', stop: 'aborted', agent: 'lead' });
			return;
		case 'approval_response':
		case 'ui_response':
			if (!s.answer(msg.request_id, c)) return fail(c, id, 'That request was answered already.');
			return respond(c, id, null);
		case 'set_model':
			s.state.model = msg.chain[0];
			s.state.chain = msg.chain;
			return respond(c, id, { models: msg.chain });
		case 'set_mode':
			s.state.mode = msg.mode;
			return respond(c, id, { mode: msg.mode });
		case 'set_thinking':
			s.state.thinking = msg.level;
			return respond(c, id, null);
		case 'set_agent':
			s.state.agent = msg.agent;
			return respond(c, id, { agent: msg.agent });
		case 'agent_send':
			return respond(c, id, { agents: [msg.to] });
		case 'agent_stop':
			return respond(c, id, { agents: [msg.agent] });
		case 'agent_mention':
			return respond(c, id, { agent: msg.name, started: false });
		default:
			return respond(c, id, null);
	}
}

// --- static files --------------------------------------------------------------------------

const build = join(root, 'build');
const mime: Record<string, string> = {
	'.html': 'text/html; charset=utf-8',
	'.js': 'text/javascript',
	'.css': 'text/css',
	'.svg': 'image/svg+xml',
	'.json': 'application/json',
	'.webmanifest': 'application/manifest+json',
	'.png': 'image/png',
	'.woff2': 'font/woff2'
};
const CSP =
	"default-src 'self'; connect-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'";

function asset(path: string): Response {
	if (!existsSync(join(build, 'index.html'))) {
		return new Response(
			'The web UI was not built. Run `bun run build` in web/, or use `bun run dev`.',
			{ status: 404 }
		);
	}
	const clean = normalize(path).replace(/^([/\\])+/, '');
	let file = join(build, clean);
	if (!file.startsWith(build) || !existsSync(file) || clean === '')
		file = join(build, 'index.html');
	const type = mime[extname(file)] ?? 'application/octet-stream';
	const immutable = file.includes('_app') && file.includes('immutable');
	return new Response(Bun.file(file), {
		headers: {
			'content-type': type,
			'content-security-policy': CSP,
			'x-content-type-options': 'nosniff',
			'referrer-policy': 'no-referrer',
			'cache-control': immutable ? 'public, max-age=31536000, immutable' : 'no-cache'
		}
	});
}

// --- server --------------------------------------------------------------------------------

const server = Bun.serve<WsData>({
	port,
	hostname: '127.0.0.1',
	async fetch(req, srv) {
		const url = new URL(req.url);
		const p = url.pathname;
		if (!p.startsWith('/api/')) return asset(p);

		if (p === '/api/pair' && req.method === 'POST') {
			const { secret } = (await req.json().catch(() => ({}))) as { secret?: string };
			if (!secret || usedSecrets.has(secret))
				return j(
					{ error: 'That link was used already or has expired. Ask for a new one.' },
					{ status: 401 }
				);
			const access = links.get(secret) ?? (secret.startsWith('view') ? 'view' : 'control');
			if (!secret.endsWith('-demo')) usedSecrets.add(secret);
			const tok = `tok${++ids.counter}${Math.random().toString(36).slice(2)}`;
			const now = new Date().toISOString();
			const d: Device = {
				id: `d${ids.counter}`,
				access,
				full: secret.startsWith('full'),
				name: deviceName(req.headers.get('user-agent') ?? ''),
				paired: now,
				seen: now
			};
			devices.set(tok, d);
			return j(me(d), {
				headers: {
					'set-cookie': `azazel_device=${tok}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400`
				}
			});
		}
		const dev = device(req);
		const access = dev?.access;
		if (!dev || !access) {
			if (p.startsWith('/api/ws/') && req.headers.get('upgrade') === 'websocket') {
				// A close code needs an upgraded socket.
				return srv.upgrade(req, { data: { session: '!401', access: 'view', full: false } })
					? undefined
					: new Response('unauthorised', { status: 401 });
			}
			return j({ error: 'Not signed in. Open a pairing link.' }, { status: 401 });
		}
		if (p === '/api/me') return j(me(dev));
		// The paired devices, for a control device (docs/remote.md).
		if (p === '/api/devices' && dev.access !== 'control')
			return j({ error: 'this is a view link' }, { status: 403 });
		if (p === '/api/devices' && req.method === 'GET') {
			return j({
				devices: [...devices.values()].map((d) => ({
					id: d.id,
					access: d.access,
					paired: d.paired,
					expires: new Date(Date.parse(d.paired) + 86_400_000).toISOString(),
					current: d === dev
				}))
			});
		}
		if (p === '/api/devices' && req.method === 'DELETE') {
			devices.clear();
			return new Response(null, {
				status: 204,
				headers: { 'set-cookie': 'azazel_device=; Path=/; Max-Age=0' }
			});
		}
		if (p === '/api/folders' && req.method === 'POST') {
			const { under } = (await req.json().catch(() => ({}))) as { under?: string };
			const folder = (path: string, kind: string, trusted = true) => ({
				path,
				name: path.replace(/^\/home\/dev/, '~'),
				kind,
				trusted,
				startable: true
			});
			return j(
				under
					? {
							at: under,
							parent: under.replace(/\/[^/]+$/, '') || null,
							folders: ['src', 'tests', 'docs'].map((n) => folder(`${under}/${n}`, 'dir', false)),
							anywhere: dev.full
						}
					: {
							at: null,
							parent: null,
							folders: [
								...[...new Set(scenarios.map((s) => s.cwd))].map((c) => folder(c, 'project')),
								folder('/home/dev', 'root', false)
							],
							anywhere: dev.full
						}
			);
		}
		if (p === '/api/history' && req.method === 'POST') {
			if (access !== 'control')
				return j({ error: 'A view device cannot list earlier sessions.' }, { status: 403 });
			const { query } = (await req.json().catch(() => ({}))) as { query?: string };
			const q = query?.toLowerCase() ?? '';
			return j({
				sessions: scenarios
					.filter((s) => !q || `${s.name ?? ''} ${s.cwd}`.toLowerCase().includes(q))
					.map((s, i) => ({
						id: `saved-${s.id}`,
						name: s.name,
						cwd: s.cwd,
						created: s.started,
						updated: s.started,
						first_prompt: s.name,
						messages: 4 + i * 3,
						running: i === 0 ? s.id : null
					}))
			});
		}
		if (p === '/api/sessions' && req.method === 'POST') {
			if (access !== 'control')
				return j({ error: 'A view device cannot start sessions.' }, { status: 403 });
			const body = (await req.json().catch(() => ({}))) as {
				cwd?: string;
				prompt?: string;
				mode?: string;
			};
			const sc: Scenario = {
				...scenarios.find((x) => x.frames === 'showcase.jsonl')!,
				id: `new${++ids.counter}`,
				name: body.prompt ? body.prompt.slice(0, 40) : null,
				state: 'idle',
				cwd: body.cwd ?? '/home/dev/project',
				cost: null,
				mode: body.mode ?? 'manual',
				started: new Date().toISOString(),
				prefill: () => 0,
				autostart: false
			};
			scenarios.unshift(sc);
			const ms = new MockSession(sc);
			sessions.set(sc.id, ms);
			if (body.prompt) ms.start(false, body.prompt);
			return j({ id: sc.id });
		}
		if (p === '/api/sessions') {
			return j({
				sessions: scenarios.map((s) => ({
					id: s.id,
					name: s.name,
					kind: 'background',
					cwd: s.cwd,
					cost_usd: s.cost,
					mode: s.mode,
					started: s.started,
					state: sessions.get(s.id)?.pendingFrames.size
						? 'waiting for you'
						: sessions.get(s.id)?.busy
							? 'working'
							: s.state
				}))
			});
		}
		if (p === '/api/links' && req.method === 'POST') {
			const { access: want } = (await req.json().catch(() => ({}))) as {
				access?: 'view' | 'control';
			};
			if (access === 'view' && want === 'control')
				return j({ error: 'A view device cannot make control links.' }, { status: 403 });
			const secret = `${want === 'view' ? 'view' : 'ctl'}${Math.random().toString(36).slice(2)}${Math.random().toString(36).slice(2)}`;
			links.set(secret, want ?? 'view');
			return j({
				url: `${url.origin}/#pair=${secret}`,
				access: want,
				expires: new Date(Date.now() + 15 * 60_000).toISOString()
			});
		}
		if (p === '/api/logout' && req.method === 'POST') {
			devices.delete(token(req) ?? '');
			return new Response(null, {
				status: 204,
				headers: { 'set-cookie': 'azazel_device=; Max-Age=0; Path=/' }
			});
		}
		const m = /^\/api\/ws\/([^/]+)$/.exec(p);
		if (m) {
			const id = decodeURIComponent(m[1]);
			return srv.upgrade(req, { data: { session: id, access, full: dev.full } })
				? undefined
				: new Response('upgrade required', { status: 426 });
		}
		return j({ error: 'No such route.' }, { status: 404 });
	},
	websocket: {
		open(ws) {
			const { session, access } = ws.data;
			if (session === '!401') return ws.close(4401, 'not signed in');
			const s = sessions.get(session);
			if (!s) return ws.close(4404, 'no such session');
			const c = new Client(ws);
			(ws as unknown as { client: Client }).client = c;
			s.clients.add(c);
			void access;
			c.send({ type: 'hello', protocol: 1, version: VERSION });
		},
		message(ws, raw) {
			const { session, access, full } = ws.data;
			const s = sessions.get(session);
			const c = (ws as unknown as { client: Client }).client;
			if (!s || !c) return;
			let msg: Frame;
			try {
				msg = JSON.parse(String(raw));
			} catch {
				return c.send({ type: 'response', id: null, success: false, error: 'not JSON' });
			}
			handle(s, c, access, full, msg);
		},
		close(ws) {
			const c = (ws as unknown as { client?: Client }).client;
			if (!c) return;
			const s = sessions.get(ws.data.session);
			s?.clients.delete(c);
			s?.browser.leave(c);
		}
	}
});

console.log(`mock azazel remote on http://127.0.0.1:${server.port}`);
console.log(`control link: http://127.0.0.1:${server.port}/#pair=ctl-demo`);
console.log(`view link:    http://127.0.0.1:${server.port}/#pair=view-demo`);
console.log(`full access:  http://127.0.0.1:${server.port}/#pair=full-demo`);
