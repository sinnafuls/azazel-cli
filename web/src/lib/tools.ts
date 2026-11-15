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
