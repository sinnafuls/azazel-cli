import { describe, expect, it } from 'vitest';
import { messageCount, projectFirst, savedTitle } from '$lib/history';
import { filterSlash, parseSlash, slashEntries } from '$lib/slash';

describe('the resume picker', () => {
	it('names a session by its name, else its first prompt on one line, else its folder', () => {
		expect(savedTitle({ name: 'Parser', first_prompt: 'write it', cwd: '/w/app' })).toBe('Parser');
		expect(savedTitle({ name: ' ', first_prompt: 'write\n  the parser', cwd: '/w/app' })).toBe(
			'write the parser'
		);
		expect(savedTitle({ name: null, first_prompt: null, cwd: '/w/app' })).toBe('app');
		const long = savedTitle({ name: null, first_prompt: 'x'.repeat(200), cwd: '/w' });
		expect(long).toHaveLength(80);
		expect(long.endsWith('…')).toBe(true);
	});

	it('counts messages in words', () => {
		expect(messageCount(1)).toBe('1 message');
		expect(messageCount(12)).toBe('12 messages');
	});

	it('puts the sessions of the project on screen first and keeps the order otherwise', () => {
		const list = [
			{ id: 'a', cwd: '/x' },
			{ id: 'b', cwd: '/here' },
			{ id: 'c', cwd: '/y' },
			{ id: 'd', cwd: '/here' }
		];
		expect(projectFirst(list, '/here').map((s) => s.id)).toEqual(['b', 'd', 'a', 'c']);
		expect(projectFirst(list, null).map((s) => s.id)).toEqual(['a', 'b', 'c', 'd']);
	});

	it('is reached with /resume, which the page runs itself', () => {
		const entries = slashEntries([]);
		expect(filterSlash(entries, 'res').map((e) => e.name)).toContain('resume');
		expect(parseSlash('/resume', entries)?.entry.builtin).toBe(true);
	});
});
