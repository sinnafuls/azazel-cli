// The WebSocket side of the page. One client is one connection to one session: it keeps command ids
// and the table of commands waiting for their response, attaches with the position of the last frame
// it saw (`since`) and, when the socket drops, comes back with backoff.
import type { AnyFrame, AttachedRemote, Command } from '$lib/protocol';

export type Status =
	| 'connecting'
	| 'open'
	| 'reconnecting'
	| 'ended' // the session ended (1000)
	| 'unauthorized' // 4401: pair again
	| 'missing' // 4404: no such session
	| 'closed'; // closed by the page

export interface SocketLike {
	onopen: ((ev?: unknown) => void) | null;
	onmessage: ((ev: { data: unknown }) => void) | null;
	onclose: ((ev: { code: number; reason?: string }) => void) | null;
	onerror: ((ev?: unknown) => void) | null;
	send(data: string): void;
	close(code?: number, reason?: string): void;
}
export type SocketFactory = (url: string) => SocketLike;

export interface Timers {
	setTimeout(fn: () => void, ms: number): unknown;
	clearTimeout(handle: unknown): void;
}

export interface ClientOptions {
	url: string;
	/** `at` of the last frame the page has seen; asked again on every connect. */
	since: () => number | null;
	onFrame: (frame: AnyFrame) => void;
	/** Called after `attach` succeeded. `resync` is true when the page must rebuild from the snapshot. */
	onAttached: (attached: AttachedRemote, resync: boolean) => void;
	onStatus: (status: Status, detail?: string) => void;
	factory?: SocketFactory;
	timers?: Timers;
	/** Milliseconds before reconnect attempt `n` (0-based). */
	backoff?: (n: number) => number;
	/** Reconnects after a 4408 do not wait. */
	random?: () => number;
}

export class CommandError extends Error {}

export function backoffMs(n: number, random: () => number = Math.random): number {
	const base = Math.min(15_000, 500 * 2 ** n);
	return Math.round(base * (0.75 + random() * 0.5));
}

const defaultFactory: SocketFactory = (url) => new WebSocket(url) as unknown as SocketLike;
const defaultTimers: Timers = {
	setTimeout: (fn, ms) => setTimeout(fn, ms),
	clearTimeout: (h) => clearTimeout(h as ReturnType<typeof setTimeout>)
};

interface Pending {
	resolve: (data: unknown) => void;
	reject: (err: Error) => void;
}

export class RpcClient {
	status: Status = 'connecting';
	private socket: SocketLike | null = null;
	private nextId = 1;
	private pending = new Map<number, Pending>();
	private attempts = 0;
	private timer: unknown = null;
	private stopped = false;
	private ready = false;
	private readonly o: ClientOptions;

	constructor(options: ClientOptions) {
		this.o = options;
	}

	private get timers() {
		return this.o.timers ?? defaultTimers;
	}

	private set(status: Status, detail?: string) {
		this.status = status;
		this.o.onStatus(status, detail);
	}

	connect() {
		if (this.stopped) return;
		this.clearTimer();
		this.ready = false;
		this.set(this.attempts === 0 ? 'connecting' : 'reconnecting');
		const socket = (this.o.factory ?? defaultFactory)(this.o.url);
		this.socket = socket;
		socket.onmessage = (ev) => this.onMessage(socket, ev.data);
		socket.onclose = (ev) => this.onClose(socket, ev.code, ev.reason);
		socket.onerror = () => {};
	}

	/** Reconnect now if the connection is down (the page became visible again, or the network came back). */
	nudge() {
		if (this.stopped || this.ready) return;
		if (this.status === 'reconnecting' || this.status === 'connecting') {
			if (this.timer !== null) this.connect();
		}
	}

	/** Sends a command and resolves with the response's `data`. */
	send<T = unknown>(command: Command): Promise<T> {
		const socket = this.socket;
		if (!socket || !this.ready) return Promise.reject(new CommandError('Not connected'));
		return new Promise<T>((resolve, reject) => {
			const id = this.nextId++;
			this.pending.set(id, { resolve: resolve as (d: unknown) => void, reject });
			try {
				socket.send(JSON.stringify({ ...command, id }));
			} catch (e) {
				this.pending.delete(id);
				reject(new CommandError(String(e)));
			}
		});
	}

	/** Drops a socket that may be dead (a phone woke up) and connects again at once. */
	reset() {
		if (this.stopped) return;
		const s = this.socket;
		this.socket = null;
		this.ready = false;
		this.failPending('Connection reset');
		s?.close(4000, 'reset');
		this.clearTimer();
		this.connect();
	}

	close() {
		this.stopped = true;
		this.clearTimer();
		this.failPending('Closed');
		const s = this.socket;
		this.socket = null;
		this.ready = false;
		s?.close(1000, 'page closed');
		this.set('closed');
	}

	private clearTimer() {
		if (this.timer !== null) this.timers.clearTimeout(this.timer);
		this.timer = null;
	}

	private failPending(why: string) {
		for (const p of this.pending.values()) p.reject(new CommandError(why));
		this.pending.clear();
	}

	private rawSend(socket: SocketLike, command: object): number {
		const id = this.nextId++;
		socket.send(JSON.stringify({ ...command, id }));
		return id;
	}

	private onMessage(socket: SocketLike, data: unknown) {
		if (socket !== this.socket || typeof data !== 'string') return;
		let frame: AnyFrame & { id?: number; success?: boolean; error?: string; data?: unknown };
		try {
			frame = JSON.parse(data);
		} catch {
			return;
		}
		if (frame.type === 'hello') {
			this.attach(socket);
			return;
		}
		if (frame.type === 'response') {
			const p = typeof frame.id === 'number' ? this.pending.get(frame.id) : undefined;
			if (!p) return;
			this.pending.delete(frame.id as number);
			if (frame.success) p.resolve(frame.data);
			else p.reject(new CommandError(frame.error ?? 'The command failed'));
			return;
		}
		this.o.onFrame(frame);
	}

	private attach(socket: SocketLike) {
		const since = this.o.since();
		const id = this.nextId++;
		this.pending.set(id, {
			resolve: (data) => {
				if (socket !== this.socket) return;
				const attached = data as AttachedRemote;
				// A host that does not know `since` answers without a cursor: rebuild from the snapshot.
				const resync = since === null || attached.gap === true || attached.cursor === undefined;
				this.ready = true;
				this.attempts = 0;
				this.set('open');
				this.o.onAttached(attached, resync);
			},
			reject: (err) => {
				if (socket !== this.socket) return;
				this.set('closed', err.message);
			}
		});
		// The snapshot gives the state at attach time, so nothing needs replaying; `since` closes the
		// gap of a reconnect without it.
		const cmd: Record<string, unknown> = { type: 'attach', replay: 'none', id };
		if (since !== null) cmd.since = since;
		socket.send(JSON.stringify(cmd));
	}

	private onClose(socket: SocketLike, code: number, reason?: string) {
		if (socket !== this.socket) return;
		this.socket = null;
		this.ready = false;
		this.failPending('Connection lost');
		if (this.stopped) return;
		if (code === 4401) return this.set('unauthorized', reason);
		if (code === 4404) return this.set('missing', reason);
		if (code === 1000) return this.set('ended', reason);
		if (code === 1003 || code === 1009) return this.set('closed', reason);
		// 4408: the host closed us for being slow; resume at once. Anything else: back off.
		const delay =
			code === 4408 ? 0 : (this.o.backoff ?? ((n) => backoffMs(n, this.o.random)))(this.attempts);
		this.attempts++;
		this.set('reconnecting', reason);
		this.timer = this.timers.setTimeout(() => this.connect(), delay);
	}
}
