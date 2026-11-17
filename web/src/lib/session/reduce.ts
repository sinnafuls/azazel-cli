// A pure reducer: protocol frames in, a model of the conversation out. Nothing here touches the
// DOM or the network, so it runs the same in the page, in the tests and over recorded logs.
import type { AnyFrame, Block, Member, Row, SnapshotRemote, State } from '$lib/protocol';

export type ToolStatus = 'pending' | 'running' | 'done' | 'error';

export interface UserItem {
	kind: 'user';
	id: string;
	agent: string;
	text: string;
	via: 'prompt' | 'steer' | 'follow_up';
	images: number;
	/** Data URLs of the pictures this page sent, shown as thumbnails. */
	previews?: string[];
}
export interface TextBlock {
	index: number;
	type: 'text' | 'thinking';
	text: string;
}
export interface AssistantItem {
	kind: 'assistant';
	id: string;
	agent: string;
	blocks: TextBlock[];
	streaming: boolean;
	model?: string;
	tokens?: { input: number; output: number };
}
export interface ToolItem {
	kind: 'tool';
	id: string;
	agent: string;
	callId: string;
	name: string;
	input: unknown;
	argsText: string;
	status: ToolStatus;
	live: string;
	output: string;
	diff?: string;
	durationMs?: number;
	imageCount: number;
}
export interface NoticeItem {
	kind: 'notice';
	id: string;
	agent: string;
	level: 'info' | 'warn' | 'error';
	text: string;
}
export type Item = UserItem | AssistantItem | ToolItem | NoticeItem;

/** A request that waits for an answer: an approval, or a question from `ask_user` and friends. */
export type OpenRequest =
	| {
			kind: 'approval';
			id: string;
			agent: string;
			frame: Extract<AnyFrame, { type: 'approval_request' }>;
	  }
	| { kind: 'ui'; id: string; agent: string; frame: Extract<AnyFrame, { type: 'ui_request' }> };

export interface Toast {
	id: string;
	level: 'info' | 'warn' | 'error';
	text: string;
}

export interface AgentRun {
	busy: boolean;
	stop?: string;
}

export interface Model {
	seq: number;
	items: Item[];
	/** Message item currently being written, per agent. */
	open: Record<string, string>;
	runs: Record<string, AgentRun>;
	roster: Member[];
	rosterAt: number;
	requests: OpenRequest[];
	queue: { kind: string; text: string }[];
	toasts: Toast[];
	/** `at` of the last frame seen; what `attach` sends as `since` after a drop. */
	lastAt: number | null;
	history: { earliest: string | null; more: boolean };
	state: State | null;
	turn: number;
}

export function emptyModel(): Model {
	return {
		seq: 0,
		items: [],
		open: {},
		runs: {},
		roster: [],
		rosterAt: 0,
		requests: [],
		queue: [],
		toasts: [],
		lastAt: null,
		history: { earliest: null, more: false },
		state: null,
		turn: 0
	};
}

export function leadBusy(m: Model): boolean {
	return m.runs['lead']?.busy ?? m.state?.busy ?? false;
}

/** The memories azazel recalls for a prompt are appended to it in a `<memory>` block for the
 * model; the transcript shows only what the user wrote. */
export const withoutMemory = (text: string): string => {
	const at = text.lastIndexOf('\n\n<memory>\n');
	return at >= 0 && text.trimEnd().endsWith('</memory>') ? text.slice(0, at) : text;
};

const textOf = (blocks: Block[] | undefined): string =>
	(blocks ?? [])
		.filter((b): b is Extract<Block, { type: 'text' }> => b.type === 'text')
		.map((b) => b.text)
		.join('\n');

function toolOutput(output: unknown): {
	text: string;
	images: number;
	diff?: string;
	isError: boolean;
} {
	const o = (output ?? {}) as {
		content?: Block[];
		is_error?: boolean;
		details?: { diff?: unknown };
	};
	const diff = typeof o.details?.diff === 'string' ? o.details.diff : undefined;
	return {
		text: textOf(o.content),
		images: (o.content ?? []).filter((b) => b.type === 'image').length,
		diff,
		isError: !!o.is_error
	};
}

function next(m: Model, prefix: string): [Model, string] {
	const seq = m.seq + 1;
	return [{ ...m, seq }, `${prefix}${seq}`];
}

function patchItem<T extends Item>(m: Model, id: string, fn: (i: T) => T): Model {
	const at = m.items.findIndex((i) => i.id === id);
	if (at < 0) return m;
	const items = m.items.slice();
	items[at] = fn(items[at] as T);
	return { ...m, items };
}

function findTool(m: Model, agent: string, callId: string): ToolItem | undefined {
	for (let i = m.items.length - 1; i >= 0; i--) {
		const it = m.items[i];
		if (it.kind === 'tool' && it.callId === callId && it.agent === agent) return it;
	}
	return undefined;
}

function newTool(id: string, agent: string, callId: string, name: string): ToolItem {
	return {
		kind: 'tool',
		id,
		agent,
		callId,
		name,
		input: undefined,
		argsText: '',
		status: 'pending',
		live: '',
		output: '',
		imageCount: 0
	};
}

function upsertTool(
	m: Model,
	agent: string,
	callId: string,
	name: string,
	fn: (t: ToolItem) => ToolItem
): Model {
	const existing = findTool(m, agent, callId);
	if (existing) return patchItem<ToolItem>(m, existing.id, fn);
	const [m2, id] = next(m, 't');
	return { ...m2, items: [...m2.items, fn(newTool(id, agent, callId, name))] };
}

function ensureOpen(m: Model, agent: string): [Model, string] {
	const open = m.open[agent];
	if (open && m.items.some((i) => i.id === open)) return [m, open];
	const [m2, id] = next(m, 'a');
	const item: AssistantItem = { kind: 'assistant', id, agent, blocks: [], streaming: true };
	return [{ ...m2, items: [...m2.items, item], open: { ...m2.open, [agent]: id } }, id];
}

function setBlock(
	item: AssistantItem,
	index: number,
	type: 'text' | 'thinking',
	fn: (t: string) => string
): AssistantItem {
	const blocks = item.blocks.slice();
	const at = blocks.findIndex((b) => b.index === index && b.type === type);
	if (at >= 0) blocks[at] = { ...blocks[at], text: fn(blocks[at].text) };
	else blocks.push({ index, type, text: fn('') });
	return { ...item, blocks };
}

function stream(m: Model, agent: string, ev: Record<string, unknown>): Model {
	switch (ev.type) {
		case 'text_delta':
		case 'thinking_delta': {
			const [m2, id] = ensureOpen(m, agent);
			const type = ev.type === 'text_delta' ? 'text' : 'thinking';
			return patchItem<AssistantItem>(m2, id, (it) =>
				setBlock(it, ev.index as number, type, (t) => t + (ev.delta as string))
			);
		}
		case 'tool_call_start': {
			const [m2] = ensureOpen(m, agent);
			return upsertTool(m2, agent, ev.id as string, ev.name as string, (t) => ({
				...t,
				name: ev.name as string
			}));
		}
		case 'tool_call_delta': {
			// The delta names no call, only an index; the call started last at that index is the one.
			return m;
		}
		case 'block_end': {
			const block = ev.block as Block;
			if (block.type === 'tool_call') {
				return upsertTool(m, agent, block.id, block.name, (t) => ({
					...t,
					name: block.name,
					input: block.input
				}));
			}
			if (block.type === 'text' || block.type === 'thinking') {
				const [m2, id] = ensureOpen(m, agent);
				return patchItem<AssistantItem>(m2, id, (it) =>
					setBlock(it, ev.index as number, block.type as 'text' | 'thinking', () => block.text)
				);
			}
			return m;
		}
		default:
			return m;
	}
}

function messageEnd(
	m: Model,
	agent: string,
	message: {
		content?: Block[];
		meta?: { model?: string; usage?: { input: number; output: number } };
	}
): Model {
	const open = m.open[agent];
	const content = message.content ?? [];
	const blocks: TextBlock[] = content.flatMap((b, index): TextBlock[] =>
		(b.type === 'text' || b.type === 'thinking') && b.text.trim()
			? [{ index, type: b.type, text: b.text }]
			: []
	);
	let out: Model = m;
	if (open) {
		out = blocks.length
			? patchItem<AssistantItem>(m, open, (it) => ({
					...it,
					blocks,
					streaming: false,
					model: message.meta?.model,
					tokens: message.meta?.usage
						? { input: message.meta.usage.input, output: message.meta.usage.output }
						: undefined
				}))
			: { ...m, items: m.items.filter((i) => i.id !== open) };
	} else if (blocks.length) {
		const [m2, id] = next(m, 'a');
		out = {
			...m2,
			items: [
				...m2.items,
				{ kind: 'assistant', id, agent, blocks, streaming: false, model: message.meta?.model }
			]
		};
	}
	for (const b of content) {
		if (b.type === 'tool_call') {
			out = upsertTool(out, agent, b.id, b.name, (t) => ({ ...t, name: b.name, input: b.input }));
		}
	}
	const rest = { ...out.open };
	delete rest[agent];
	return { ...out, open: rest };
}

function addNotice(m: Model, agent: string, level: NoticeItem['level'], text: string): Model {
	const [m2, id] = next(m, 'n');
	return { ...m2, items: [...m2.items, { kind: 'notice', id, agent, level, text }] };
}

function closeStreaming(m: Model, agent: string): Model {
	const open = m.open[agent];
	if (!open) return m;
	const rest = { ...m.open };
	delete rest[agent];
	return patchItem<AssistantItem>({ ...m, open: rest }, open, (it) => ({
		...it,
		streaming: false
	}));
}

function setRun(m: Model, agent: string, run: AgentRun): Model {
	return { ...m, runs: { ...m.runs, [agent]: run } };
}

/** Applies one frame. Unknown frame types change nothing. */
export function reduce(m: Model, frame: AnyFrame): Model {
	const at = typeof frame.at === 'number' ? frame.at : null;
	const base: Model =
		at !== null && (m.lastAt === null || at > m.lastAt) ? { ...m, lastAt: at } : m;
	const f = frame as Record<string, unknown> & { type: string };
	const agent = (f.agent as string | undefined) ?? 'lead';
	switch (f.type) {
		case 'agent_start':
			return setRun(base, agent, { busy: true });
		case 'turn_start':
			return agent === 'lead' ? { ...base, turn: f.turn as number } : base;
		case 'message_start': {
			const [m2, id] = next(base, 'a');
			const item: AssistantItem = { kind: 'assistant', id, agent, blocks: [], streaming: true };
			return { ...m2, items: [...m2.items, item], open: { ...m2.open, [agent]: id } };
		}
		case 'stream':
			return stream(base, agent, f.event as Record<string, unknown>);
		case 'message_end':
			return messageEnd(base, agent, f.message as Parameters<typeof messageEnd>[2]);
		case 'tool_start':
			return upsertTool(base, agent, f.call_id as string, f.name as string, (t) => ({
				...t,
				name: f.name as string,
				input: f.input,
				status: 'running'
			}));
		case 'tool_update':
			return upsertTool(base, agent, f.call_id as string, '', (t) => ({
				...t,
				status: t.status === 'pending' ? 'running' : t.status,
				live: t.live ? `${t.live}\n${f.text as string}` : (f.text as string)
			}));
		case 'tool_end': {
			const o = toolOutput(f.output);
			return upsertTool(base, agent, f.call_id as string, '', (t) => ({
				...t,
				status: o.isError ? 'error' : 'done',
				output: o.text,
				diff: o.diff ?? t.diff,
				imageCount: o.images,
				durationMs: f.duration_ms as number
			}));
		}
		case 'idle':
			return setRun(closeStreaming(base, agent), agent, { busy: false, stop: f.stop as string });
		case 'failed':
			return addNotice(
				setRun(closeStreaming(base, agent), agent, { busy: false, stop: 'error' }),
				agent,
				'error',
				f.error as string
			);
		case 'retry':
			return addNotice(
				base,
				agent,
				'warn',
				`Retrying ${f.model as string} (attempt ${f.attempt as number})`
			);
		case 'fallback':
			return addNotice(
				base,
				agent,
				'warn',
				`Model fell back from ${f.from as string} to ${f.to as string}`
			);
		case 'account':
			return addNotice(
				base,
				agent,
				'warn',
				`${f.provider as string} account switched: ${f.limit as string}`
			);
		case 'tier_dropped':
			return addNotice(
				base,
				agent,
				'info',
				`${f.model as string} does not take the ${f.tier as string} tier`
			);
		case 'rule':
			return addNotice(
				base,
				agent,
				'info',
				`Stream rule “${f.name as string}” matched; asking again`
			);
		case 'compaction':
			return addNotice(
				base,
				agent,
				'info',
				`Context compacted: ${f.tokens_before as number} → ${f.tokens_after as number} tokens`
			);
		case 'roster':
			return { ...base, roster: f.members as Member[], rosterAt: Date.now() };
		case 'approval_request': {
			const frame = frame0(f) as Extract<AnyFrame, { type: 'approval_request' }>;
			const id = frame.request_id;
			if (base.requests.some((r) => r.id === id)) return base;
			return {
				...base,
				requests: [...base.requests, { kind: 'approval', id, agent: frame.agent, frame }]
			};
		}
		case 'ui_request': {
			const frame = frame0(f) as Extract<AnyFrame, { type: 'ui_request' }>;
			if ((frame as { kind: string }).kind === 'notify') {
				const n = frame as unknown as { request_id: string; level: string; text: string };
				const level = n.level === 'warn' || n.level === 'error' ? n.level : 'info';
				return { ...base, toasts: [...base.toasts, { id: n.request_id, level, text: n.text }] };
			}
			if (base.requests.some((r) => r.id === frame.request_id)) return base;
			return {
				...base,
				requests: [...base.requests, { kind: 'ui', id: frame.request_id, agent: 'lead', frame }]
			};
		}
		case 'request_closed':
			return { ...base, requests: base.requests.filter((r) => r.id !== f.request_id) };
		case 'user_input': {
			const [m2, id] = next(base, 'u');
			const text = withoutMemory(f.text as string);
			const via = (['prompt', 'steer', 'follow_up'] as const).find((v) => v === f.kind) ?? 'prompt';
			const agentId = (f.agent as string | null | undefined) ?? 'lead';
			return {
				...m2,
				items: [...m2.items, { kind: 'user', id, agent: agentId, text, via, images: 0 }],
				queue: m2.queue.filter((q) => q.text !== text)
			};
		}
		default:
			return base;
	}
}

const frame0 = (f: unknown) => f as AnyFrame;

/** What the page itself sent: the host does not echo a client's own input back to it. */
export function addLocalUser(
	m: Model,
	text: string,
	via: UserItem['via'],
	images: number,
	agent = 'lead',
	previews: string[] = []
): Model {
	const [m2, id] = next(m, 'u');
	const item: UserItem = { kind: 'user', id, agent, text, via, images };
	if (previews.length) item.previews = previews;
	return { ...m2, items: [...m2.items, item] };
}

export function takeToasts(m: Model): [Model, Toast[]] {
	return m.toasts.length ? [{ ...m, toasts: [] }, m.toasts] : [m, []];
}

/** Turns stored messages (`get_messages`, the snapshot tail) into items. */
export function rowsToItems(rows: Row[], agent = 'lead', prefix = 'h'): Item[] {
	const items: Item[] = [];
	const tools = new Map<string, ToolItem>();
	for (const row of rows) {
		const msg = row.message;
		if (!msg) continue;
		const id = `${prefix}${row.entry_id}`;
		if (msg.role === 'user') {
			const text = withoutMemory(textOf(msg.content));
			const images = msg.content.filter((b) => b.type === 'image').length;
			if (text || images) items.push({ kind: 'user', id, agent, text, via: 'prompt', images });
		} else if (msg.role === 'assistant') {
			const blocks: TextBlock[] = msg.content.flatMap((b, index): TextBlock[] =>
				(b.type === 'text' || b.type === 'thinking') && b.text.trim()
					? [{ index, type: b.type, text: b.text }]
					: []
			);
			if (blocks.length) {
				items.push({
					kind: 'assistant',
					id,
					agent,
					blocks,
					streaming: false,
					model: msg.meta?.model,
					tokens: msg.meta?.usage
						? { input: msg.meta.usage.input, output: msg.meta.usage.output }
						: undefined
				});
			}
			for (const b of msg.content) {
				if (b.type !== 'tool_call') continue;
				const t: ToolItem = {
					...newTool(`${id}:${b.id}`, agent, b.id, b.name),
					input: b.input,
					status: 'running'
				};
				tools.set(b.id, t);
				items.push(t);
			}
		} else if (msg.role === 'tool') {
			for (const b of msg.content) {
				if (b.type !== 'tool_result') continue;
				const t = tools.get(b.call_id);
				if (!t) continue;
				t.output = textOf(b.content);
				t.imageCount = b.content.filter((c) => c.type === 'image').length;
				t.status = b.is_error ? 'error' : 'done';
			}
		}
	}
	return items;
}

/** A history tool call whose result never came is not running: the session moved on. */
function settle(items: Item[], stillRunning: Set<string>): Item[] {
	return items.map((i) =>
		i.kind === 'tool' && i.status === 'running' && !stillRunning.has(i.callId)
			? { ...i, status: 'done' }
			: i
	);
}

/** Rebuilds the model from what `attach` reports: the tail, the replies being written, the tools running. */
export function applySnapshot(m: Model, snap: SnapshotRemote | undefined, tailLimit = 20): Model {
	const tail = ((snap?.tail ?? []) as unknown as Row[]) ?? [];
	const running = new Set((snap?.tools ?? []).map((t) => t.call_id));
	let items = settle(rowsToItems(tail), running);
	let seq = m.seq;
	const open: Record<string, string> = {};
	for (const s of snap?.streaming ?? []) {
		seq++;
		const id = `a${seq}`;
		const blocks: TextBlock[] = [];
		if (s.thinking) blocks.push({ index: 0, type: 'thinking', text: s.thinking });
		if (s.text) blocks.push({ index: 1, type: 'text', text: s.text });
		items.push({ kind: 'assistant', id, agent: s.agent, blocks, streaming: true });
		open[s.agent] = id;
	}
	for (const t of snap?.tools ?? []) {
		const existing = items.findIndex(
			(i) => i.kind === 'tool' && i.agent === t.agent && i.callId === t.call_id
		);
		const prior = existing >= 0 ? (items[existing] as ToolItem) : undefined;
		const tool: ToolItem = {
			...(prior ?? newTool(`t${++seq}`, t.agent, t.call_id, t.name)),
			input: t.input,
			status: 'running',
			durationMs: t.elapsed_ms
		};
		if (existing >= 0) items[existing] = tool;
		else items.push(tool);
	}
	items = items.slice();
	if (snap?.last_error) {
		seq++;
		items.push({
			kind: 'notice',
			id: `n${seq}`,
			agent: 'lead',
			level: 'error',
			text: snap.last_error
		});
	}
	return {
		...m,
		seq,
		items,
		open,
		runs: { ...m.runs, lead: { busy: !!snap?.busy } },
		queue: (snap?.queue ?? []).map((q) => ({ kind: q.kind, text: q.text })),
		requests: [],
		history: { earliest: tail[0]?.entry_id ?? null, more: tail.length >= tailLimit }
	};
}

/** Puts older rows in front of the transcript. */
export function prependRows(m: Model, rows: Row[], limit: number): Model {
	if (!rows.length) return { ...m, history: { ...m.history, more: false } };
	const older = rowsToItems(rows, 'lead', 'p');
	return {
		...m,
		items: [...settle(older, new Set()), ...m.items],
		history: { earliest: rows[0].entry_id, more: rows.length >= limit }
	};
}

export function withState(m: Model, state: State): Model {
	const runs = { ...m.runs, lead: { ...(m.runs['lead'] ?? {}), busy: state.busy } };
	// `get_state` carries the roster too; frames sent before attach are not replayed, so it seeds the hub.
	const seeded = !!state.agents?.length;
	return {
		...m,
		state,
		runs,
		roster: seeded ? (state.agents as Member[]) : m.roster,
		rosterAt: seeded ? Date.now() : m.rosterAt
	};
}

export function itemsFor(m: Model, agent: string): Item[] {
	return m.items.filter((i) => i.agent === agent);
}
