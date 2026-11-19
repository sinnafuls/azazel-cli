// The HTTP half of the remote API (docs/remote.md): pairing, who am I, the session list, links,
// starting a session in a folder, and going on with an earlier one.
import type { Folders, Me, NewSession, SavedSession, SessionInfo } from '$lib/protocol';

/** A row of `GET /api/devices`: a short label, never a token. */
export interface DeviceRow {
	id: string;
	access: 'view' | 'control';
	paired: string;
	expires: string;
	current: boolean;
}

export class ApiError extends Error {
	constructor(
		readonly status: number,
		message: string
	) {
		super(message);
	}
}

async function call<T>(path: string, init: RequestInit = {}): Promise<T> {
	const headers: Record<string, string> = { Accept: 'application/json' };
	if (init.method && init.method !== 'GET') {
		headers['X-Azazel'] = '1';
		if (init.body) headers['Content-Type'] = 'application/json';
	}
	let res: Response;
	try {
		res = await fetch(path, { credentials: 'same-origin', cache: 'no-store', ...init, headers });
	} catch {
		throw new ApiError(0, 'Cannot reach the azazel server. Is it still running?');
	}
	if (!res.ok) {
		let message = `${res.status} ${res.statusText}`;
		try {
			const body = (await res.json()) as { error?: string };
			if (body.error) message = body.error;
		} catch {
			/* no body */
		}
		throw new ApiError(res.status, message);
	}
	if (res.status === 204) return undefined as T;
	return (await res.json()) as T;
}

export const api = {
	me: () => call<Me>('/api/me'),
	/** The paired devices (a control device only; an older server answers 404). */
	devices: async () => (await call<{ devices: DeviceRow[] }>('/api/devices')).devices,
	/** Signs every device out, this one included. */
	signOutAll: () => call<void>('/api/devices', { method: 'DELETE' }),
	sessions: async () => (await call<{ sessions: SessionInfo[] }>('/api/sessions')).sessions,
	pair: (secret: string) =>
		call<Me>('/api/pair', { method: 'POST', body: JSON.stringify({ secret }) }),
	link: (access: 'view' | 'control') =>
		call<{ url: string; access: string; expires: string }>('/api/links', {
			method: 'POST',
			body: JSON.stringify({ access })
		}),
	logout: () => call<void>('/api/logout', { method: 'POST', body: '{}' }),
	/** The folders a session may start in, or the folders inside `under`. */
	folders: (under?: string) =>
		call<Folders>('/api/folders', { method: 'POST', body: JSON.stringify(under ? { under } : {}) }),
	/** Starts a background session; it may wait while the computer is asked to allow it. */
	/** Earlier sessions a browser may go on with, the most recent first; `query` narrows them. */
	history: async (query?: string) =>
		(
			await call<{ sessions: SavedSession[] }>('/api/history', {
				method: 'POST',
				body: JSON.stringify(query?.trim() ? { query: query.trim() } : {})
			})
		).sessions,
	start: async (body: NewSession) =>
		(await call<{ id: string }>('/api/sessions', { method: 'POST', body: JSON.stringify(body) })).id
};

/** The secret in `#pair=<secret>`, or null. */
export function pairSecret(hash: string): string | null {
	const m = /^#?pair=([A-Za-z0-9_-]+)$/.exec(hash);
	return m ? m[1] : null;
}

export function wsUrl(id: string, loc: Pick<Location, 'protocol' | 'host'> = location): string {
	return `${loc.protocol === 'https:' ? 'wss:' : 'ws:'}//${loc.host}/api/ws/${encodeURIComponent(id)}`;
}
