// The agents' browser as one session's page sees it: its state, the last picture, and whether this
// device is subscribed to pictures. Pictures cost the host work and the device data, so the page
// says when it wants them (`watch`) and this keeps the host in step with that across reconnects.
import { untrack } from 'svelte';
import { CommandError } from '$lib/rpc/client';
import type { BrowserInput, BrowserState, Command, FrameOf } from '$lib/protocol';

export type BrowserCommand = Extract<
	Command,
	{
		type: 'browser_watch' | 'browser_take' | 'browser_input' | 'browser_sign_in' | 'browser_open';
	}
>;

export interface Picture {
	/** A `data:` URL of the JPEG. */
	src: string;
	width: number;
	height: number;
	seq: number;
	tab: string;
}

/** The server marks what it refuses a device for its access with this `code`. */
const REFUSED = 'refused';
/**
 * How often a device that holds the browser tells the host it is still there. The host lets go of
 * a hold that has been silent for ten minutes, and reading a page sends nothing.
 */
export const HOLD_HEARTBEAT_MS = 3 * 60 * 1000;
const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

export class BrowserLive {
	/** What the host said last; null until it has said anything. */
	state = $state.raw<BrowserState | null>(null);
	picture = $state.raw<Picture | null>(null);
	/** The host sends pictures to this device. */
	watching = $state(false);
	/** This device may not watch (a view link, unless the host allows it). */
	denied = $state(false);
	error = $state<string | null>(null);
	/** A take-over, a hand-back, a sign-in or the start of the browser is on its way. */
	working = $state(false);

	// What decides is kept in plain fields, and the state above only shows it: `watch` is called
	// from the teardown of an effect, where reading a `$state` gives the value it had before, so a
	// subscription the host had just confirmed would look as if it was never made, and stay.
	private wanted = false;
	private wanters = 0;
	private connected = false;
	private syncing = false;
	/** What the host was told last, and took. */
	private subscribed = false;
	private refused = false;
	private heartbeat: ReturnType<typeof setInterval> | null = null;

	constructor(private readonly send: (c: BrowserCommand) => Promise<BrowserState | null>) {}

	/**
	 * A part of the page shows the browser from now until the function this returns is called.
	 * Pictures are asked for while at least one part shows it.
	 */
	want(): () => void {
		let held = true;
		this.wanters++;
		this.watch(true);
		return () => {
			if (!held) return;
			held = false;
			this.wanters--;
			this.watch(this.wanters > 0);
		};
	}
	/** Whether the page shows the browser now: pictures are asked for only while it does. */
	watch(on: boolean) {
		this.wanted = on;
		if (on) this.error = null;
		void this.sync();
	}

	/** The socket is attached: the host knows nothing of what an earlier socket asked for. */
	attached() {
		this.connected = true;
		this.subscribed = this.watching = false;
		this.refused = this.denied = false;
		void this.sync();
	}
	/** The socket is gone, and with it the subscription and a take-over (the host lets go of both). */
	detached() {
		this.connected = false;
		this.subscribed = this.watching = false;
		const s = untrack(() => this.state);
		if (s?.held_by_you) this.state = { ...s, held: false, held_by_you: false };
		this.keepHolding(false);
	}

	/** While this device holds the browser, takes it over again now and then: that renews the hold. */
	private keepHolding(holding: boolean) {
		if (!holding) {
			if (this.heartbeat !== null) clearInterval(this.heartbeat);
			this.heartbeat = null;
		} else if (this.heartbeat === null) {
			this.heartbeat = setInterval(() => {
				if (this.connected)
					void this.send({ type: 'browser_take', on: true }).then(
						(s) => s && this.onState(s),
						() => {}
					);
			}, HOLD_HEARTBEAT_MS);
		}
	}

	onState(s: BrowserState) {
		// A frame carries its `type` and its place in the log; the state is the rest.
		const { type: _type, at: _at, ...state } = s as BrowserState & { type?: string; at?: number };
		this.state = state;
		this.keepHolding(!!state.held_by_you);
		if (!state.running) this.picture = null;
		// A browser that starts is a reason to ask again for what was not given before.
		else if (this.wanted && !this.subscribed) void this.sync();
	}
	onFrame(f: FrameOf<'browser_frame'>) {
		// One that was on its way when the page stopped watching.
		if (!this.wanted) return;
		this.picture = {
			src: `data:image/jpeg;base64,${f.data}`,
			width: f.width,
			height: f.height,
			seq: f.seq,
			tab: f.tab
		};
	}

	private async sync() {
		if (this.syncing) return;
		this.syncing = true;
		try {
			while (this.connected && !this.refused && this.wanted !== this.subscribed) {
				const on = this.wanted;
				try {
					const s = await this.send({ type: 'browser_watch', on });
					if (!this.connected) break;
					this.subscribed = this.watching = on;
					if (s) this.onState(s);
				} catch (e) {
					if (!this.connected) break;
					if (e instanceof CommandError && e.code === REFUSED) this.refused = this.denied = true;
					else if (on) this.error = said(e);
					// Stopping failed: there is nothing to stop, then.
					else this.subscribed = this.watching = false;
					if (on) break;
				}
				// The last picture of a browser nobody looks at is old when it is looked at again.
				if (!on) this.picture = null;
			}
		} finally {
			this.syncing = false;
		}
	}

	private async run(c: BrowserCommand): Promise<boolean> {
		try {
			const s = await this.send(c);
			if (s) this.onState(s);
			this.error = null;
			return true;
		} catch (e) {
			this.error = said(e);
			return false;
		}
	}
	/** Takes the browser over (the agents wait), or hands it back. */
	async take(on: boolean): Promise<boolean> {
		this.working = true;
		try {
			return await this.run({ type: 'browser_take', on });
		} finally {
			this.working = false;
		}
	}
	input(input: BrowserInput): Promise<boolean> {
		return this.run({ type: 'browser_input', input });
	}
	/** Starts the agents' browser on a blank page when none runs, so there is a page to watch. */
	async open(): Promise<boolean> {
		this.working = true;
		try {
			return await this.run({ type: 'browser_open' });
		} finally {
			this.working = false;
		}
	}
	/** Shows the agents' browser profile in a window on the host. */
	async signIn(url?: string): Promise<boolean> {
		this.working = true;
		try {
			return await this.run({ type: 'browser_sign_in', ...(url ? { url } : {}) });
		} finally {
			this.working = false;
		}
	}
}
