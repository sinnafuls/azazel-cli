import { test as base, expect, type Page } from '@playwright/test';
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs';
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
	/** The first pairing link, `http://127.0.0.1:<port>/#pair=<secret>`. */
	pair = '';
	port = 0;
	sessionId = '';

	private azazel(args: string[]) {
		return [
			'--set',
			'session.auto_name=false',
			'--no-memory',
			'--set',
			'notify.desktop="off"',
			'--cwd',
			join(this.dir, 'project'),
			...args
		];
	}

	private env() {
		return {
			...process.env,
			AZAZEL_HOME: join(this.dir, 'home'),
			AZAZEL_MOCK_SCRIPT: join(this.dir, 'script.json')
		};
	}

	private run(args: string[]) {
		const out = spawnSync(binary, this.azazel(args), { env: this.env(), encoding: 'utf8' });
		if (out.status !== 0) throw new Error(`azazel ${args.join(' ')}: ${out.stdout}${out.stderr}`);
		return out.stdout;
	}

	/** Starts the session (in `mode`: `manual` waits for approvals) and the server. */
	async start(mode: 'manual' | 'auto') {
		if (!existsSync(binary))
			throw new Error(`${binary} is missing: build it with --features test-support,remote`);
		mkdirSync(join(this.dir, 'project', '.git'), { recursive: true });
		mkdirSync(join(this.dir, 'home'), { recursive: true });
		cpSync(join(root, 'tests/fixtures/scripts/web_session.json'), join(this.dir, 'script.json'));
		this.sessionId = this.run(['--mode', mode, 'bg', 'create hello']).trim();
		const wanted = mode === 'manual' ? 'waiting for you' : 'idle';
		const deadline = Date.now() + 30_000;
		for (;;) {
			const rows = this.run(['ps', '--json'])
				.split('\n')
				.filter((l) => l.startsWith('{'))
				.map((l) => JSON.parse(l));
			if (rows.some((r) => r.id === this.sessionId && r.state === wanted)) break;
			if (Date.now() > deadline) throw new Error(`session ${this.sessionId} never was ${wanted}`);
			await new Promise((r) => setTimeout(r, 100));
		}
		this.child = spawn(binary, this.azazel(['remote', '--port', '0', '--json']), {
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

	async stop() {
		if (this.child) {
			this.child.stdin!.write('q\n');
			this.child.kill();
		}
		try {
			spawnSync(binary, this.azazel(['kill', this.sessionId]), { env: this.env() });
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
