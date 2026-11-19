// How the resume picker shows an earlier session. Pure, so it can be tested.
import type { SavedSession } from '$lib/protocol';
import { basename } from '$lib/format';

/** Its name, else the first thing asked in it (on one line), else its folder. */
export function savedTitle(s: Pick<SavedSession, 'name' | 'first_prompt' | 'cwd'>): string {
	const name = s.name?.trim();
	if (name) return name;
	const first = s.first_prompt?.replace(/\s+/g, ' ').trim();
	if (first) return first.length > 80 ? `${first.slice(0, 79)}…` : first;
	return basename(s.cwd) || 'Untitled';
}

/** `12 messages`, `1 message`. */
export function messageCount(n: number): string {
	return `${n} message${n === 1 ? '' : 's'}`;
}

/** The sessions of `cwd` first (the project on screen), each part keeping the server's order. */
export function projectFirst<T extends Pick<SavedSession, 'cwd'>>(
	list: T[],
	cwd: string | null
): T[] {
	if (!cwd) return list;
	return [...list.filter((s) => s.cwd === cwd), ...list.filter((s) => s.cwd !== cwd)];
}
