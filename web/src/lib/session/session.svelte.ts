// Runes state around the reducer: one Session is one WebSocket to one azazel session.
import { RpcClient, type SocketFactory, type Status, type Timers } from '$lib/rpc/client';
import { wsUrl } from '$lib/api';
import type {
	AccountUsage,
	AnyFrame,
	AttachedRemote,
	Command,
	CommandResult,
	CommandRow,
	Decision,
	Image,
	ModelRow,
	Roles,
	Row,
	State,
	TerminalTheme
} from '$lib/protocol';
import type { TreeEntry } from './tree';
import {
	addCommandCard,
	addQueued,
	addLocalUser,
	applySnapshot,
	commandCards,
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
	/** The terminal's theme, when the session reports it (`get_theme`) or it changes (a `theme` frame). */
	onTheme?: (t: TerminalTheme) => void;
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
	/** Why the model list could not be read, shown in the picker instead of waiting forever. */
	modelsError = $state<string | null>(null);
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
		// Not part of the conversation: the terminal changed its theme. The frame carries the
		// palette for a dark background; ask again with this device's own background.
		if (f.type === 'theme') {
			void this.refreshTheme(f);
			return;
		}
		this.queue.push(f);
		if (this.o.sync) return this.flush();
		this.tick ??= setTimeout(() => this.flush(), 33);
	}

	private async onAttached(a: AttachedRemote, resync: boolean) {
		this.attached = a;
		// A reconnect that resumes from `since` keeps the transcript; anything else rebuilds it.
		if (resync) this.rebuild(a);
		await Promise.allSettled([
			this.refreshState(),
			this.refreshModels(),
			this.refreshUsage(),
			this.refreshTheme()
		]);
	}
	/** The transcript from a snapshot; what slash commands showed is local, so it is carried over. */
	private rebuild(a: AttachedRemote) {
		const cards = commandCards(this.model);
		const m = applySnapshot(
			{ ...this.model, lastAt: a.cursor ? a.cursor : this.model.lastAt },
			a.snapshot,
			PAGE
		);
		this.model = cards.length ? { ...m, items: [...m.items, ...cards] } : m;
	}
	/** Reads the conversation again, after a command moved it (a fork, a rewind, another branch). */
	async reload() {
		try {
			// `replay: none`: the snapshot is the truth, nothing already shown is sent again.
			const a = await this.client.send<AttachedRemote>({ type: 'attach', replay: 'none' });
			this.attached = a;
			this.flush();
			const waiting = [...((a.snapshot as { pending?: string[] } | undefined)?.pending ?? [])];
			const requests = this.model.requests.filter((r) => waiting.includes(r.id));
			this.model = {
				...applySnapshot(
					{ ...this.model, lastAt: a.cursor ? a.cursor : this.model.lastAt },
					a.snapshot,
					PAGE
				),
				requests
			};
		} catch {
			this.model = { ...this.model, lastAt: null };
			this.client.reset();
			return;
		}
		await this.refreshState().catch(() => {});
	}
	/** Shows text in the transcript as the answer of a slash command. */
	showCard(name: string, args: string, text: string) {
		this.model = addCommandCard(this.model, name, args, text);
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
		try {
			this.models = await this.client.send<ModelRow[]>({ type: 'list_models' });
			this.modelsError = null;
		} catch (e) {
			this.modelsError = e instanceof Error ? e.message : String(e);
		}
	}
	async refreshUsage() {
		this.usage = await this.client.send<AccountUsage[]>({ type: 'get_usage' });
	}
	async refreshRoles() {
		this.roles = await this.client.send<Roles>({ type: 'list_roles' });
	}
	/** Resolves once the socket is attached, or false when it is not within `ms`. */
	async whenOpen(ms = 10_000): Promise<boolean> {
		const until = Date.now() + ms;
		while (this.status !== 'open') {
			if (Date.now() > until || this.status === 'ended' || this.status === 'missing') return false;
			await new Promise((r) => setTimeout(r, 50));
		}
		return true;
	}
	async loadCommands() {
		if (this.commands.length) return;
		// A line typed the moment the page opened comes before the socket is attached.
		if (!(await this.whenOpen())) throw new Error('The session is not connected.');
		this.commands = await this.client.send<CommandRow[]>({ type: 'list_commands' });
	}

	/**
	 * Asks for the terminal's theme with this device's background, so that a theme that follows
	 * the terminal takes the matching palette. A device that may not ask keeps `fallback` (the
	 * frame that made it ask) or else the built-in theme.
	 */
	async refreshTheme(fallback?: TerminalTheme) {
		const light =
			typeof matchMedia === 'function'
				? matchMedia('(prefers-color-scheme: light)').matches
				: undefined;
		try {
			this.o.onTheme?.(
				await this.client.send<TerminalTheme>({
					type: 'get_theme',
					...(light === undefined ? {} : { light })
				})
			);
		} catch {
			if (fallback) this.o.onTheme?.(fallback);
		}
	}
	/** Runs a slash command; what it shows becomes a card in the transcript. */
	async runCommand(name: string, args = ''): Promise<CommandResult> {
		const r = (await this.client.send<CommandResult | null>({
			type: 'run_command',
			name,
			args
		})) ?? { outcome: 'done' };
		if (r.outcome === 'show' && r.text) this.model = addCommandCard(this.model, name, args, r.text);
		return r;
	}

	// --- the session itself ---------------------------------------------------------------

	compact(instructions?: string) {
		return this.client.send({ type: 'compact', instructions: instructions?.trim() || null });
	}
	retry() {
		return this.client.send({ type: 'retry' });
	}
	async setName(name: string) {
		await this.client.send({ type: 'set_name', name });
		await this.refreshState().catch(() => {});
	}
	/** Continues in a copy of the session, cut at `entryId` when it is given. */
	async fork(entryId?: string) {
		await this.client.send({ type: 'fork_session', entry_id: entryId ?? null });
		await this.reload();
	}
	async newSession() {
		await this.client.send({ type: 'new_session' });
		await this.reload();
	}
	exportTo(format?: 'markdown' | 'html' | 'jsonl') {
		return this.client.send<{ path: string }>({ type: 'export', format: format ?? null });
	}
	/** A question on the side; the answer is shown as a card and never joins the conversation. */
	async btw(question: string) {
		const r = await this.client.send<{ answer: string; model?: string }>({
			type: 'btw',
			question
		});
		this.model = addCommandCard(this.model, 'btw', question, r?.answer ?? '');
		return r;
	}
	tree() {
		return this.client.send<TreeEntry[]>({ type: 'get_tree' });
	}
	async rewind(entryId: string, what: 'both' | 'code' | 'conversation' = 'both') {
		await this.client.send({ type: 'rewind', entry_id: entryId, what });
		await this.reload();
	}
	/** Goes back to before a prompt, so that it can be sent again in other words. */
	async backBefore(entryId: string) {
		await this.client.send({ type: 'set_leaf', entry_id: entryId, before: true });
		await this.reload();
	}
	/** Ends the session; `now` stops the lead first. */
	end(now = false) {
		return this.client.send({ type: 'stop', now });
	}
	async setRole(role: string, chain: string[] | null, save: 'session' | 'home' | 'project') {
		this.roles = await this.client.send<Roles>({ type: 'set_role', role, chain, save });
		await this.refreshState().catch(() => {});
	}
	async savePreset(name: string) {
		this.roles = await this.client.send<Roles>({ type: 'save_preset', name });
	}
	async applyPreset(name: string) {
		this.roles = await this.client.send<Roles>({ type: 'apply_preset', name });
		await this.refreshState().catch(() => {});
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
			});
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
		});
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
			images.length,
			'lead',
			images.map((im) => `data:${im.mime};base64,${im.data}`)
		);
	}
	async followUp(text: string) {
		await this.client.send({ type: 'follow_up', text });
		// It waits in the queue above the message box; the run that takes it up makes it a message.
		this.model = addQueued(this.model, 'follow_up', text);
	}
	/** Always a new prompt, whatever the page believes about the run (edit and resend). */
	async promptNow(text: string) {
		await this.client.send({ type: 'prompt', text, images: [] });
		this.model = addLocalUser(
			{ ...this.model, runs: { ...this.model.runs, lead: { busy: true } } },
			text,
			'prompt',
			0
		);
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
		});
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
