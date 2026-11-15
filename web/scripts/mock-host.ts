// A stand-in for `azazel remote` so the page can be built and tested without Rust.
//
//   bun run dev:mock            serves the API on :7420 (and web/build when it exists)
//   bun run dev                 then the Vite dev server proxies /api here
//
// It speaks the contract of docs/remote.md: /api/pair, /api/me, /api/sessions, /api/links,
// /api/logout and /api/ws/{id}, and replays frames recorded from the real `azazel rpc` with the
// scripted provider (fixtures/*.jsonl). A secret starting with `view` pairs a view-only device.
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
	'list_commands'
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
}

class MockSession {
	log: Frame[] = [];
	rows: Frame[] = [];
	clients = new Set<Client>();
	waiting = new Map<string, () => void>();
	pendingFrames = new Map<string, Frame>();
	state: Frame;
	runId = 0;
	started = false;
	aborted = false;
	busy = false;

	constructor(readonly s: Scenario) {
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
		} else if (f.type === 'idle' && f.agent === 'lead') {
			this.busy = false;
		}
	}

	snapshot() {
		return {
			busy: this.busy,
			tail: this.rows.slice(-20),
			streaming: [],
			tools: [],
			queue: [],
			pending: [...this.pendingFrames.keys()]
		};
	}

	async run(frames: Frame[], prefill: number, prompt: string) {
		const my = ++this.runId;
		this.aborted = false;
		this.emit({ type: 'user_input', kind: 'prompt', text: prompt });
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

const devices = new Map<string, 'view' | 'control'>();
const usedSecrets = new Set<string>();
const links = new Map<string, 'view' | 'control'>();

function device(req: Request): 'view' | 'control' | null {
	const cookie = req.headers.get('cookie') ?? '';
	const m = /azazel_device=([^;]+)/.exec(cookie);
	return m ? (devices.get(m[1]) ?? null) : null;
}

const me = (access: 'view' | 'control') => ({
	access,
	expires: new Date(Date.now() + 24 * 3600_000).toISOString(),
	allow_bypass: false,
	protocol: 1,
	version: '0.11.0-mock'
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

const roles = { roles: [], presets: [], project: false };
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
function fail(c: Client, id: unknown, error: string) {
	c.send({ type: 'response', id, success: false, error });
}

function handle(s: MockSession, c: Client, access: 'view' | 'control', msg: Frame) {
	const { id, type } = msg;
	if (access === 'view' && !VIEW_COMMANDS.has(type))
		return fail(c, id, `A view link cannot send \`${type}\`. Ask for a control link.`);
	switch (type) {
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
			return respond(c, id, json('commands.json'));
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
			s.emit({ type: 'user_input', kind: type === 'prompt' ? 'prompt' : type, text: msg.text }, c);
			if (type === 'prompt' && !s.busy) {
				const scenario = lines(s.s.frames);
				void s.run(scenario, 0, msg.text).catch(() => {});
				// `run` emits its own user_input; the sender does not see it twice because it ignores echoes.
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
			const token = `tok${++ids.counter}${Math.random().toString(36).slice(2)}`;
			devices.set(token, access);
			return j(me(access), {
				headers: {
					'set-cookie': `azazel_device=${token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400`
				}
			});
		}
		const access = device(req);
		if (!access) {
			if (p.startsWith('/api/ws/') && req.headers.get('upgrade') === 'websocket') {
				// A close code needs an upgraded socket.
				return srv.upgrade(req, { data: { session: '!401', access: 'view' } })
					? undefined
					: new Response('unauthorised', { status: 401 });
			}
			return j({ error: 'Not signed in. Open a pairing link.' }, { status: 401 });
		}
		if (p === '/api/me') return j(me(access));
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
					state: sessions.get(s.id)?.busy ? 'working' : s.state
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
			return new Response(null, {
				status: 204,
				headers: { 'set-cookie': 'azazel_device=; Max-Age=0; Path=/' }
			});
		}
		const m = /^\/api\/ws\/([^/]+)$/.exec(p);
		if (m) {
			const id = decodeURIComponent(m[1]);
			return srv.upgrade(req, { data: { session: id, access } })
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
			c.send({ type: 'hello', protocol: 1, version: '0.11.0-mock' });
		},
		message(ws, raw) {
			const { session, access } = ws.data;
			const s = sessions.get(session);
			const c = (ws as unknown as { client: Client }).client;
			if (!s || !c) return;
			let msg: Frame;
			try {
				msg = JSON.parse(String(raw));
			} catch {
				return c.send({ type: 'response', id: null, success: false, error: 'not JSON' });
			}
			handle(s, c, access, msg);
		},
		close(ws) {
			const c = (ws as unknown as { client?: Client }).client;
			if (c) sessions.get(ws.data.session)?.clients.delete(c);
		}
	}
});

console.log(`mock azazel remote on http://127.0.0.1:${server.port}`);
console.log(`control link: http://127.0.0.1:${server.port}/#pair=ctl-demo`);
console.log(`view link:    http://127.0.0.1:${server.port}/#pair=view-demo`);
