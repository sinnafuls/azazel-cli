// Runes state around the reducer: one Session is one WebSocket to one azazel session.
import { RpcClient, type SocketFactory, type Status, type Timers } from '$lib/rpc/client';
import { wsUrl } from '$lib/api';
import type {
	AccountUsage,
	AnyFrame,
	AttachedRemote,
	Command,
	CommandRow,
	Decision,
	Image,
	ModelRow,
	Roles,
	Row,
	State
} from '$lib/protocol';
import {
	addLocalUser,
	applySnapshot,
	emptyModel,
	leadBusy,
	prependRows,
	reduce,
	takeToasts,
	withState,
	type Model,
	type Toast
} from './reduce';

const PAGE = 20;

export interface SessionOptions {
	id: string;
	url?: string;
	factory?: SocketFactory;
	timers?: Timers;
	onToast?: (t: Toast) => void;
	/** Apply frames at once instead of once per tick (tests). */
	sync?: boolean;
}

export class Session {
	// Frames are applied to an immutable model; $state.raw keeps Svelte from proxying it deeply.
	model = $state.raw<Model>(emptyModel());
	status = $state<Status>('connecting');
	statusDetail = $state<string | undefined>(undefined);
	attached = $state.raw<AttachedRemote | null>(null);
	models = $state.raw<ModelRow[]>([]);
	usage = $state.raw<AccountUsage[]>([]);
	roles = $state.raw<Roles | null>(null);
	commands = $state.raw<CommandRow[]>([]);
	files = $state.raw<string[] | null>(null);
	loadingEarlier = $state(false);

	readonly id: string;
	private client: RpcClient;
	private queue: AnyFrame[] = [];
	private tick: ReturnType<typeof setTimeout> | null = null;
	private stateTimer: ReturnType<typeof setTimeout> | null = null;
	private readonly o: SessionOptions;

	constructor(o: SessionOptions) {
		this.o = o;
		this.id = o.id;
		this.client = new RpcClient({
			url: o.url ?? wsUrl(o.id),
			since: () => this.model.lastAt,
			factory: o.factory,
			timers: o.timers,
			onFrame: (f) => this.push(f),
			onStatus: (s, d) => {
				this.status = s;
				this.statusDetail = d;
			},
			onAttached: (a, resync) => void this.onAttached(a, resync)
		});
	}

	get busy() {
		return leadBusy(this.model);
	}
	get state(): State | null {
		return this.model.state;
	}

	start() {
		this.client.connect();
	}
	stop() {
		if (this.tick) clearTimeout(this.tick);
		if (this.stateTimer) clearTimeout(this.stateTimer);
		this.client.close();
	}
	nudge() {
		this.client.nudge();
	}
	/** The page became visible: a phone may have suspended the socket. Check it, or replace it. */
	async resume() {
		if (this.status !== 'open') return this.client.nudge();
		const alive = await Promise.race([
			this.refreshState().then(
				() => true,
				() => false
			),
			new Promise<boolean>((r) => setTimeout(() => r(false), 4000))
		]);
		if (!alive) this.client.reset();
	}

	/** Applies every queued frame now. */
	flush() {
		if (this.tick) clearTimeout(this.tick);
		this.tick = null;
		if (!this.queue.length) return;
		let m = this.model;
		let sawEnd = false;
		for (const f of this.queue) {
			m = reduce(m, f);
			if (f.type === 'idle' || f.type === 'failed' || f.type === 'compaction') sawEnd = true;
		}
		this.queue = [];
		const [m2, toasts] = takeToasts(m);
		this.model = m2;
		for (const t of toasts) this.o.onToast?.(t);
		if (sawEnd) this.refreshStateSoon();
	}

	private push(f: AnyFrame) {
		this.queue.push(f);
		if (this.o.sync) return this.flush();
		this.tick ??= setTimeout(() => this.flush(), 33);
	}

	private async onAttached(a: AttachedRemote, resync: boolean) {
		this.attached = a;
		// A reconnect that resumes from `since` keeps the transcript; anything else rebuilds it.
		if (resync)
			this.model = applySnapshot(
				{ ...this.model, lastAt: a.cursor ? a.cursor : this.model.lastAt },
				a.snapshot,
				PAGE
			);
		await Promise.allSettled([this.refreshState(), this.refreshModels(), this.refreshUsage()]);
	}

	// --- queries ---------------------------------------------------------------------------

	async refreshState() {
		const state = await this.client.send<State>({ type: 'get_state' });
		this.model = withState(this.model, state);
	}
	private refreshStateSoon() {
		if (this.stateTimer) return;
		this.stateTimer = setTimeout(() => {
			this.stateTimer = null;
			void this.refreshState().catch(() => {});
		}, 400);
	}
	async refreshModels() {
		if (this.models.length) return;
		this.models = await this.client.send<ModelRow[]>({ type: 'list_models' });
	}
	async refreshUsage() {
		this.usage = await this.client.send<AccountUsage[]>({ type: 'get_usage' });
	}
	async refreshRoles() {
		this.roles = await this.client.send<Roles>({ type: 'list_roles' });
	}
	async loadCommands() {
		if (!this.commands.length)
			this.commands = await this.client.send<CommandRow[]>({ type: 'list_commands' });
	}
	async loadFiles() {
		if (this.files) return;
		this.files = await this.client.send<string[]>({ type: 'list_files' });
	}

	async loadEarlier() {
		const h = this.model.history;
		if (this.loadingEarlier || !h.more || !h.earliest) return;
		this.loadingEarlier = true;
		try {
			const rows = await this.client.send<Row[]>({
				type: 'get_messages',
				before: h.earliest,
				limit: PAGE
			} as unknown as Command);
			this.model = prependRows(this.model, rows, PAGE);
		} finally {
			this.loadingEarlier = false;
		}
	}

	async agentHistory(agent: string): Promise<Row[]> {
		return this.client.send<Row[]>({
			type: 'get_agent_messages',
			agent,
			limit: 40
		} as unknown as Command);
	}

	// --- commands --------------------------------------------------------------------------

	send<T = unknown>(c: Command) {
		return this.client.send<T>(c);
	}

	async prompt(text: string, images: Image[] = []) {
		const via = this.busy ? 'steer' : 'prompt';
		await this.client.send(
			this.busy ? { type: 'steer', text, images } : { type: 'prompt', text, images }
		);
		this.model = addLocalUser(
			{ ...this.model, runs: { ...this.model.runs, lead: { busy: true } } },
			text,
			via,
			images.length
		);
	}
	async followUp(text: string) {
		await this.client.send({ type: 'follow_up', text });
		this.model = addLocalUser(this.model, text, 'follow_up', 0);
	}
	/** `@name text`. False when `name` is neither an agent nor a definition: the caller sends a prompt. */
	async mention(name: string, text: string): Promise<boolean> {
		const r = await this.client.send<{ agent: string } | null>({
			type: 'agent_mention',
			name,
			text
		});
		if (r === null) return false;
		this.model = addLocalUser(this.model, `@${name} ${text}`, 'prompt', 0);
		return true;
	}
	abort() {
		return this.client.send({ type: 'abort' });
	}

	async approve(requestId: string, decision: Decision, feedback?: string) {
		await this.client.send({
			type: 'approval_response',
			request_id: requestId,
			decision,
			feedback
		});
		this.closeRequest(requestId);
	}
	async answer(requestId: string, value: unknown) {
		await this.client.send({
			type: 'ui_response',
			request_id: requestId,
			value
		} as unknown as Command);
		this.closeRequest(requestId);
	}
	private closeRequest(id: string) {
		this.model = { ...this.model, requests: this.model.requests.filter((r) => r.id !== id) };
	}

	async setModel(model: string) {
		await this.client.send({ type: 'set_model', chain: [model] });
		await this.refreshState();
	}
	async setMode(mode: string) {
		await this.client.send({ type: 'set_mode', mode });
		await this.refreshState();
	}
	async setThinking(level: string) {
		await this.client.send({ type: 'set_thinking', level });
		await this.refreshState();
	}
	async setAgent(agent: string) {
		await this.client.send({ type: 'set_agent', agent });
		await this.refreshState();
	}
	agentSend(to: string, message: string) {
		return this.client.send({ type: 'agent_send', to, message });
	}
	agentStop(agent: string) {
		return this.client.send({ type: 'agent_stop', agent });
	}
}
