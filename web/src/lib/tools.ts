// Display helpers for tool calls: a one-line summary, an icon name, and a diff for edits.
const str = (v: unknown): string | undefined => (typeof v === 'string' && v ? v : undefined);

export function summarize(name: string, input: unknown): string {
	const i = (input ?? {}) as Record<string, unknown>;
	switch (name) {
		case 'read':
		case 'write':
		case 'edit':
		case 'ls':
			return str(i.path) ?? '';
		case 'shell':
			return str(i.command)?.split('\n')[0] ?? '';
		case 'grep':
		case 'glob':
			return [str(i.pattern), str(i.path)].filter(Boolean).join('  in  ');
		case 'task': {
			const a = Array.isArray(i.agents) ? i.agents.length : 0;
			return a ? `${a} agent${a > 1 ? 's' : ''}` : '';
		}
		case 'web_fetch':
			return str(i.url) ?? '';
		case 'web_search':
			return str(i.query) ?? '';
		default: {
			for (const v of Object.values(i)) if (typeof v === 'string' && v) return v.split('\n')[0];
			return '';
		}
	}
}

/** A unified-diff-shaped preview of what a `write` or `edit` is about to do. */
export function previewDiff(name: string, input: unknown): string | undefined {
	const i = (input ?? {}) as Record<string, unknown>;
	if (name === 'write' && typeof i.content === 'string') {
		const lines = i.content.replace(/\n$/, '').split('\n');
		return [
			`--- /dev/null`,
			`+++ b/${String(i.path ?? '')}`,
			`@@ -0,0 +1,${lines.length} @@`,
			...lines.map((l) => `+${l}`)
		].join('\n');
	}
	if (name === 'edit' && Array.isArray(i.edits)) {
		const out = [`--- a/${String(i.path ?? '')}`, `+++ b/${String(i.path ?? '')}`];
		for (const e of i.edits as { old?: string; new?: string }[]) {
			out.push('@@ @@');
			for (const l of String(e.old ?? '')
				.replace(/\n$/, '')
				.split('\n'))
				out.push(`-${l}`);
			for (const l of String(e.new ?? '')
				.replace(/\n$/, '')
				.split('\n'))
				out.push(`+${l}`);
		}
		return out.join('\n');
	}
	return undefined;
}

export function prettyInput(input: unknown, max = 1600): string {
	const s = typeof input === 'string' ? input : JSON.stringify(input, null, 2);
	return s.length > max ? `${s.slice(0, max)}\n… ${s.length - max} more characters` : s;
}

export type DiffLine = { kind: 'add' | 'del' | 'ctx' | 'hunk' | 'meta'; text: string };
export function parseDiff(diff: string): DiffLine[] {
	return diff
		.replace(/\n$/, '')
		.split('\n')
		.map((text): DiffLine => {
			if (text.startsWith('+++') || text.startsWith('---')) return { kind: 'meta', text };
			if (text.startsWith('@@')) return { kind: 'hunk', text };
			if (text.startsWith('+')) return { kind: 'add', text: text.slice(1) };
			if (text.startsWith('-')) return { kind: 'del', text: text.slice(1) };
			return { kind: 'ctx', text: text.startsWith(' ') ? text.slice(1) : text };
		});
}

/** A line of a diff with the number it has in its file, when the diff says where its hunks start. */
export type DiffRow = DiffLine & { n?: number };

/**
 * Numbers the lines of a diff: added and unchanged lines by their place in the new file, removed
 * lines by their place in the old one. A hunk without a position (`@@ @@`, the preview of an edit
 * that has not run) leaves its lines unnumbered.
 */
export function numberDiff(lines: DiffLine[]): DiffRow[] {
	let before = NaN;
	let after = NaN;
	const num = (n: number) => (Number.isNaN(n) ? undefined : n);
	return lines.map((l): DiffRow => {
		switch (l.kind) {
			case 'meta':
				return l;
			case 'hunk': {
				const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)/.exec(l.text);
				before = m ? Number(m[1]) : NaN;
				after = m ? Number(m[2]) : NaN;
				return l;
			}
			case 'del':
				return { ...l, n: num(before++) };
			case 'add':
				return { ...l, n: num(after++) };
			default:
				before++;
				return { ...l, n: num(after++) };
		}
	});
}

/** What kind of work a tool does, for the words and the icon of its row. */
export type ToolKind =
	'command' | 'read' | 'create' | 'edit' | 'search' | 'web' | 'agents' | 'other';

export function toolKind(name: string): ToolKind {
	switch (name) {
		case 'shell':
			return 'command';
		case 'read':
		case 'ls':
			return 'read';
		case 'write':
			return 'create';
		case 'edit':
			return 'edit';
		case 'grep':
		case 'glob':
			return 'search';
		case 'task':
			return 'agents';
		default:
			return name.startsWith('web') ? 'web' : 'other';
	}
}

const PHRASES: Record<ToolKind, [one: string, many: (n: number) => string]> = {
	command: ['ran a command', (n) => `ran ${n} commands`],
	read: ['read a file', (n) => `read ${n} files`],
	create: ['created a file', (n) => `created ${n} files`],
	edit: ['edited a file', (n) => `edited ${n} files`],
	search: ['searched the code', (n) => `searched the code ${n} times`],
	web: ['looked something up', (n) => `looked ${n} things up`],
	agents: ['handed work to agents', (n) => `handed work to agents ${n} times`],
	other: ['used a tool', (n) => `used ${n} tools`]
};

/** A run of tool calls in a few words: `Created a file, ran 2 commands`. */
export function describeSteps(names: string[]): string {
	const counts = new Map<ToolKind, number>();
	for (const n of names) counts.set(toolKind(n), (counts.get(toolKind(n)) ?? 0) + 1);
	const text = [...counts]
		.map(([kind, n]) => (n === 1 ? PHRASES[kind][0] : PHRASES[kind][1](n)))
		.join(', ');
	return text.charAt(0).toUpperCase() + text.slice(1);
}

/** One call as a row says it: the verb, and what it worked on. */
export function stepVerb(name: string, running: boolean): string {
	const kind = toolKind(name);
	const verbs: Record<ToolKind, [done: string, now: string]> = {
		command: ['Ran', 'Running'],
		read: ['Read', 'Reading'],
		create: ['Created', 'Creating'],
		edit: ['Edited', 'Editing'],
		search: ['Searched', 'Searching'],
		web: ['Looked up', 'Looking up'],
		agents: ['Handed off', 'Handing off'],
		other: [name, name]
	};
	return verbs[kind][running ? 1 : 0];
}

/** The size of a change: lines added and taken away. */
export function diffStats(diff: string | undefined): { add: number; del: number } | null {
	if (!diff) return null;
	const ls = parseDiff(diff);
	return {
		add: ls.filter((l) => l.kind === 'add').length,
		del: ls.filter((l) => l.kind === 'del').length
	};
}

/** The name on a call's card: what ran it (`Bash`), or the file it worked on. */
export function toolLabel(name: string, input: unknown): { text: string; path: boolean } {
	const i = (input ?? {}) as Record<string, unknown>;
	const path = str(i.path);
	switch (toolKind(name)) {
		case 'command':
			return { text: 'Bash', path: false };
		case 'read':
		case 'create':
		case 'edit':
			return path ? { text: path, path: true } : { text: name, path: false };
		case 'search':
			return { text: 'Search', path: false };
		case 'web':
			return { text: 'Web', path: false };
		case 'agents':
			return { text: 'Agents', path: false };
		default:
			return { text: name, path: false };
	}
}
