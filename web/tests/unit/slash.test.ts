import { describe, expect, it } from 'vitest';
import {
	filterSlash,
	groupSlash,
	needsArgs,
	parseSlash,
	SESSION_COMMANDS,
	slashEntries,
	slashLabel,
	slashQuery,
	unknownSlash
} from '$lib/slash';

const entries = slashEntries([
	{ name: 'cost', description: 'What this session cost', source: 'builtin' },
	{ name: 'deploy', description: 'Ship it', source: 'custom' },
	// A row the page already has as a session command is not listed twice.
	{ name: 'compact', description: 'duplicate', source: 'builtin' },
	{ name: 'skill:review', description: 'Look over a change', source: 'skill' }
]);
const names = (q: string) => filterSlash(entries, q).map((e) => e.name);

describe('the / menu', () => {
	it('opens on a slash at the very start and closes at the first space', () => {
		expect(slashQuery('/')).toBe('');
		expect(slashQuery('/co')).toBe('co');
		expect(slashQuery('/skill:rev')).toBe('skill:rev');
		expect(slashQuery('/cost ')).toBeNull();
		expect(slashQuery('say /cost')).toBeNull();
		expect(slashQuery(' /cost')).toBeNull();
		expect(slashQuery('')).toBeNull();
	});

	it('lists the session commands, then commands, then skills', () => {
		expect(entries.filter((e) => e.name === 'compact')).toHaveLength(1);
		expect(entries.find((e) => e.name === 'compact')?.builtin).toBe(true);
		const groups = groupSlash(filterSlash(entries, ''));
		expect(groups.map((g) => g.group)).toEqual(['Session', 'Commands', 'Skills']);
		expect(groups[0].items).toHaveLength(SESSION_COMMANDS.length);
		expect(groups.map((g) => g.start)).toEqual([
			0,
			SESSION_COMMANDS.length,
			SESSION_COMMANDS.length + 2
		]);
		expect(groups[1].items.map((e) => e.name)).toEqual(['cost', 'deploy']);
		expect(groups[2].items.map((e) => e.name)).toEqual(['skill:review']);
	});

	it('filters by name as you type', () => {
		expect(names('co')).toEqual(['compact', 'cost']);
		expect(names('COST')).toEqual(['cost']);
		expect(names('zzz')).toEqual([]);
	});
	it('finds a skill without its prefix', () => {
		expect(names('rev')).toEqual(['skill:review']);
		expect(names('skill:')).toEqual(['skill:review']);
	});
	it('puts an exact name before one that only starts with it', () => {
		expect(names('mode').slice(0, 2)).toEqual(['mode', 'model']);
	});
	it('searches descriptions only for a real word', () => {
		expect(names('summ')).toEqual(['compact']);
		expect(names('su')).toEqual([]);
	});

	it('reads a typed line as a known command with its arguments', () => {
		expect(parseSlash('/name  My title ', entries)).toMatchObject({
			entry: { name: 'name', builtin: true },
			args: 'My title'
		});
		expect(parseSlash('/cost', entries)).toMatchObject({ entry: { name: 'cost' }, args: '' });
		expect(parseSlash('/review the diff', entries)).toMatchObject({
			entry: { name: 'skill:review', group: 'Skills' },
			args: 'the diff'
		});
	});
	it('leaves an unknown /word and ordinary text alone', () => {
		expect(parseSlash('/nonsense now', entries)).toBeNull();
		expect(parseSlash('not a command', entries)).toBeNull();
		expect(parseSlash('/', entries)).toBeNull();
	});

	it('knows which commands cannot run without an argument', () => {
		const by = (n: string) => entries.find((e) => e.name === n)!;
		expect(needsArgs(by('name'))).toBe(true);
		expect(needsArgs(by('btw'))).toBe(true);
		expect(needsArgs(by('compact'))).toBe(false);
		expect(needsArgs(by('cost'))).toBe(false);
		expect(slashLabel(by('skill:review'))).toBe('/review');
		expect(slashLabel(by('cost'))).toBe('/cost');
	});
});

describe('a lone /word that is no command', () => {
	it('is named, so the page can refuse to send it', () => {
		expect(unknownSlash('/trust', entries)).toBe('trust');
		expect(unknownSlash('  /login ', entries)).toBe('login');
		expect(unknownSlash('/cost', entries)).toBeNull();
		expect(unknownSlash('/review', entries)).toBeNull();
		// Anything longer than one word is an ordinary message.
		expect(unknownSlash('/trust this folder', entries)).toBeNull();
		expect(unknownSlash('look at /trust', entries)).toBeNull();
		expect(unknownSlash('/usr/bin/env', entries)).toBeNull();
	});
});
