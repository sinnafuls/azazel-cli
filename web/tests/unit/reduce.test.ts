import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import type { AnyFrame } from '$lib/protocol';
import {
	addLocalUser,
	addQueued,
	applySnapshot,
	emptyModel,
	leadBusy,
	reduce,
	type AssistantItem,
	treeOrder,
	type ToolItem
} from '$lib/session/reduce';

const load = (name: string): AnyFrame[] =>
	readFileSync(resolve(process.cwd(), 'fixtures', `${name}.jsonl`), 'utf8')
		.split('\n')
		.filter(Boolean)
		.map((l) => JSON.parse(l));
const run = (frames: AnyFrame[]) => frames.reduce(reduce, emptyModel());

describe('frame reducer over recorded runs', () => {
	it('builds a tool card with its diff from a write that needed approval', () => {
		const frames = load('write_read');
		const beforeAnswer = run(
			frames.slice(
				0,
				frames.findIndex((f) => f.type === 'tool_end')
			)
		);
		expect(beforeAnswer.requests).toHaveLength(1);
		expect(beforeAnswer.requests[0].kind).toBe('approval');

		const m = run(frames);
		const write = m.items.find((i): i is ToolItem => i.kind === 'tool' && i.name === 'write');
		expect(write?.status).toBe('done');
		expect(write?.diff).toContain('+hello from azazel');
		expect(leadBusy(m)).toBe(false);
		const last = m.items.filter((i): i is AssistantItem => i.kind === 'assistant').at(-1);
		expect(last?.streaming).toBe(false);
	});

	it('keeps thinking, text and a markdown reply apart, and shows live shell output', () => {
		const m = run(load('showcase'));
		const assistants = m.items.filter((i): i is AssistantItem => i.kind === 'assistant');
		expect(assistants[0].blocks.map((b) => b.type)).toEqual(['thinking', 'text']);
		expect(assistants.at(-1)?.blocks[0].text).toContain('## What changed');
		const shell = m.items.find((i): i is ToolItem => i.kind === 'tool' && i.name === 'shell');
		expect(shell?.live).toContain('2 passed');
		expect(shell?.status).toBe('done');
		expect(m.toasts).toEqual([]);
	});

	it('follows a roster of sub-agents to their final states and keeps their frames apart', () => {
		const m = run(load('team_goal'));
		expect(m.roster.map((r) => [r.id, r.state])).toContainEqual(['scout-1', 'done']);
		expect(new Set(m.items.map((i) => i.agent)).has('scout-1')).toBe(true);
		expect(m.items.filter((i) => i.agent === 'lead').every((i) => i.agent === 'lead')).toBe(true);
	});

	it('turns notify requests into toasts and every other ui_request into an open request', () => {
		const m = run(load('forms'));
		expect(m.requests.map((r) => r.kind)).toEqual(['ui', 'ui']);
		expect(m.toasts).toHaveLength(1);
		const closed = reduce(m, { type: 'request_closed', request_id: m.requests[0].id });
		expect(closed.requests).toHaveLength(m.requests.length - 1);
	});

	it('ignores a request it already has and frames it does not know', () => {
		const frames = load('write_read');
		const req = frames.find((f) => f.type === 'approval_request')!;
		const m = reduce(reduce(emptyModel(), req), req);
		expect(m.requests).toHaveLength(1);
		const same = reduce(m, { type: 'from_the_future' } as unknown as AnyFrame);
		expect(same.items).toBe(m.items);
	});

	it('merges a running snapshot tool with its history card and finishes that same card', () => {
		let m = applySnapshot(emptyModel(), {
			busy: true,
			tail: [
				{
					entry_id: 'entry-1',
					timestamp: '',
					message: {
						role: 'assistant',
						content: [{ type: 'tool_call', id: 'c1', name: 'write', input: { path: 'hello.txt' } }]
					}
				}
			],
			tools: [
				{
					agent: 'lead',
					call_id: 'c1',
					name: 'write',
					input: { path: 'hello.txt' },
					elapsed_ms: 20
				}
			]
		} as never);
		expect(m.items.filter((i) => i.kind === 'tool')).toHaveLength(1);
		m = reduce(m, {
			type: 'tool_end',
			agent: 'lead',
			call_id: 'c1',
			output: {
				content: [{ type: 'text', text: 'created' }],
				is_error: false
			}
		} as AnyFrame);
		const tools = m.items.filter((i): i is ToolItem => i.kind === 'tool');
		expect(tools).toHaveLength(1);
		expect(tools[0]).toMatchObject({ status: 'done', output: 'created' });
	});

	it('keeps thumbnails of the pictures this page sent', () => {
		const m = addLocalUser(emptyModel(), 'look', 'prompt', 1, 'lead', ['data:image/png;base64,AA']);
		expect(m.items[0]).toMatchObject({
			kind: 'user',
			images: 1,
			previews: ['data:image/png;base64,AA']
		});
	});

	it('remembers the position of the last frame for resuming', () => {
		let m = reduce(emptyModel(), { type: 'agent_start', agent: 'lead', at: 10 });
		m = reduce(m, { type: 'idle', agent: 'lead', stop: 'stop', at: 42 });
		m = reduce(m, { type: 'roster', members: [], at: 7 });
		expect(m.lastAt).toBe(42);
	});

	it('shows the last run failure when joining after the run ended', () => {
		const m = applySnapshot(emptyModel(), {
			busy: false,
			tail: [],
			last_error: 'Usage limit reached'
		} as never);
		expect(m.items).toContainEqual(
			expect.objectContaining({
				kind: 'notice',
				level: 'error',
				text: 'Usage limit reached'
			})
		);
		// A resync replaces the transcript instead of adding the error a second time.
		const again = applySnapshot(m, { busy: false, last_error: 'Usage limit reached' } as never);
		expect(again.items.filter((i) => i.kind === 'notice')).toHaveLength(1);
	});

	it('rebuilds a half-written reply and running tools from an attach snapshot', () => {
		const m = applySnapshot(emptyModel(), {
			busy: true,
			tail: [
				{
					entry_id: 'e1',
					timestamp: '',
					message: { role: 'user', content: [{ type: 'text', text: 'go' }] }
				},
				{
					entry_id: 'e2',
					timestamp: '',
					message: {
						role: 'assistant',
						content: [
							{ type: 'text', text: 'On it.' },
							{ type: 'tool_call', id: 'c1', name: 'read', input: { path: 'a' } }
						]
					}
				},
				{
					entry_id: 'e3',
					timestamp: '',
					message: {
						role: 'tool',
						content: [
							{ type: 'tool_result', call_id: 'c1', content: [{ type: 'text', text: 'file' }] }
						]
					}
				}
			] as never,
			streaming: [{ agent: 'lead', text: 'Half a sen' }],
			tools: [
				{ agent: 'lead', call_id: 'c9', name: 'shell', input: { command: 'ls' }, elapsed_ms: 1200 }
			],
			queue: [{ kind: 'steer', text: 'wait' }]
		});
		expect(leadBusy(m)).toBe(true);
		expect(m.items.map((i) => i.kind)).toEqual(['user', 'assistant', 'tool', 'assistant', 'tool']);
		const read = m.items.find((i): i is ToolItem => i.kind === 'tool' && i.callId === 'c1');
		expect(read).toMatchObject({ status: 'done', output: 'file' });
		// A delta that arrives next lands in the reply the snapshot opened.
		const next = reduce(m, {
			type: 'stream',
			agent: 'lead',
			event: { type: 'text_delta', index: 1, delta: 'tence' }
		});
		const open = next.items.filter((i): i is AssistantItem => i.kind === 'assistant').at(-1);
		expect(open?.blocks[0].text).toBe('Half a sentence');
	});

	it('shows a prompt without the memories recalled for the model, live and after a reload', () => {
		const sent = 'check the cache\n\n<memory>\n- [fact] usage is logged\n</memory>';
		const live = reduce(emptyModel(), {
			type: 'user_input',
			kind: 'prompt',
			text: sent
		} as unknown as AnyFrame);
		const reloaded = applySnapshot(emptyModel(), {
			busy: false,
			tail: [
				{
					entry_id: 'e1',
					timestamp: '',
					message: { role: 'user', content: [{ type: 'text', text: sent }] }
				}
			] as never,
			streaming: [],
			tools: [],
			queue: []
		});
		for (const m of [live, reloaded]) {
			expect(m.items).toHaveLength(1);
			expect(m.items[0]).toMatchObject({ kind: 'user', text: 'check the cache' });
		}
		// A prompt that only mentions the tag is left as written.
		const own = reduce(emptyModel(), {
			type: 'user_input',
			kind: 'prompt',
			text: 'what is the <memory> block?'
		} as unknown as AnyFrame);
		expect(own.items[0]).toMatchObject({ text: 'what is the <memory> block?' });
	});
});

describe('the queue of follow-ups', () => {
	const input = (kind: string, text: string) =>
		({ type: 'user_input', kind, text }) as unknown as AnyFrame;
	const start = { type: 'agent_start', agent: 'lead' } as unknown as AnyFrame;

	it('holds a follow-up in the queue, not in the transcript, until the agent takes it up', () => {
		let m = addQueued(emptyModel(), 'follow_up', 'then run the tests');
		expect(m.queue).toEqual([{ kind: 'follow_up', text: 'then run the tests' }]);
		expect(m.items).toHaveLength(0);
		m = reduce(m, start);
		expect(m.queue).toEqual([]);
		expect(m.items).toHaveLength(1);
		expect(m.items[0]).toMatchObject({
			kind: 'user',
			text: 'then run the tests',
			via: 'follow_up'
		});
	});

	it('queues the follow-up another client sent, and takes the oldest one per run', () => {
		let m = reduce(emptyModel(), input('follow_up', 'one'));
		m = reduce(m, input('follow_up', 'two'));
		expect(m.queue.map((q) => q.text)).toEqual(['one', 'two']);
		expect(m.items).toHaveLength(0);
		m = reduce(m, start);
		expect(m.queue.map((q) => q.text)).toEqual(['two']);
		expect(m.items.map((i) => i.kind === 'user' && i.text)).toEqual(['one']);
	});

	it('takes steering at the next request, and leaves a follow-up waiting', () => {
		const snap = applySnapshot(emptyModel(), {
			busy: true,
			tail: [],
			streaming: [],
			tools: [],
			queue: [
				{ kind: 'follow_up', text: 'later' },
				{ kind: 'steer', text: 'now' }
			]
		});
		expect(snap.queue).toHaveLength(2);
		const first = reduce(snap, {
			type: 'turn_start',
			agent: 'lead',
			turn: 1
		} as unknown as AnyFrame);
		expect(first.queue).toHaveLength(2);
		const second = reduce(snap, {
			type: 'turn_start',
			agent: 'lead',
			turn: 2
		} as unknown as AnyFrame);
		expect(second.queue).toEqual([{ kind: 'follow_up', text: 'later' }]);
		expect(second.items).toMatchObject([{ kind: 'user', text: 'now', via: 'steer' }]);
	});
});

describe('the summary of a branch that was left', () => {
	const NOTE =
		'I went back to an earlier point of this conversation. What happened on the branch I left, in short:';
	const summary = `${NOTE}\nThe user asked for a cache.\nIt was half written.`;

	it("is a quiet notice, live and after a reload, never the user's own bubble", () => {
		const live = reduce(emptyModel(), {
			type: 'user_input',
			kind: 'prompt',
			text: summary
		} as unknown as AnyFrame);
		const reloaded = applySnapshot(emptyModel(), {
			busy: false,
			tail: [
				{
					entry_id: 'e1',
					timestamp: '',
					message: { role: 'user', content: [{ type: 'text', text: summary }] }
				}
			] as never,
			streaming: [],
			tools: [],
			queue: []
		});
		for (const m of [live, reloaded]) {
			expect(m.items).toHaveLength(1);
			expect(m.items[0]).toMatchObject({
				kind: 'notice',
				level: 'info',
				summary: true,
				text: 'The user asked for a cache.\nIt was half written.'
			});
		}
	});

	it('leaves a prompt that merely mentions it alone', () => {
		const m = reduce(emptyModel(), {
			type: 'user_input',
			kind: 'prompt',
			text: `why does it say "${NOTE}"?`
		} as unknown as AnyFrame);
		expect(m.items[0]).toMatchObject({ kind: 'user' });
	});
});

describe('treeOrder', () => {
	const member = (id: string, parent: string | null, depth: number) => ({
		id,
		def: 'scout',
		parent,
		depth,
		state: 'queued',
		elapsed_ms: 0,
		tokens: 0
	});

	it('puts an agent under the one that started it, not under the last row', () => {
		// The lead starts x and y; x then starts z (issue #7).
		const roster = [
			member('lead', null, 0),
			member('x', 'lead', 1),
			member('y', 'lead', 1),
			member('z', 'x', 2)
		];
		expect(treeOrder(roster).map((m) => m.id)).toEqual(['lead', 'x', 'z', 'y']);
	});

	it('keeps an agent whose parent is gone, and agents in a cycle', () => {
		const roster = [
			member('lead', null, 0),
			member('a', 'gone', 2),
			member('b', 'c', 1),
			member('c', 'b', 1)
		];
		expect(
			treeOrder(roster)
				.map((m) => m.id)
				.sort()
		).toEqual(['a', 'b', 'c', 'lead']);
	});
});
