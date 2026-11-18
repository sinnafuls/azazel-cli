import { beforeEach, describe, expect, it, vi } from 'vitest';

// A plain function, not a spy: a spy records the promise it returns in a way that reports a
// rejection as unhandled.
const asked = vi.hoisted(() => ({
	n: 0,
	answer: (() => Promise.reject(new Error('unset'))) as () => Promise<unknown>
}));
vi.mock('$lib/ui.svelte', () => ({
	ui: {
		session: {
			status: 'open',
			send: () => {
				asked.n++;
				return asked.answer();
			}
		}
	}
}));
vi.mock('$lib/sessions.svelte', () => ({ sessions: { list: [] } }));
vi.mock('$lib/rpc/oneshot', () => ({ oneShot: () => Promise.reject(new Error('unused')) }));
const me = vi.hoisted(() => ({ access: 'view', full_access: false }));
vi.mock('$lib/app.svelte', () => ({ app: { me } }));

import { hostRpc, Refused } from '$lib/host';
import { CommandError } from '$lib/rpc/client';

const VIEW = new CommandError(
	'this is a view link: `get_settings` needs a control link',
	'refused'
);

describe('a command the computer refuses to this device', () => {
	beforeEach(() => {
		asked.n = 0;
	});

	it('ends as a quiet refusal, and a read that was refused is not asked again', async () => {
		asked.answer = () => Promise.reject(VIEW);
		for (let i = 0; i < 3; i++)
			await expect(hostRpc({ type: 'get_settings' })).rejects.toBeInstanceOf(Refused);
		expect(asked.n).toBe(1);
	});

	it('does not read a message as a refusal: only the code says so', async () => {
		asked.answer = () => Promise.reject(new CommandError('this is a view link: needs full access'));
		await expect(hostRpc({ type: 'get_usage' })).rejects.toThrow('view link');
	});

	it('lets other failures through as they are', async () => {
		asked.answer = () => Promise.reject(new Error('The computer did not answer.'));
		await expect(hostRpc({ type: 'get_usage' })).rejects.toThrow('did not answer');
		await expect(hostRpc({ type: 'get_usage' })).rejects.toThrow('did not answer');
		expect(asked.n).toBe(2);
	});

	it('asks again once the device has more access', async () => {
		asked.answer = () => Promise.reject(VIEW);
		await expect(hostRpc({ type: 'get_cost' })).rejects.toBeInstanceOf(Refused);
		Object.assign(me, { access: 'control' });
		asked.answer = () => Promise.resolve({ ok: 1 });
		await expect(hostRpc({ type: 'get_cost' })).resolves.toEqual({ ok: 1 });
	});
});
