import { test as base, expect, type Page } from '@playwright/test';
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import {
	cpSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	rmSync,
	writeFileSync
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';

const root = resolve(import.meta.dirname, '../../..');
const exe = process.platform === 'win32' ? 'azazel.exe' : 'azazel';
// A build with `--features test-support,remote`; AZAZEL_BIN points at another one.
const binary =
	process.env.AZAZEL_BIN ??
	join(process.env.CARGO_TARGET_DIR ?? join(root, 'target'), 'debug', exe);

/** `azazel remote` in front of one scripted background session, in a home and project of their own. */
export class Stack {
	private child: ChildProcess | null = null;
	private lines: string[] = [];
	private waiters: { wanted: string; resolve: (line: string) => void }[] = [];
	readonly dir = mkdtempSync(join(tmpdir(), 'azazel-e2e-'));
	/** The folder the scripted session works in. */
	get project() {
		return join(this.dir, 'project');
	}

	/**
	 * What the scripted run wrote. The script's closing text says the file was written whatever
	 * happened, so a test that waits for it checks the file as well. The project is new for every
	 * stack (see `start`), so the file is there only when this run's `write` succeeded.
	 */
	hello(dir = this.project) {
		return readFileSync(join(dir, 'hello.txt'), 'utf8');
	}

	/** The first pairing link, `http://127.0.0.1:<port>/#pair=<secret>`. */
	pair = '';
	port = 0;
	sessionId = '';
	/** Offers the sessions started from now on the tool `test_wait`, a call that takes as long as it is told to. */
	mockTools = false;

	private azazel(args: string[], cwd = this.project) {
		return [
			'--set',
			'session.auto_name=false',
			'--no-memory',
			'--set',
			'notify.desktop="off"',
			'--cwd',
			cwd,
			...args
		];
	}

	private env() {
		return {
			...process.env,
			AZAZEL_HOME: join(this.dir, 'home'),
			AZAZEL_MOCK_SCRIPT: join(this.dir, 'script.json'),
			...(this.mockTools ? { AZAZEL_MOCK_TOOLS: '1' } : {})
		};
	}

	private run(args: string[], cwd = this.project) {
		const out = spawnSync(binary, this.azazel(args, cwd), { env: this.env(), encoding: 'utf8' });
		if (out.status !== 0) throw new Error(`azazel ${args.join(' ')}: ${out.stdout}${out.stderr}`);
		return out.stdout;
	}

	/** The state `azazel ps` reports for every session of this home. */
	private states(): Map<string, string> {
		return new Map(
			this.run(['ps', '--json'])
				.split('\n')
				.filter((l) => l.startsWith('{'))
				.map((l) => JSON.parse(l) as { id: string; state: string })
				.map((r) => [r.id, r.state])
		);
	}

	/** Waits until every session of `ids` is in the state named for it. */
	async until(ids: Record<string, string>) {
		const deadline = Date.now() + 30_000;
		for (;;) {
			const now = this.states();
			if (Object.entries(ids).every(([id, state]) => now.get(id) === state)) return;
			if (Date.now() > deadline)
				throw new Error(`sessions never were ${JSON.stringify(ids)}: ${JSON.stringify([...now])}`);
			await new Promise((r) => setTimeout(r, 100));
		}
	}

	/**
	 * Starts one more scripted session and returns its id, without waiting for it (see `until`).
	 * Every session reads the script from its first step, so `script` is what this one is answered
	 * with. `cwd` is a folder of this stack's directory, made when it is missing.
	 */
	session(prompt: string, script: unknown, mode: 'manual' | 'auto' = 'auto', cwd = this.project) {
		mkdirSync(join(cwd, '.git'), { recursive: true });
		writeFileSync(join(this.dir, 'script.json'), JSON.stringify(script));
		return this.run(['--mode', mode, 'bg', prompt], cwd).trim();
	}

	/** Ends a session: it stays on disk as an earlier session. */
	end(id: string) {
		this.run(['kill', id]);
	}

	/**
	 * Starts the session (in `mode`: `manual` waits for approvals) and the server, with `remote` added
	 * to its arguments. `script` answers the session in place of tests/fixtures/scripts/web_session.json.
	 */
	async start(
		mode: 'manual' | 'auto',
		failure = false,
		remote: string[] = [],
		script: unknown = null
	) {
		if (!existsSync(binary))
			throw new Error(`${binary} is missing: build it with --features test-support,remote`);
		// A file left over from an earlier run would make `write` fail while the script still claims it worked.
		if (existsSync(join(this.project, 'hello.txt')))
			throw new Error('the project is not new: hello.txt exists before the run');
		mkdirSync(join(this.project, '.git'), { recursive: true });
		mkdirSync(join(this.dir, 'home'), { recursive: true });
		cpSync(join(root, 'tests/fixtures/scripts/web_session.json'), join(this.dir, 'script.json'));
		if (failure) {
			writeFileSync(
				join(this.dir, 'script.json'),
				JSON.stringify({
					lead: [
						{
							events: [
								{
									type: 'error',
									error: { kind: 'invalid_request', detail: 'Test account has no quota' }
								}
							]
						}
					]
				})
			);
		}
		if (script) writeFileSync(join(this.dir, 'script.json'), JSON.stringify(script));
		this.sessionId = this.run(['--mode', mode, 'bg', 'create hello']).trim();
		await this.until({ [this.sessionId]: mode === 'manual' ? 'waiting for you' : 'idle' });
		this.child = spawn(binary, this.azazel(['remote', '--port', '0', '--json', ...remote]), {
			env: this.env(),
			stdio: ['pipe', 'pipe', 'pipe']
		});
		const out = createInterface({ input: this.child.stdout! });
		const first = new Promise<string>((res, rej) => {
			out.once('line', res);
			this.child!.once('exit', () =>
				rej(new Error('azazel remote ended before it printed a link'))
			);
		});
		// The server writes its prompts and links to standard error; keep reading so the pipe never fills.
		createInterface({ input: this.child.stderr! }).on('line', (line) => this.heard(line));
		const info = JSON.parse(await first);
		this.pair = info.pair;
		this.port = info.port;
	}

	private heard(line: string) {
		this.lines.push(line);
		for (const w of this.waiters.filter((w) => line.includes(w.wanted))) w.resolve(line);
		this.waiters = this.waiters.filter((w) => !line.includes(w.wanted));
	}

	/** How many lines the server has printed so far. */
	heardSoFar() {
		return this.lines.length;
	}

	/** The first line the server prints after the first `since`, that contains `wanted`. */
	heardLine(wanted: string, since = 0) {
		const already = this.lines.slice(since).find((l) => l.includes(wanted));
		if (already) return Promise.resolve(already);
		return new Promise<string>((res) => this.waiters.push({ wanted, resolve: res }));
	}

	/** Types `text` + Enter on the server's stdin. */
	type(text: string) {
		this.child!.stdin!.write(`${text}\n`);
	}

	/** Types `key` + Enter on the server's stdin (`l` makes a control link) and returns the next link it prints. */
	async newLink(key = 'l') {
		const seen = this.lines.length;
		const next = new Promise<string>((res) => {
			this.waiters.push({ wanted: '#pair=', resolve: res });
			// A link printed between the write and the waiter is still found.
			setTimeout(() => {
				const late = this.lines.slice(seen).find((l) => l.includes('#pair='));
				if (late) res(late);
			}, 500);
		});
		this.child!.stdin!.write(`${key}\n`);
		const line = await next;
		return line.slice(line.indexOf('http'));
	}

	/** Ends the session the stack started with: it stays on disk as an earlier session. */
	endSession() {
		this.end(this.sessionId);
	}

	async stop() {
		if (this.child) {
			this.child.stdin!.write('q\n');
			this.child.kill();
		}
		// The session it started with, and every one a browser started.
		try {
			for (const id of new Set([this.sessionId, ...this.states().keys()]))
				spawnSync(binary, this.azazel(['kill', id]), { env: this.env() });
		} catch {
			// Already gone.
		}
		rmSync(this.dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
	}
}

export const test = base.extend<{ manual: Stack; auto: Stack }>({
	// Playwright reads the fixtures a function wants from its destructured argument.
	// eslint-disable-next-line no-empty-pattern
	manual: async ({}, use) => {
		const stack = new Stack();
		try {
			await stack.start('manual');
			await use(stack);
		} finally {
			await stack.stop();
		}
	},
	// eslint-disable-next-line no-empty-pattern
	auto: async ({}, use) => {
		const stack = new Stack();
		try {
			await stack.start('auto');
			await use(stack);
		} finally {
			await stack.stop();
		}
	}
});

export { expect, type Page };
