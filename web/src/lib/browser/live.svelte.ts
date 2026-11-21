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

/** The room a device has for the picture, in CSS pixels. */
export interface Room {
	width: number;
	height: number;
}
const sameRoom = (a: Room | null, b: Room | null) =>
	a === b || (!!a && !!b && a.width === b.width && a.height === b.height);

/** The server marks what it refuses a device for its access with this `code`. */
const REFUSED = 'refused';
/**
 * How often a device that holds the browser tells the host it is still there. The host lets go of
 * a hold that has been silent for ten minutes, and reading a page sends nothing.
 */
export const HOLD_HEARTBEAT_MS = 3 * 60 * 1000;
/** How long the room of the picture has to stay as it is before the host is told its new size. */
export const ROOM_SETTLE_MS = 250;
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
	/** The room the picture has here, and the one the host was told with the last `browser_watch`. */
	private room: Room | null = null;
	private told: Room | null = null;
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
	/**
	 * The room the picture has on this device, in CSS pixels: the host lays the agents' page out
	 * for that shape while this device watches, so the picture fills the room. A device that may
	 * not control never says one (the host refuses it), and `null` takes back what was said.
	 */
	fit(room: Room | null) {
		const next =
			room && room.width >= 1 && room.height >= 1
				? { width: Math.round(room.width), height: Math.round(room.height) }
				: null;
		if (sameRoom(next, this.room)) return;
		this.room = next;
		void this.sync();
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
		this.told = null;
		this.refused = this.denied = false;
		void this.sync();
	}
	/** The socket is gone, and with it the subscription and a take-over (the host lets go of both). */
	detached() {
		this.connected = false;
		this.subscribed = this.watching = false;
		this.told = null;
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
			// Watching again with another room is how the host is told that the room changed.
			while (
				this.connected &&
				!this.refused &&
				(this.wanted !== this.subscribed || (this.wanted && !sameRoom(this.room, this.told)))
			) {
				const on = this.wanted;
				const room = on ? this.room : null;
				try {
					const s = await this.send({ type: 'browser_watch', on, ...room });
					if (!this.connected) break;
					this.subscribed = this.watching = on;
					this.told = room;
					if (s) this.onState(s);
				} catch (e) {
					if (!this.connected) break;
					if (e instanceof CommandError && e.code === REFUSED) this.refused = this.denied = true;
					else if (on) this.error = said(e);
					// Stopping failed: there is nothing to stop, then.
					else {
						this.subscribed = this.watching = false;
						this.told = null;
					}
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
