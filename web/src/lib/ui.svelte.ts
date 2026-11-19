// What is open on the page: the sidebar (and how wide), the settings window, the search palette,
// a picker asked for by a `/` command. The session a page shows registers itself here so that the
// settings window and the shortcuts can reach it.
import type { Session } from '$lib/session/session.svelte';
import type { useSidebar } from '$lib/components/ui/sidebar';

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
	collapsed: 'azazel-sidebar-collapsed',
	folded: 'azazel-folded',
	right: 'azazel-right-pane'
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
	/** The sidebar folded to its rail of icons, on a wide screen. */
	collapsed = $state(read(K.collapsed) === '1');
	/** The state of the shadcn sidebar, once it is on the page (components/Sidebar.svelte). */
	sidebar = $state.raw<ReturnType<typeof useSidebar> | null>(null);
	/** The sidebar as a sheet over the page, on a narrow screen. */
	get drawer(): boolean {
		return this.sidebar?.openMobile ?? false;
	}
	set drawer(open: boolean) {
		this.sidebar?.setOpenMobile(open);
	}
	/** Project folders whose sessions are folded away, by folder. */
	folded = $state<Record<string, boolean>>({});
	/** The settings pane that is open; null when the window is closed, '' for the list on a phone. */
	settings = $state<Pane | '' | null>(null);
	search = $state(false);
	pairing = $state(false);
	/** A picker of the status bar that something else asked to open. */
	picker = $state<'model' | 'profile' | 'thinking' | 'mode' | null>(null);
	/** The rewind sheet: open on the list of prompts, or on one entry. */
	rewind = $state<{ entryId?: string; text?: string } | null>(null);
	/** The prompt being rewritten: sending goes back to before it first (edit and resend). */
	edit = $state<{ text: string; entryId: string | null } | null>(null);
	/** The right pane (agents, usage and the agents' browser) docked beside the chat, from 1024px. */
	rightOpen = $state(true);
	rightTab = $state<'agents' | 'usage' | 'browser'>('agents');
	/** The session on screen, if any. */
	session = $state.raw<Session | null>(null);

	init() {
		this.collapsed = read(K.collapsed) === '1';
		this.rightOpen = read(K.right) !== '0';
		try {
			const f = JSON.parse(read(K.folded) ?? '{}');
			if (f && typeof f === 'object') this.folded = f;
		} catch {
			/* start with every folder open */
		}
	}
	setCollapsed(collapsed: boolean) {
		this.collapsed = collapsed;
		store(K.collapsed, collapsed ? '1' : '0');
	}
	/** Folds or unfolds the sidebar; on a narrow screen opens or closes its sheet. */
	toggleSidebar() {
		this.sidebar?.toggle();
	}
	setRight(open: boolean) {
		this.rightOpen = open;
		store(K.right, open ? '1' : '0');
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
