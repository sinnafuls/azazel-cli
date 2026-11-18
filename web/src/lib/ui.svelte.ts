// What is open on the page: the sidebar (and how wide), the settings window, the search palette,
// a picker asked for by a `/` command. The session a page shows registers itself here so that the
// settings window and the shortcuts can reach it.
import type { Session } from '$lib/session/session.svelte';
import { clampWidth, SIDEBAR_DEFAULT } from '$lib/sidebar';

export const PANES = [
	['general', 'General'],
	['models', 'Models and roles'],
	['permissions', 'Permissions'],
	['usage', 'Usage'],
	['context', 'Context'],
	['all', 'All settings'],
	['devices', 'Devices and remote'],
	['shortcuts', 'Keyboard shortcuts'],
	['about', 'About']
] as const;
export type Pane = (typeof PANES)[number][0];

const K = {
	width: 'azazel-sidebar-width',
	collapsed: 'azazel-sidebar-collapsed',
	folded: 'azazel-folded'
};
const read = (k: string) => {
	try {
		return localStorage.getItem(k);
	} catch {
		return null;
	}
};
const store = (k: string, v: string) => {
	try {
		localStorage.setItem(k, v);
	} catch {
		/* the choice lasts for this page */
	}
};

class Ui {
	width = $state(SIDEBAR_DEFAULT);
	collapsed = $state(false);
	/** The sidebar as an overlay, on a narrow screen. */
	drawer = $state(false);
	/** Project folders whose sessions are folded away, by folder. */
	folded = $state<Record<string, boolean>>({});
	/** The settings pane that is open; null when the window is closed, '' for the list on a phone. */
	settings = $state<Pane | '' | null>(null);
	search = $state(false);
	pairing = $state(false);
	/** A picker of the status bar that something else asked to open. */
	picker = $state<'model' | 'thinking' | 'mode' | null>(null);
	/** The rewind sheet: open on the list of prompts, or on one entry. */
	rewind = $state<{ entryId?: string; text?: string } | null>(null);
	/** The prompt being rewritten: sending goes back to before it first (edit and resend). */
	edit = $state<{ text: string; entryId: string | null } | null>(null);
	/** The session on screen, if any. */
	session = $state.raw<Session | null>(null);

	init() {
		this.width = clampWidth(Number(read(K.width) ?? SIDEBAR_DEFAULT));
		this.collapsed = read(K.collapsed) === '1';
		try {
			const f = JSON.parse(read(K.folded) ?? '{}');
			if (f && typeof f === 'object') this.folded = f;
		} catch {
			/* start with every folder open */
		}
	}
	setWidth(w: number, remember = true) {
		this.width = clampWidth(w);
		if (remember) store(K.width, String(this.width));
	}
	toggleSidebar(narrow: boolean) {
		if (narrow) this.drawer = !this.drawer;
		else {
			this.collapsed = !this.collapsed;
			store(K.collapsed, this.collapsed ? '1' : '0');
		}
	}
	toggleFolder(cwd: string) {
		this.folded = { ...this.folded, [cwd]: !this.folded[cwd] };
		store(K.folded, JSON.stringify(this.folded));
	}
	openSettings(pane: Pane | '' = 'general') {
		this.drawer = false;
		this.search = false;
		this.settings = pane;
	}
}
export const ui = new Ui();
