// Finding the entry of the session file that a message of the transcript stands for. History rows
// carry their entry id; what arrived live does not, so it is looked up in `get_tree`.
import type { Item } from './reduce';

/** A row of `get_tree`. */
export interface TreeEntry {
	id: string;
	parent_id?: string | null;
	timestamp?: string;
	kind: string;
	role?: string | null;
	text?: string | null;
	target_id?: string | null;
	current?: boolean;
}

const norm = (t: string | null | undefined) => (t ?? '').replace(/\s+/g, ' ').trim().toLowerCase();
const textOf = (i: Item): string =>
	i.kind === 'user'
		? i.text
		: i.kind === 'assistant'
			? i.blocks
					.filter((b) => b.type === 'text')
					.map((b) => b.text)
					.join(' ')
			: '';
/** Whether a tree entry's (possibly shortened) text and a message's text are the same message. */
function sameText(entry: string | null | undefined, item: string): boolean {
	const a = norm(entry).replace(/(…|\.\.\.)$/, '');
	const b = norm(item);
	if (!a || !b) return false;
	const n = Math.min(a.length, b.length, 60);
	return a.slice(0, n) === b.slice(0, n);
}

/**
 * The entry id of `item` (a prompt or a reply of the lead) on the current branch, or null.
 * A history item names its entry; a live one is matched by its place among the messages of its
 * kind, counted from the end, and by its text.
 */
export function entryFor(tree: TreeEntry[], items: Item[], item: Item): string | null {
	if (item.kind !== 'user' && item.kind !== 'assistant') return null;
	const stored = /^[hp](.+)$/.exec(item.id)?.[1];
	if (stored && tree.some((e) => e.id === stored)) return stored;

	const role = item.kind;
	const entries = tree.filter(
		(e) => e.current !== false && e.role === role && (e.kind === 'message' || !e.kind)
	);
	if (!entries.length) return null;
	const same = items.filter((i) => i.kind === item.kind && i.agent === item.agent);
	const fromEnd = same.length - 1 - same.indexOf(item);
	const text = textOf(item);
	const at = entries[entries.length - 1 - fromEnd];
	if (at && (sameText(at.text, text) || (role === 'user' && !at.text))) return at.id;
	for (let i = entries.length - 1; i >= 0; i--)
		if (sameText(entries[i].text, text)) return entries[i].id;
	// A prompt's place is reliable (one entry per prompt); a reply's is not (one per model turn).
	return role === 'user' && at ? at.id : null;
}
