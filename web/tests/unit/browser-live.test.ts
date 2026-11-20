import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import BrowserPanel from '$lib/components/BrowserPanel.svelte';
import { BrowserLive, HOLD_HEARTBEAT_MS, type BrowserCommand } from '$lib/browser/live.svelte';
import { CommandError } from '$lib/rpc/client';
import { toasts } from '$lib/toasts.svelte';
import type { BrowserState, FrameOf } from '$lib/protocol';
import type { Session } from '$lib/session/session.svelte';

const OPEN: BrowserState = {
	running: true,
	tab: 't1',
	url: 'https://example.com/',
	title: 'Example Domain'
};
const FRAME: FrameOf<'browser_frame'> = {
	type: 'browser_frame',
	tab: 't1',
	data: 'AAAA',
	width: 1280,
	height: 720,
	seq: 1
};

/** A host that answers every command with its state, and remembers what it was sent. */
function host(initial: BrowserState = OPEN, refuse?: (c: BrowserCommand) => Error | undefined) {
	let state = initial;
	const sent: BrowserCommand[] = [];
	const send = vi.fn(async (c: BrowserCommand) => {
		sent.push(c);
		const no = refuse?.(c);
		if (no) throw no;
		if (c.type === 'browser_take') state = { ...state, held: c.on, held_by_you: c.on };
		return state;
	});
	const live = new BrowserLive(send);
	const watches = () => sent.filter((c) => c.type === 'browser_watch').map((c) => c.on);
	return { live, sent, watches, set: (s: BrowserState) => (state = s) };
}
const settle = () => new Promise((r) => setTimeout(r, 0));

/** Just what the panel reads of a session. */
const session = (live: BrowserLive, busy = false) =>
	({ browser: live, busy }) as unknown as Session;

function setVisible(visible: boolean) {
	Object.defineProperty(document, 'visibilityState', {
		configurable: true,
		get: () => (visible ? 'visible' : 'hidden')
	});
	document.dispatchEvent(new Event('visibilitychange'));
}

afterEach(() => {
	cleanup();
	toasts.items = [];
	setVisible(true);
});

describe('holding the browser', () => {
	it('tells the host now and then that it is still there, only while it holds', async () => {
		vi.useFakeTimers();
		try {
			const { live, sent } = host();
			live.attached();
			await vi.advanceTimersByTimeAsync(0);
			const takes = () => sent.filter((c) => c.type === 'browser_take').length;
			await vi.advanceTimersByTimeAsync(HOLD_HEARTBEAT_MS * 2);
			expect(takes()).toBe(0);
			await live.take(true);
			expect(takes()).toBe(1);
			await vi.advanceTimersByTimeAsync(HOLD_HEARTBEAT_MS);
			expect(takes()).toBe(2);
			await vi.advanceTimersByTimeAsync(HOLD_HEARTBEAT_MS);
			expect(takes()).toBe(3);
			await live.take(false);
			await vi.advanceTimersByTimeAsync(HOLD_HEARTBEAT_MS * 3);
			expect(takes()).toBe(4);
		} finally {
			vi.useRealTimers();
		}
	});
});

describe('the subscription to pictures', () => {
	it('asks for pictures only once the socket is attached', async () => {
		const h = host();
		h.live.watch(true);
		await settle();
		expect(h.watches()).toEqual([]);
		h.live.attached();
		await settle();
		expect(h.watches()).toEqual([true]);
		expect(h.live.watching).toBe(true);
		expect(h.live.state).toEqual(OPEN);
	});

	it('stops when nobody looks, and forgets the picture', async () => {
		const h = host();
		h.live.attached();
		h.live.watch(true);
		await settle();
		h.live.onFrame(FRAME);
		expect(h.live.picture).toMatchObject({ width: 1280, height: 720, seq: 1 });
		expect(h.live.picture?.src).toBe('data:image/jpeg;base64,AAAA');
		h.live.watch(false);
		await settle();
		expect(h.watches()).toEqual([true, false]);
		expect(h.live.watching).toBe(false);
		expect(h.live.picture).toBeNull();
		// A picture that was on its way is not shown.
		h.live.onFrame({ ...FRAME, seq: 2 });
		expect(h.live.picture).toBeNull();
	});

	it('sends nothing when it is asked for what it already has', async () => {
		const h = host();
		h.live.attached();
		h.live.watch(true);
		h.live.watch(true);
		await settle();
		h.live.watch(false);
		h.live.watch(false);
		await settle();
		expect(h.watches()).toEqual([true, false]);
	});

	it('ends in what was asked for last, however fast it changed', async () => {
		const h = host();
		h.live.attached();
		h.live.watch(true);
		h.live.watch(false);
		h.live.watch(true);
		await settle();
		expect(h.live.watching).toBe(true);
		expect(h.watches().at(-1)).toBe(true);
	});

	it('watches while at least one part of the page shows the browser', async () => {
		const h = host();
		h.live.attached();
		const a = h.live.want();
		const b = h.live.want();
		await settle();
		a();
		a();
		await settle();
		expect(h.watches()).toEqual([true]);
		b();
		await settle();
		expect(h.watches()).toEqual([true, false]);
	});

	it('asks again after a reconnect, since the new socket has no subscription', async () => {
		const h = host();
		h.live.attached();
		h.live.watch(true);
		await settle();
		h.live.detached();
		expect(h.live.watching).toBe(false);
		h.live.attached();
		await settle();
		expect(h.watches()).toEqual([true, true]);
	});

	it('lets go of a take-over when the socket goes, as the host does', async () => {
		const h = host();
		h.live.attached();
		await h.live.take(true);
		expect(h.live.state?.held_by_you).toBe(true);
		h.live.detached();
		expect(h.live.state).toMatchObject({ running: true, held: false, held_by_you: false });
	});

	it('knows a device that may not watch, and does not ask over and over', async () => {
		const h = host(OPEN, (c) =>
			c.type === 'browser_watch'
				? new CommandError('A view link cannot watch.', 'refused')
				: undefined
		);
		h.live.attached();
		h.live.watch(true);
		await settle();
		expect(h.live.denied).toBe(true);
		expect(h.live.error).toBeNull();
		h.live.watch(true);
		await settle();
		expect(h.watches()).toEqual([true]);
	});

	it('says why a command failed, and clears it when the next one works', async () => {
		let fail = true;
		const h = host(OPEN, (c) =>
			c.type === 'browser_take' && fail ? new CommandError('another device has it') : undefined
		);
		h.live.attached();
		expect(await h.live.take(true)).toBe(false);
		expect(h.live.error).toBe('another device has it');
		expect(h.live.working).toBe(false);
		fail = false;
		expect(await h.live.take(true)).toBe(true);
		expect(h.live.error).toBeNull();
	});

	it('drops the picture when the browser closes, and keeps only the state of a state frame', () => {
		const h = host();
		h.live.watch(true);
		h.live.onFrame(FRAME);
		h.live.onState({ type: 'browser_state', at: 12, running: false } as BrowserState);
		expect(h.live.picture).toBeNull();
		expect(h.live.state).toEqual({ running: false });
	});
});

describe('the panel and the visibility of the page', () => {
	it('watches while its tab is shown and the page is visible, and stops otherwise', async () => {
		const h = host();
		h.live.attached();
		const view = render(BrowserPanel, { session: session(h.live), canControl: true, active: true });
		await vi.waitFor(() => expect(h.watches()).toEqual([true]));

		// The phone is locked, or another tab of the device's browser is in front.
		setVisible(false);
		await vi.waitFor(() => expect(h.watches()).toEqual([true, false]));
		setVisible(true);
		await vi.waitFor(() => expect(h.watches()).toEqual([true, false, true]));

		// Another tab of the right pane is chosen.
		await view.rerender({ active: false });
		await vi.waitFor(() => expect(h.watches()).toEqual([true, false, true, false]));
		// Coming back to a hidden page does not start it.
		setVisible(false);
		await view.rerender({ active: true });
		await settle();
		expect(h.watches()).toEqual([true, false, true, false]);
		setVisible(true);
		await vi.waitFor(() => expect(h.watches()).toEqual([true, false, true, false, true]));
	});

	it('never asks for pictures while its tab is not the one shown', async () => {
		const h = host();
		h.live.attached();
		render(BrowserPanel, { session: session(h.live), canControl: true, active: false });
		await settle();
		expect(h.watches()).toEqual([]);
	});

	it('stops watching when it leaves the page', async () => {
		const h = host();
		h.live.attached();
		const view = render(BrowserPanel, { session: session(h.live), canControl: true });
		await vi.waitFor(() => expect(h.watches()).toEqual([true]));
		view.unmount();
		await vi.waitFor(() => expect(h.watches()).toEqual([true, false]));
	});
});

describe('what the panel shows', () => {
	const shown = async (state: BrowserState, canControl: boolean, busy = false) => {
		const h = host(state);
		h.live.attached();
		render(BrowserPanel, { session: session(h.live, busy), canControl });
		await vi.waitFor(() => expect(h.live.state).not.toBeNull());
		await settle();
		return h;
	};
	const status = () => screen.getByRole('status').textContent?.trim();

	it('says that no browser is open, and still offers the sign-in', async () => {
		await shown({ running: false }, true);
		expect(status()).toBe('No browser open');
		expect(screen.getByText('The agent has not opened a browser')).toBeTruthy();
		expect(screen.queryByRole('button', { name: 'Take over' })).toBeNull();
		expect(screen.getByRole('button', { name: 'Sign in' })).toBeTruthy();
	});

	it('shows the title, the address and the picture of the page the agent is on', async () => {
		const h = await shown(OPEN, true, true);
		expect(status()).toBe('Agent working');
		expect(screen.getByRole('heading', { name: 'Example Domain' })).toBeTruthy();
		expect(screen.getByText('https://example.com/')).toBeTruthy();
		// Until the first picture arrives its place is held.
		expect(screen.queryByRole('img')).toBeNull();
		h.live.onFrame(FRAME);
		await vi.waitFor(() => expect(screen.getByRole('img')).toBeTruthy());
		expect((screen.getByRole('img') as HTMLImageElement).alt).toContain('Example Domain');
	});

	it('says watching when the agent is idle', async () => {
		await shown(OPEN, true, false);
		expect(status()).toBe('Watching');
	});

	it('hides taking over and signing in from a device that may only view', async () => {
		await shown(OPEN, false);
		expect(status()).toBe('Watching');
		expect(screen.queryByRole('button', { name: 'Take over' })).toBeNull();
		expect(screen.queryByRole('button', { name: 'Hand back' })).toBeNull();
		expect(screen.queryByRole('button', { name: 'Sign in' })).toBeNull();
		expect(screen.queryByRole('textbox', { name: 'Address' })).toBeNull();
		expect(screen.queryByRole('application')).toBeNull();
	});

	it('hides them from a view device when no browser is open too', async () => {
		await shown({ running: false }, false);
		expect(screen.queryByRole('button')).toBeNull();
	});

	it('tells a view device that may not watch why it sees nothing', async () => {
		const h = host(OPEN, () => new CommandError('A view link cannot watch.', 'refused'));
		h.live.attached();
		render(BrowserPanel, { session: session(h.live), canControl: false });
		await vi.waitFor(() => expect(screen.getByText('Watching needs a control link')).toBeTruthy());
		expect(screen.queryByRole('button')).toBeNull();
		expect(screen.queryByRole('status')).toBeNull();
	});

	it('takes over and hands back, with the address field only while holding', async () => {
		const h = await shown(OPEN, true);
		await fireEvent.click(screen.getByRole('button', { name: 'Take over' }));
		await vi.waitFor(() => expect(status()).toBe('You have control'));
		expect(h.sent).toContainEqual({ type: 'browser_take', on: true });
		expect(screen.queryByRole('button', { name: 'Take over' })).toBeNull();
		const picture = screen.getByRole('application');
		expect(picture.tabIndex).toBe(0);
		await vi.waitFor(() => expect(document.activeElement).toBe(picture));
		expect((screen.getByRole('textbox', { name: 'Address' }) as HTMLInputElement).value).toBe(
			'https://example.com/'
		);

		await fireEvent.click(screen.getByRole('button', { name: 'Hand back' }));
		await vi.waitFor(() => expect(status()).toBe('Watching'));
		expect(h.sent).toContainEqual({ type: 'browser_take', on: false });
		expect(screen.queryByRole('application')).toBeNull();
		expect(screen.queryByRole('textbox', { name: 'Address' })).toBeNull();
	});

	it('cannot take over what another device holds', async () => {
		await shown({ ...OPEN, held: true, held_by_you: false }, true);
		expect(status()).toBe('Held by another device');
		expect((screen.getByRole('button', { name: 'Take over' }) as HTMLButtonElement).disabled).toBe(
			true
		);
	});

	it('sends keys over the picture to the page while holding, and none before', async () => {
		const h = await shown(OPEN, true);
		const inputs = () => h.sent.filter((c) => c.type === 'browser_input').map((c) => c.input);
		await fireEvent.click(screen.getByRole('button', { name: 'Take over' }));
		const picture = await vi.waitFor(() => screen.getByRole('application'));
		await fireEvent.keyDown(picture, { key: 'h' });
		await fireEvent.keyDown(picture, { key: 'i' });
		await fireEvent.keyDown(picture, { key: 'Enter' });
		await vi.waitFor(() =>
			expect(inputs()).toEqual([
				{ kind: 'text', text: 'hi' },
				{ kind: 'key', keys: 'Enter' }
			])
		);
		// Shift+Escape leaves the picture for the button that hands back, and is not sent.
		await fireEvent.keyDown(picture, { key: 'Escape', shiftKey: true });
		expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Hand back' }));
		expect(inputs()).toHaveLength(2);
	});

	it('opens what is typed into the address field, and refuses what is no address', async () => {
		const h = await shown(OPEN, true);
		await fireEvent.click(screen.getByRole('button', { name: 'Take over' }));
		const field = (await vi.waitFor(() =>
			screen.getByRole('textbox', { name: 'Address' })
		)) as HTMLInputElement;
		const form = field.closest('form') as HTMLFormElement;
		await fireEvent.input(field, { target: { value: 'two words' } });
		await fireEvent.submit(form);
		expect(screen.getByRole('alert').textContent).toContain('Enter a web address');
		expect(h.sent.some((c) => c.type === 'browser_input')).toBe(false);

		await fireEvent.input(field, { target: { value: 'example.org/docs' } });
		await fireEvent.submit(form);
		await vi.waitFor(() =>
			expect(h.sent).toContainEqual({
				type: 'browser_input',
				input: { kind: 'navigate', url: 'https://example.org/docs' }
			})
		);
		expect(screen.queryByRole('alert')).toBeNull();
	});

	it('opens the sign-in window on the host and says where it is', async () => {
		const h = await shown(OPEN, true);
		await fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));
		await vi.waitFor(() => expect(toasts.items).toHaveLength(1));
		expect(h.sent).toContainEqual({ type: 'browser_sign_in' });
		expect(toasts.items[0].text).toContain('window on the computer that runs azazel');
	});
});
