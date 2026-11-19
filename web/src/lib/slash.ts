// The `/` menu of the message box: what it lists, how typing filters it, and how a typed line is
// read as a command. Pure, so it can be tested without a page.
import type { CommandRow } from '$lib/protocol';

export type SlashGroup = 'Session' | 'Commands' | 'Skills';
export const SLASH_GROUPS: SlashGroup[] = ['Session', 'Commands', 'Skills'];

export interface SlashEntry {
	/** What `run_command` takes (`cost`, `skill:review`), or the name of a session command. */
	name: string;
	group: SlashGroup;
	description: string;
	/** The arguments, as a hint: `<title>` must be given, `[instructions]` may be. */
	args?: string;
	/** A session command: the page sends its own RPC command instead of `run_command`. */
	builtin: boolean;
}

const SESSION_ROWS: [string, string, string?][] = [
	['compact', 'Summarise the conversation to free context', '[instructions]'],
	['retry', 'Send the last prompt again'],
	['name', 'Name this session', '<title>'],
	['fork', 'Continue in a copy of this session'],
	['handoff', 'Continue in a new session that starts from a written brief', '[focus]'],
	['new', 'Leave this conversation and start a new one'],
	['resume', 'Continue an earlier session'],
	['export', 'Write the session to a file', '[markdown|html|jsonl]'],
	['btw', 'Ask on the side, without interrupting', '<question>'],
	['model', 'Choose the model', '[provider/model]'],
	['mode', 'Choose the permission mode', '[mode]'],
	['thinking', 'Choose how hard the model thinks', '[level]'],
	['rewind', 'Go back to an earlier prompt']
];

/** The commands the page runs itself. */
export const SESSION_COMMANDS: SlashEntry[] = SESSION_ROWS.map(([name, description, args]) => ({
	name,
	description,
	args,
	group: 'Session',
	builtin: true
}));

const isSkill = (r: CommandRow) => r.name.startsWith('skill:');

/** The whole menu: the session commands, then what `list_commands` offers. */
export function slashEntries(rows: CommandRow[]): SlashEntry[] {
	const taken = new Set(SESSION_COMMANDS.map((c) => c.name));
	const out = [...SESSION_COMMANDS];
	for (const r of rows) {
		if (!r?.name || taken.has(r.name)) continue;
		taken.add(r.name);
		out.push({
			name: r.name,
			group: isSkill(r) ? 'Skills' : 'Commands',
			description: r.description ?? '',
			builtin: false
		});
	}
	return out;
}

/**
 * The part of a command name typed so far, or null when the menu is closed. The menu opens on a
 * `/` at the very start and stays open while only a name follows it: a space ends the name.
 */
export function slashQuery(text: string): string | null {
	return /^\/([\w:.-]*)$/.exec(text)?.[1] ?? null;
}

function rank(e: SlashEntry, q: string): number {
	const name = e.name.toLowerCase();
	const bare = name.replace(/^skill:/, '');
	if (name === q || bare === q) return 0;
	if (name.startsWith(q) || bare.startsWith(q)) return 1;
	if (bare.split(/[-_:.]/).some((w) => w.startsWith(q))) return 2;
	if (name.includes(q)) return 3;
	// A description only counts for a real word: two letters are found inside almost every sentence.
	if (
		q.length >= 3 &&
		e.description
			.toLowerCase()
			.split(/[^a-z0-9]+/)
			.some((w) => w.startsWith(q))
	)
		return 4;
	return -1;
}

/** The entries that match `query`, in the order of the groups, the closest match first in each. */
export function filterSlash(entries: SlashEntry[], query: string): SlashEntry[] {
	const q = query.trim().toLowerCase();
	return entries
		.map((e, i) => ({ e, i, r: q ? rank(e, q) : 0 }))
		.filter((x) => x.r >= 0)
		.sort(
			(a, b) =>
				SLASH_GROUPS.indexOf(a.e.group) - SLASH_GROUPS.indexOf(b.e.group) || a.r - b.r || a.i - b.i
		)
		.map((x) => x.e);
}

/** Splits a filtered list into its groups, each with the index of its first row in the list. */
export function groupSlash(
	entries: SlashEntry[]
): { group: SlashGroup; start: number; items: SlashEntry[] }[] {
	const out: { group: SlashGroup; start: number; items: SlashEntry[] }[] = [];
	entries.forEach((e, i) => {
		const last = out[out.length - 1];
		if (last?.group === e.group) last.items.push(e);
		else out.push({ group: e.group, start: i, items: [e] });
	});
	return out;
}

/** `/name args` as a known command, or null: an unknown `/word` is an ordinary message. */
export function parseSlash(
	text: string,
	entries: SlashEntry[]
): { entry: SlashEntry; args: string } | null {
	const m = /^\/([\w:.-]+)(?:\s+([\s\S]*))?$/.exec(text.trim());
	if (!m) return null;
	const entry = entries.find(
		(e) => e.name === m[1] || (e.group === 'Skills' && e.name === `skill:${m[1]}`)
	);
	return entry ? { entry, args: (m[2] ?? '').trim() } : null;
}

/** Whether the command cannot run without an argument (`<title>`). */
export const needsArgs = (e: SlashEntry) => !!e.args?.startsWith('<');

/** How a row is shown: a skill without its `skill:` prefix. */
export const slashLabel = (e: SlashEntry) => `/${e.name.replace(/^skill:/, '')}`;

/**
 * The name of a lone `/word` that is no command this page can run, or null. Such a line is not sent:
 * the terminal has commands (`/trust`, `/login`) that a browser may run only with full access, and
 * the model should not be asked to guess at them.
 */
export function unknownSlash(text: string, entries: SlashEntry[]): string | null {
	const m = /^\/([\w:.-]+)$/.exec(text.trim());
	return m && !parseSlash(text, entries) ? m[1] : null;
}
