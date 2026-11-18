// Commands that are about the computer rather than one conversation (settings, roles, usage): sent
// through the session on screen, or through any running session when none is open.
import { app } from '$lib/app.svelte';
import type { Command } from '$lib/protocol';
import { CommandError } from '$lib/rpc/client';
import { oneShot } from '$lib/rpc/oneshot';
import { sessions } from '$lib/sessions.svelte';
import { ui } from '$lib/ui.svelte';

/** There is no session to ask. */
export class NoSession extends Error {
	constructor() {
		super('Start a session first: settings are read through a running session.');
	}
}

/** The computer refuses this device the command; the pane says so quietly, with no detail. */
export class Refused extends Error {
	constructor() {
		super('This device may not do that.');
	}
}

/** The server marks what it refuses a device for its access with this `code` (`policy.rs` in azazel-remote). */
export const REFUSED = 'refused';
const isRefusal = (e: unknown) => e instanceof CommandError && e.code === REFUSED;

// Commands already refused to this device: asking again would only be refused again.
const refused = new Set<string>();
let refusedFor = '';

export function hostRpc<T = unknown>(c: Command): Promise<T> {
	const who = `${app.me?.access}:${app.me?.full_access}`;
	if (who !== refusedFor) {
		refused.clear();
		refusedFor = who;
	}
	if (refused.has(c.type)) return Promise.reject(new Refused());
	const ask = (): Promise<T> => {
		const s = ui.session;
		if (s && s.status === 'open') return s.send<T>(c);
		const any = sessions.list[0];
		if (!any) return Promise.reject(new NoSession());
		return oneShot<T>(any.id, c);
	};
	return ask().catch((e: unknown) => {
		if (!isRefusal(e)) throw e;
		// Only a read is remembered: a write that was refused may be tried again after a change of access.
		if (/^(get|list)_/.test(c.type)) refused.add(c.type);
		throw new Refused();
	});
}
