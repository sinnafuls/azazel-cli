// How the sidebar arranges sessions, and how its search ranks them. Pure, so it can be tested.
import type { SessionInfo } from '$lib/protocol';
import { basename } from '$lib/format';

export const SIDEBAR_MIN = 220;
export const SIDEBAR_MAX = 480;
export const SIDEBAR_DEFAULT = 260;
export const SIDEBAR_RAIL = 48;

export function clampWidth(w: number): number {
	return Math.round(Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, Number.isFinite(w) ? w : 260)));
}

export interface ProjectGroup {
	/** The folder, as the session reports it: the key the collapsed state is remembered by. */
	cwd: string;
	name: string;
	sessions: SessionInfo[];
}
export interface Arranged {
	/** Waiting for an answer: always on top, never folded away. */
	needs: SessionInfo[];
	running: SessionInfo[];
	/** Everything else, by project folder; the folder touched last comes first. */
	recent: ProjectGroup[];
	total: number;
}

const newest = (a: SessionInfo, b: SessionInfo) => Date.parse(b.started) - Date.parse(a.started);

export function titleOf(s: Pick<SessionInfo, 'name' | 'cwd'>): string {
	return s.name?.trim() || basename(s.cwd) || 'Untitled';
}

/** `~` for the home folder, on Unix and Windows. */
export function homePath(p: string): string {
	return p.replace(/^\/(home|Users)\/[^/]+/, '~').replace(/^[A-Za-z]:\\Users\\[^\\]+/, '~');
}

export function arrange(list: SessionInfo[]): Arranged {
	const needs = list.filter((s) => s.state === 'waiting for you').sort(newest);
	const running = list.filter((s) => s.state === 'working').sort(newest);
	const groups = new Map<string, ProjectGroup>();
	for (const s of list
		.filter((s) => s.state !== 'waiting for you' && s.state !== 'working')
		.sort(newest)) {
		let g = groups.get(s.cwd);
		if (!g) groups.set(s.cwd, (g = { cwd: s.cwd, name: basename(s.cwd) || s.cwd, sessions: [] }));
		g.sessions.push(s);
	}
	return { needs, running, recent: [...groups.values()], total: list.length };
}

/**
 * Sessions that match every word of `query`, best first: a title that starts with the query, then
 * a word of the title that does, then the title anywhere, then the folder. An empty query keeps
 * the list's own order (those that need you first).
 */
export function searchSessions(list: SessionInfo[], query: string): SessionInfo[] {
	const order = (s: SessionInfo) =>
		s.state === 'waiting for you' ? 0 : s.state === 'working' ? 1 : 2;
	const words = query.toLowerCase().split(/\s+/).filter(Boolean);
	if (!words.length) return [...list].sort((a, b) => order(a) - order(b) || newest(a, b));
	const q = words.join(' ');
	const scored: [number, SessionInfo][] = [];
	for (const s of list) {
		const title = titleOf(s).toLowerCase();
		const hay = `${title} ${s.cwd.toLowerCase()} ${s.state} ${s.mode ?? ''}`;
		if (!words.every((w) => hay.includes(w))) continue;
		const score = title.startsWith(q)
			? 0
			: title.split(/[^a-z0-9]+/).some((w) => w.startsWith(words[0]))
				? 1
				: words.every((w) => title.includes(w))
					? 2
					: 3;
		scored.push([score, s]);
	}
	return scored
		.sort((a, b) => a[0] - b[0] || order(a[1]) - order(b[1]) || newest(a[1], b[1]))
		.map(([, s]) => s);
}
