// Page-wide state: who this device is, the theme and the other choices of Settings > General.
import { api, ApiError, pairSecret } from '$lib/api';
import type { Me, TerminalTheme } from '$lib/protocol';
import {
	ACCENTS,
	accentVars,
	DEFAULT_ACCENT,
	MAPPED_VARS,
	mapTheme,
	toHex,
	type Accent,
	type Mapped
} from '$lib/theme';

export type Auth = 'checking' | 'pairing' | 'ok' | 'needs-link' | 'error';
export type ThemeChoice = 'terminal' | 'dark' | 'light';
export type TextSize = 'small' | 'default' | 'large';
/** What Enter does in the message box; `auto` sends with a keyboard and makes a line on touch. */
export type EnterKey = 'auto' | 'send' | 'newline';
export type ThinkingView = 'follow' | 'show' | 'hide';

export const THEMES: { value: ThemeChoice; label: string; hint: string }[] = [
	{ value: 'terminal', label: 'Match terminal', hint: 'The colours of the azazel theme in use' },
	{ value: 'dark', label: 'Dark', hint: 'Neutral greys on a dark ground' },
	{ value: 'light', label: 'Light', hint: 'The same, on white' }
];
const TEXT_SCALE: Record<TextSize, string> = { small: '93.75%', default: '100%', large: '112.5%' };

const KEYS = {
	theme: 'azazel-theme',
	accent: 'azazel-accent',
	terminal: 'azazel-terminal-theme',
	text: 'azazel-text-size',
	enter: 'azazel-enter',
	notify: 'azazel-notify',
	thinking: 'azazel-thinking'
};
const read = (k: string) => {
	try {
		return localStorage.getItem(k);
	} catch {
		return null;
	}
};
const store = (k: string, v: string | null) => {
	try {
		if (v === null) localStorage.removeItem(k);
		else localStorage.setItem(k, v);
	} catch {
		/* private mode: the choice lasts for this page */
	}
};
const oneOf = <T extends string>(v: string | null, all: readonly T[], or: T): T =>
	all.includes(v as T) ? (v as T) : or;

class App {
	me = $state<Me | null>(null);
	auth = $state<Auth>('checking');
	error = $state<string | null>(null);

	theme = $state<ThemeChoice>('dark');
	/** The colour of focus, of what is running or chosen and of the meters, in the dark and light themes. */
	accent = $state<Accent>(DEFAULT_ACCENT);
	/** The terminal's theme, as the last session reported it; kept so the next visit starts in it. */
	terminal = $state.raw<TerminalTheme | null>(null);
	/** What "Match terminal" made of it: which variables it set and which fell back. */
	mapped = $state.raw<Mapped | null>(null);
	textSize = $state<TextSize>('default');
	enter = $state<EnterKey>('auto');
	notify = $state(false);
	/** Whether the transcript shows the model's thinking: the terminal's setting, or this page's own. */
	thinking = $state<ThinkingView>('follow');

	/** Reads the stored choices and paints; called once, before the first frame. */
	applyTheme() {
		this.theme = oneOf(read(KEYS.theme), ['terminal', 'dark', 'light'] as const, 'dark');
		this.accent = oneOf(
			read(KEYS.accent),
			ACCENTS.map((a) => a.value),
			DEFAULT_ACCENT
		);
		this.textSize = oneOf(read(KEYS.text), ['small', 'default', 'large'] as const, 'default');
		this.enter = oneOf(read(KEYS.enter), ['auto', 'send', 'newline'] as const, 'auto');
		this.notify = read(KEYS.notify) === '1';
		this.thinking = oneOf(read(KEYS.thinking), ['follow', 'show', 'hide'] as const, 'follow');
		try {
			const t = JSON.parse(read(KEYS.terminal) ?? 'null') as TerminalTheme | null;
			if (t && typeof t.roles === 'object') this.terminal = t;
		} catch {
			/* a stored theme that does not parse is as good as none */
		}
		this.paint();
	}
	setTheme(t: ThemeChoice) {
		this.theme = t;
		store(KEYS.theme, t);
		this.paint();
	}
	setAccent(a: Accent) {
		this.accent = a;
		store(KEYS.accent, a === DEFAULT_ACCENT ? null : a);
		this.paint();
	}
	/** A session told us the terminal's theme (`get_theme`, or a `theme` frame when it changes). */
	setTerminalTheme(t: TerminalTheme | null | undefined) {
		if (!t || typeof t.roles !== 'object' || !t.roles) return;
		this.terminal = { name: String(t.name ?? ''), light: !!t.light, roles: t.roles };
		store(KEYS.terminal, JSON.stringify(this.terminal));
		if (this.theme === 'terminal') this.paint();
	}
	setTextSize(s: TextSize) {
		this.textSize = s;
		store(KEYS.text, s === 'default' ? null : s);
		this.paint();
	}
	setThinking(v: ThinkingView) {
		this.thinking = v;
		store(KEYS.thinking, v === 'follow' ? null : v);
	}
	/** Whether thinking is drawn, given what the terminal's `tui.show_thinking` says. */
	showsThinking(terminal: boolean | undefined): boolean {
		return this.thinking === 'follow' ? (terminal ?? true) : this.thinking === 'show';
	}
	setEnter(e: EnterKey) {
		this.enter = e;
		store(KEYS.enter, e === 'auto' ? null : e);
	}
	/** Turns notifications on (asking the browser when it has not been asked) or off. */
	async setNotify(on: boolean): Promise<boolean> {
		if (on) {
			if (typeof Notification === 'undefined') return false;
			const p =
				Notification.permission === 'default'
					? await Notification.requestPermission()
					: Notification.permission;
			if (p !== 'granted') {
				this.notify = false;
				store(KEYS.notify, null);
				return false;
			}
		}
		this.notify = on;
		store(KEYS.notify, on ? '1' : null);
		return true;
	}

	/** Whether the page is painted light: the choice, or what the matched terminal theme is. */
	light = $state(false);

	private paint() {
		const el = document.documentElement;
		const mapped = this.theme === 'terminal' && this.terminal ? mapTheme(this.terminal) : null;
		const light = this.theme === 'light' || (this.theme === 'terminal' && !!mapped?.light);
		this.mapped = mapped;
		this.light = light;
		el.classList.toggle('light', light);
		el.classList.toggle('dark', !light);
		for (const v of MAPPED_VARS) el.style.removeProperty(v);
		// A matched terminal theme brings its own accent; the dark and light themes take the chosen one.
		const vars = mapped ? mapped.vars : accentVars(this.accent, light);
		for (const [k, v] of Object.entries(vars)) el.style.setProperty(k, v);
		el.style.setProperty('--text-scale', TEXT_SCALE[this.textSize]);
		const canvas = mapped ? toHex(mapped.palette.background) : light ? '#fafafa' : '#0a0a0a';
		document.querySelector('meta[name=theme-color]')?.setAttribute('content', canvas);
	}

	/** Pairs when the URL carries `#pair=…`, then asks who this device is. */
	async init() {
		this.auth = 'checking';
		const secret = pairSecret(location.hash);
		try {
			if (secret) {
				this.auth = 'pairing';
				// The secret must not stay in the address bar or the history.
				history.replaceState(null, '', location.pathname + location.search);
				this.me = await api.pair(secret);
			} else {
				this.me = await api.me();
			}
			this.auth = 'ok';
		} catch (e) {
			this.me = null;
			if (e instanceof ApiError && (e.status === 401 || e.status === 429)) {
				this.auth = 'needs-link';
				this.error = secret ? e.message : null;
			} else {
				this.auth = 'error';
				this.error = e instanceof Error ? e.message : String(e);
			}
		}
	}

	get canControl() {
		return this.me?.access === 'control';
	}
	/** This browser may do anything on the computer: settings, rules, every command. */
	get fullAccess() {
		return this.me?.full_access === true;
	}
	async logout() {
		await api.logout().catch(() => {});
		this.me = null;
		this.auth = 'needs-link';
	}
}

export const app = new App();
