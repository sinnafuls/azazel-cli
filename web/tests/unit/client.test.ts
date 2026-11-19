import { describe, expect, it } from 'vitest';
import { backoffMs, RpcClient, type SocketLike, type Status, type Timers } from '$lib/rpc/client';
import type { AnyFrame } from '$lib/protocol';

class FakeSocket implements SocketLike {
	onopen = null;
	onmessage: SocketLike['onmessage'] = null;
	onclose: SocketLike['onclose'] = null;
	onerror = null;
	sent: Record<string, unknown>[] = [];
	closed = false;
	send(d: string) {
		this.sent.push(JSON.parse(d));
	}
	close() {
		this.closed = true;
	}
	recv(o: object) {
		this.onmessage?.({ data: JSON.stringify(o) });
	}
	drop(code = 1006) {
		this.onclose?.({ code });
	}
}

function harness(since: () => number | null = () => null) {
	const sockets: FakeSocket[] = [];
	const timeouts: { fn: () => void; ms: number }[] = [];
	const timers: Timers = {
		setTimeout: (fn, ms) => (timeouts.push({ fn, ms }), timeouts.length),
		clearTimeout: () => {}
	};
	const statuses: Status[] = [];
	const frames: AnyFrame[] = [];
	const attached: { resync: boolean; gap?: boolean }[] = [];
	const client = new RpcClient({
		url: 'ws://x/api/ws/s1',
		since,
		factory: () => {
			const s = new FakeSocket();
			sockets.push(s);
			return s;
		},
		timers,
		backoff: (n) => 100 * 2 ** n,
		onFrame: (f) => frames.push(f),
		onAttached: (a, resync) => attached.push({ resync, gap: a.gap }),
		onStatus: (s) => statuses.push(s)
	});
	const hello = (s: FakeSocket) => s.recv({ type: 'hello', protocol: 1, version: 'x' });
	const accept = (s: FakeSocket, data: object = { cursor: 5 }) => {
		const attach = s.sent.at(-1)!;
		s.recv({
			type: 'response',
			id: attach.id,
			success: true,
			data: { host: 'h', pid: 1, session_id: 's', session_file: 'f', ...data }
		});
	};
	return { client, sockets, timeouts, statuses, frames, attached, hello, accept };
}

describe('reconnect', () => {
	it('attaches after hello and routes responses to the command that asked', async () => {
		const h = harness();
		h.client.connect();
		h.hello(h.sockets[0]);
		expect(h.sockets[0].sent[0]).toMatchObject({ type: 'attach' });
		h.accept(h.sockets[0]);
		expect(h.client.status).toBe('open');

		const p = h.client.send({ type: 'get_state' });
		const cmd = h.sockets[0].sent.at(-1)!;
		h.sockets[0].recv({ type: 'response', id: cmd.id, success: true, data: { busy: false } });
		await expect(p).resolves.toEqual({ busy: false });

		const bad = h.client.send({ type: 'abort' });
		h.sockets[0].recv({
			type: 'response',
			id: h.sockets[0].sent.at(-1)!.id,
			success: false,
			error: 'A view link cannot send `abort`.',
			code: 'refused'
		});
		await expect(bad).rejects.toMatchObject({
			message: expect.stringContaining('view link'),
			code: 'refused'
		});
	});

	it('comes back after a drop with backoff and asks only for what it missed', () => {
		let seen: number | null = null;
		const h = harness(() => seen);
		h.client.connect();
		h.hello(h.sockets[0]);
		h.accept(h.sockets[0]);
		expect(h.attached[0].resync).toBe(true); // first connect builds from the snapshot

		h.sockets[0].recv({ type: 'idle', agent: 'lead', stop: 'stop', at: 17 });
		seen = 17;
		h.sockets[0].drop();
		expect(h.client.status).toBe('reconnecting');
		expect(h.timeouts.at(-1)!.ms).toBe(100);
		h.timeouts.at(-1)!.fn();
		h.hello(h.sockets[1]);
		expect(h.sockets[1].sent[0]).toMatchObject({ type: 'attach', since: 17 });
		h.accept(h.sockets[1], { cursor: 20 });
		expect(h.attached[1].resync).toBe(false);

		// Success resets the ladder; failures without success climb it.
		h.sockets[1].drop();
		expect(h.timeouts.at(-1)!.ms).toBe(100);
		h.timeouts.at(-1)!.fn();
		h.sockets[2].drop();
		expect(h.timeouts.at(-1)!.ms).toBe(200);
	});

	it('rebuilds from the snapshot when the host says the cursor is unknown', () => {
		const h = harness(() => 99);
		h.client.connect();
		h.hello(h.sockets[0]);
		h.accept(h.sockets[0], { cursor: 3, gap: true });
		expect(h.attached[0]).toEqual({ resync: true, gap: true });
	});

	it('fails commands in flight when the socket drops', async () => {
		const h = harness();
		h.client.connect();
		h.hello(h.sockets[0]);
		h.accept(h.sockets[0]);
		const p = h.client.send({ type: 'get_state' });
		h.sockets[0].drop();
		await expect(p).rejects.toThrow('Connection lost');
		await expect(h.client.send({ type: 'get_state' })).rejects.toThrow('Not connected');
	});

	it('marks a command lost with the socket as disconnected, so a caller can check what the host did', async () => {
		const h = harness();
		h.client.connect();
		h.hello(h.sockets[0]);
		h.accept(h.sockets[0]);
		const p = h.client.send({ type: 'handoff' });
		h.client.reset();
		await expect(p).rejects.toMatchObject({ code: 'disconnected' });
	});

	it('does not retry when signed out or the session is gone, and resumes at once after 4408', () => {
		const h = harness();
		h.client.connect();
		h.sockets[0].drop(4401);
		expect(h.client.status).toBe('unauthorized');
		expect(h.timeouts).toHaveLength(0);

		const g = harness();
		g.client.connect();
		g.sockets[0].drop(4404);
		expect(g.client.status).toBe('missing');

		const s = harness();
		s.client.connect();
		s.sockets[0].drop(4408);
		expect(s.timeouts.at(-1)!.ms).toBe(0);
	});

	it('ignores late messages from a socket it replaced and stays closed once closed', () => {
		const h = harness();
		h.client.connect();
		h.hello(h.sockets[0]);
		h.accept(h.sockets[0]);
		h.client.reset();
		expect(h.sockets).toHaveLength(2);
		h.sockets[0].recv({ type: 'idle', agent: 'lead', stop: 'stop' });
		expect(h.frames).toHaveLength(0);
		h.client.close();
		h.sockets[1].drop();
		expect(h.timeouts).toHaveLength(0);
		expect(h.client.status).toBe('closed');
	});

	it('backoff grows, is capped and is jittered', () => {
		expect(backoffMs(0, () => 0.5)).toBe(500);
		expect(backoffMs(3, () => 0.5)).toBe(4000);
		expect(backoffMs(20, () => 0.5)).toBe(15000);
		expect(backoffMs(2, () => 0)).toBeLessThan(backoffMs(2, () => 1));
	});
});
