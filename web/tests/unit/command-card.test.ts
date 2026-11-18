import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import type { AnyFrame } from '$lib/protocol';
import { addCommandCard, commandCards, emptyModel, reduce } from '$lib/session/reduce';

const load = (name: string): AnyFrame[] =>
	readFileSync(resolve(process.cwd(), 'fixtures', `${name}.jsonl`), 'utf8')
		.split('\n')
		.filter(Boolean)
		.map((l) => JSON.parse(l));

describe('command cards', () => {
	it('puts what a slash command showed into the transcript', () => {
		const empty = emptyModel();
		const m = addCommandCard(empty, 'cost', '', 'Session cost   $0.42');
		expect(empty.items).toHaveLength(0);
		expect(m.items).toHaveLength(1);
		expect(m.items[0]).toMatchObject({
			kind: 'command',
			agent: 'lead',
			name: 'cost',
			args: '',
			text: 'Session cost   $0.42'
		});
	});

	it('keeps the arguments, trimmed, and gives every card its own id', () => {
		let m = addCommandCard(emptyModel(), 'btw', '  what is the cap?  ', 'It is 30 s.');
		m = addCommandCard(m, 'cost', '', 'again');
		const cards = commandCards(m);
		expect(cards.map((c) => c.name)).toEqual(['btw', 'cost']);
		expect(cards[0].args).toBe('what is the cap?');
		expect(typeof cards[0].id).toBe('string');
		expect(cards[0].id).not.toBe(cards[1].id);
	});

	it('sits after the conversation and survives the frames that follow', () => {
		const frames = load('showcase');
		const half = Math.floor(frames.length / 2);
		const before = frames.slice(0, half).reduce(reduce, emptyModel());
		const withCard = addCommandCard(before, 'cost', '', 'shown');
		expect(withCard.items).toHaveLength(before.items.length + 1);
		expect(withCard.items.at(-1)?.kind).toBe('command');
		// Nothing the reducer already held is touched.
		expect(withCard.items.slice(0, -1)).toEqual(before.items);
		expect(commandCards(before)).toEqual([]);

		const after = frames.slice(half).reduce(reduce, withCard);
		expect(commandCards(after)).toHaveLength(1);
		expect(commandCards(after)[0].text).toBe('shown');
		const plain = frames.reduce(reduce, emptyModel());
		expect(after.items.filter((i) => i.kind !== 'command')).toHaveLength(plain.items.length);
	});
});
