import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import Composer from '$lib/components/Composer.svelte';
import { toasts } from '$lib/toasts.svelte';
import { addQueued, emptyModel } from '$lib/session/reduce';
import type { Session } from '$lib/session/session.svelte';

/** Just what the message box reads of a session. */
function fake(queue: { kind: string; text: string }[], busy = true) {
	let model = emptyModel();
	for (const q of queue) model = addQueued(model, q.kind as 'follow_up', q.text);
	const prompt = vi.fn(async () => {});
	const session = {
		model: { ...model, runs: { lead: { busy } } },
		busy,
		commands: [{ name: 'cost', description: 'What it cost' }],
		files: [],
		prompt,
		loadCommands: async () => {},
		loadFiles: async () => {}
	} as unknown as Session;
	return { session, prompt };
}

afterEach(() => {
	cleanup();
	toasts.items = [];
});

describe('the message box queue', () => {
	it('lists the follow-ups that wait for the agent, oldest first, with when each is delivered', () => {
		render(Composer, {
			session: fake([
				{ kind: 'follow_up', text: 'then run the tests' },
				{ kind: 'steer', text: 'use the cache' }
			]).session,
			canControl: true
		});
		const list = screen.getByRole('list', { name: 'Queued messages' });
		const rows = within(list).getAllByRole('listitem');
		expect(rows.map((r) => r.textContent?.replace(/\s+/g, ' ').trim())).toEqual([
			'then run the tests when it is done',
			'use the cache next step'
		]);
	});

	it('shows no list when nothing waits', () => {
		render(Composer, { session: fake([]).session, canControl: true });
		expect(screen.queryByRole('list', { name: 'Queued messages' })).toBeNull();
	});
});

describe('a lone /word that is no command', () => {
	it('stays in the box and is not sent to the model', async () => {
		const { session, prompt } = fake([], false);
		render(Composer, { session, canControl: true });
		const box = screen.getByRole('combobox', { name: 'Message' }) as HTMLTextAreaElement;
		await fireEvent.input(box, { target: { value: '/trust' } });
		// Escape closes the command menu, so that Enter sends the line instead of picking from it.
		await fireEvent.keyDown(box, { key: 'Escape' });
		await fireEvent.submit(box.closest('form') as HTMLFormElement);
		await vi.waitFor(() => expect(toasts.items).toHaveLength(1));
		expect(toasts.items[0].text).toContain('/trust');
		expect(toasts.items[0].text).toContain('full access');
		expect(prompt).not.toHaveBeenCalled();
		expect(box.value).toBe('/trust');
	});

	it('still sends an ordinary message', async () => {
		const { session, prompt } = fake([], false);
		render(Composer, { session, canControl: true });
		const box = screen.getByRole('combobox', { name: 'Message' }) as HTMLTextAreaElement;
		await fireEvent.input(box, { target: { value: 'look at /trust' } });
		await fireEvent.submit(box.closest('form') as HTMLFormElement);
		await vi.waitFor(() => expect(prompt).toHaveBeenCalledWith('look at /trust', []));
	});
});
