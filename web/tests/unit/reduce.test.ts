import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import type { AnyFrame } from '$lib/protocol';
import {
	applySnapshot,
	emptyModel,
	leadBusy,
	reduce,
	type AssistantItem,
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

	it('remembers the position of the last frame for resuming', () => {
		let m = reduce(emptyModel(), { type: 'agent_start', agent: 'lead', at: 10 });
		m = reduce(m, { type: 'idle', agent: 'lead', stop: 'stop', at: 42 });
		m = reduce(m, { type: 'roster', members: [], at: 7 });
		expect(m.lastAt).toBe(42);
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
});
