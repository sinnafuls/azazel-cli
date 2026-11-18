import { describe, expect, it } from 'vitest';
import { entryFor, type TreeEntry } from '$lib/session/tree';
import type { AssistantItem, Item, UserItem } from '$lib/session/reduce';

const user = (id: string, text: string): UserItem => ({
	kind: 'user',
	id,
	agent: 'lead',
	text,
	via: 'prompt',
	images: 0
});
const reply = (id: string, text: string, agent = 'lead'): AssistantItem => ({
	kind: 'assistant',
	id,
	agent,
	blocks: [{ index: 0, type: 'text', text }],
	streaming: false
});
const entry = (
	id: string,
	role: string,
	text: string,
	extra: Partial<TreeEntry> = {}
): TreeEntry => ({
	id,
	kind: 'message',
	role,
	text,
	...extra
});

const TREE: TreeEntry[] = [
	entry('e1', 'user', 'Add a retry to the fetch'),
	entry('e2', 'assistant', 'I added a retry with backoff.'),
	entry('e3', 'user', 'Now test it'),
	entry('e4', 'assistant', 'The tests pass.')
];

describe('entryFor', () => {
	it('uses the entry a history row names', () => {
		const items: Item[] = [user('he3', 'anything')];
		expect(entryFor(TREE, items, items[0])).toBe('e3');
	});
	it('matches live messages to entries by their place counted from the end and their text', () => {
		const items: Item[] = [
			user('u1', 'Add a retry to the fetch'),
			reply('a1', 'I added a retry with backoff.'),
			user('u2', 'Now test it'),
			reply('a2', 'The tests pass.')
		];
		expect(items.map((i) => entryFor(TREE, items, i))).toEqual(['e1', 'e2', 'e3', 'e4']);
	});
	it('compares a shortened entry text with the full text, ignoring case and spacing', () => {
		const tree = [entry('e1', 'user', 'add a  retry to the…')];
		const items: Item[] = [user('u1', 'Add a retry to the fetch\nand log it')];
		expect(entryFor(tree, items, items[0])).toBe('e1');
	});
	it('finds a reply by its text when its place is off, but gives up on a reply that is not there', () => {
		// Two model turns became one message on screen: the place from the end is wrong.
		const tree = [...TREE, entry('e5', 'assistant', 'Done.')];
		const items: Item[] = [reply('a1', 'The tests pass.')];
		expect(entryFor(tree, items, items[0])).toBe('e4');
		const unknown: Item[] = [reply('a2', 'Something never stored')];
		expect(entryFor(TREE, unknown, unknown[0])).toBeNull();
	});
	it('trusts the place of a prompt whose text differs', () => {
		const items: Item[] = [user('u1', 'Add a retry to the fetch'), user('u2', 'edited text')];
		expect(entryFor(TREE, items, items[1])).toBe('e3');
	});
	it('ignores entries off the current branch, other agents and other kinds of item', () => {
		const tree = [
			entry('x1', 'user', 'Now test it', { current: false }),
			entry('e3', 'user', 'Now test it')
		];
		const items: Item[] = [user('u1', 'Now test it')];
		expect(entryFor(tree, items, items[0])).toBe('e3');
		const notice: Item = { kind: 'notice', id: 'n', agent: 'lead', level: 'info', text: 'hi' };
		expect(entryFor(TREE, [notice], notice)).toBeNull();
		expect(entryFor([], items, items[0])).toBeNull();
	});
});
