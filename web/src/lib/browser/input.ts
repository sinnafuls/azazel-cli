// What a pointer, a key or the wheel over the live picture of the agents' browser becomes on the
// wire. Nothing here touches the page, so all of it is tested without a browser.
import type { BrowserInput, BrowserState } from '$lib/protocol';

export interface Size {
	width: number;
	height: number;
}
export interface Box extends Size {
	left: number;
	top: number;
}

/**
 * Where a picture of `frame`'s shape is drawn inside `box` when it is scaled to fit with its
 * aspect kept and centred (`object-fit: contain`): the bars beside or above it are not picture.
 */
export function contained(box: Box, frame: Size): Box | null {
	if (!(box.width > 0 && box.height > 0 && frame.width > 0 && frame.height > 0)) return null;
	const scale = Math.min(box.width / frame.width, box.height / frame.height);
	const width = frame.width * scale;
	const height = frame.height * scale;
	return {
		left: box.left + (box.width - width) / 2,
		top: box.top + (box.height - height) / 2,
		width,
		height
	};
}

/**
 * A point of the screen as fractions (0 to 1) of the frame, from its top left corner; null when
 * the point is on a bar beside the picture or there is no picture to point at.
 */
export function fraction(
	point: { clientX: number; clientY: number },
	box: Box,
	frame: Size
): { x: number; y: number } | null {
	const drawn = contained(box, frame);
	if (!drawn) return null;
	const x = (point.clientX - drawn.left) / drawn.width;
	const y = (point.clientY - drawn.top) / drawn.height;
	// Half a pixel of grace: a pointer on the very edge still belongs to the picture.
	const gx = 0.5 / drawn.width;
	const gy = 0.5 / drawn.height;
	if (x < -gx || x > 1 + gx || y < -gy || y > 1 + gy) return null;
	const clamp = (n: number) => Math.min(1, Math.max(0, n));
	return { x: clamp(x), y: clamp(y) };
}

/** How many pixels of the frame one pixel of the drawn picture is. */
export function scaleOf(box: Box, frame: Size): number {
	const drawn = contained(box, frame);
	return drawn ? frame.height / drawn.height : 1;
}

/** The part of a KeyboardEvent that says which key it is. */
export interface KeyLike {
	key: string;
	ctrlKey?: boolean;
	altKey?: boolean;
	metaKey?: boolean;
	shiftKey?: boolean;
	isComposing?: boolean;
	getModifierState?: (key: string) => boolean;
}

// The keys the browser tool's `press` knows by name (`parse_keys` in azazel-browser).
const NAMED = new Set([
	'Enter',
	'Tab',
	'Escape',
	'Backspace',
	'Delete',
	'Insert',
	'ArrowUp',
	'ArrowDown',
	'ArrowLeft',
	'ArrowRight',
	'Home',
	'End',
	'PageUp',
	'PageDown'
]);

/**
 * A key press as the input to send: a character is `text`, anything else is `key` in the form of
 * the browser tool's `press` (`Enter`, `Shift+Tab`, `Control+a`). Null for what is no key of its
 * own (a modifier, a dead key, a key of the keyboard's media row).
 */
export function keysFor(e: KeyLike): Extract<BrowserInput, { kind: 'key' | 'text' }> | null {
	if (e.isComposing) return null;
	const k = e.key;
	// AltGr is reported as Control and Alt, and types a character.
	const chord = (!!e.ctrlKey || !!e.altKey || !!e.metaKey) && !e.getModifierState?.('AltGraph');
	let name: string;
	let named = true;
	if (NAMED.has(k) || /^F([1-9]|1[0-2])$/.test(k)) name = k;
	else if (k === ' ') name = 'Space';
	else if ([...k].length !== 1) return null;
	else if (!chord) return { kind: 'text', text: k };
	else {
		named = false;
		name = /^[a-z]$/i.test(k) ? k.toLowerCase() : k;
	}
	const mods = [
		e.ctrlKey && chord && 'Control',
		e.altKey && chord && 'Alt',
		e.metaKey && chord && 'Meta',
		e.shiftKey && (named || chord) && 'Shift'
	].filter(Boolean);
	return { kind: 'key', keys: [...mods, name].join('+') };
}

/**
 * What was typed into the address field as a URL to open; null when it is none. A bare host gets
 * `https://` (`http://` on this machine); only http and https open, as on the host.
 */
export function address(typed: string): string | null {
	const text = typed.trim();
	if (!text || /\s/.test(text)) return null;
	if (text === 'about:blank') return text;
	const local = /^(localhost|127\.0\.0\.1|\[::1\])(:\d+)?(\/|$)/i.test(text);
	const hasScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(text);
	if (!hasScheme && !local && !/^[^/:]+\.[^/:.]{2,}(:\d+)?(\/|$)/.test(text)) return null;
	const url = hasScheme ? text : `${local ? 'http' : 'https'}://${text}`;
	try {
		const u = new URL(url);
		return (u.protocol === 'http:' || u.protocol === 'https:') && u.hostname ? u.href : null;
	} catch {
		return null;
	}
}

/** The vertical travel of a wheel event in pixels, whatever unit the device reports it in. */
export function wheelDelta(e: { deltaY: number; deltaMode?: number }, page = 800): number {
	return e.deltaMode === 1 ? e.deltaY * 40 : e.deltaMode === 2 ? e.deltaY * page : e.deltaY;
}

export interface Clock {
	setTimeout: (fn: () => void, ms: number) => unknown;
	clearTimeout: (id: unknown) => void;
}
const realClock: Clock = {
	setTimeout: (fn, ms) => setTimeout(fn, ms),
	clearTimeout: (id) => clearTimeout(id as ReturnType<typeof setTimeout>)
};

/**
 * Scrolling, at most one command every `ms`: a wheel reports many small steps, and the host takes
 * a limited number of commands a second. The first step goes at once; those that follow within
 * `ms` are added up and sent together, at the place of the last one.
 */
export class Scrolls {
	private timer: unknown = null;
	private sum = 0;
	private at = { x: 0, y: 0 };
	constructor(
		private readonly send: (x: number, y: number, dy: number) => void,
		private readonly ms = 100,
		private readonly clock: Clock = realClock
	) {}
	add(x: number, y: number, dy: number) {
		this.at = { x, y };
		this.sum += dy;
		if (this.timer === null) this.fire();
	}
	private fire() {
		this.timer = null;
		if (!this.sum) return;
		const dy = this.sum;
		this.sum = 0;
		this.send(this.at.x, this.at.y, dy);
		this.timer = this.clock.setTimeout(() => this.fire(), this.ms);
	}
	/** Forgets what was not sent yet. */
	cancel() {
		if (this.timer !== null) this.clock.clearTimeout(this.timer);
		this.timer = null;
		this.sum = 0;
	}
}

/**
 * Clicks. A double click is one command with `count: 2`, which the host plays as both presses, so
 * the first click of a pair must not be sent on its own: a click waits `ms` for a second one.
 * `flush` sends a click that waits at once (before a key, so that what is typed lands where the
 * click put the caret).
 */
export class Clicks {
	private timer: unknown = null;
	private waiting: { x: number; y: number } | null = null;
	constructor(
		private readonly send: (x: number, y: number, count: number) => void,
		private readonly ms = 220,
		private readonly clock: Clock = realClock
	) {}
	/** `nth` is which click in a row this is, as the browser counts it (`MouseEvent.detail`). */
	click(x: number, y: number, nth = 1) {
		if (nth >= 2) {
			// The second of a pair (or the third: the host plays up to three). When the first already
			// went (a slow double click), the page still gets its double click from this one.
			this.drop();
			return this.send(x, y, Math.min(nth, 3));
		}
		this.flush();
		this.waiting = { x, y };
		this.timer = this.clock.setTimeout(() => this.flush(), this.ms);
	}
	flush() {
		const w = this.waiting;
		this.drop();
		if (w) this.send(w.x, w.y, 1);
	}
	/** Forgets a click that waits. */
	drop() {
		if (this.timer !== null) this.clock.clearTimeout(this.timer);
		this.timer = null;
		this.waiting = null;
	}
}

/**
 * Keys, paced for the host, which takes a limited number of commands a second: characters typed
 * within `ms` of each other go as one `text`, and a key that is held down repeats at most every
 * `repeatMs`. The order of what was pressed is kept: a key sends the text before it first.
 */
export class Typing {
	private timer: unknown = null;
	private text = '';
	private lastRepeat = -Infinity;
	constructor(
		private readonly send: (input: Extract<BrowserInput, { kind: 'key' | 'text' }>) => void,
		private readonly ms = 60,
		private readonly repeatMs = 90,
		private readonly clock: Clock = realClock,
		private readonly now: () => number = () => Date.now()
	) {}
	press(input: Extract<BrowserInput, { kind: 'key' | 'text' }>, repeat = false) {
		if (input.kind === 'text') {
			this.text += input.text;
			this.timer ??= this.clock.setTimeout(() => this.flush(), this.ms);
			return;
		}
		if (repeat) {
			const t = this.now();
			if (t - this.lastRepeat < this.repeatMs) return;
			this.lastRepeat = t;
		}
		this.flush();
		this.send(input);
	}
	flush() {
		if (this.timer !== null) this.clock.clearTimeout(this.timer);
		this.timer = null;
		const text = this.text;
		this.text = '';
		if (text) this.send({ kind: 'text', text });
	}
	/** Forgets what was not sent yet. */
	cancel() {
		this.text = '';
		this.flush();
	}
}

export type BrowserStatus = 'none' | 'watching' | 'agent' | 'you' | 'other';

/** What the panel says the browser is doing. `busy` is whether the agent is at work. */
export function statusOf(state: BrowserState | null, busy: boolean): BrowserStatus {
	if (!state?.running) return 'none';
	if (state.held) return state.held_by_you ? 'you' : 'other';
	return busy ? 'agent' : 'watching';
}
