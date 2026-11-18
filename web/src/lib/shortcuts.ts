// The keyboard shortcuts of the page, in one table: the handler and the list in Settings read it.

export interface Shortcut {
	id: 'stop' | 'new-chat' | 'search' | 'settings' | 'sidebar' | 'shortcuts';
	/** `mod` is Command on a Mac and Control elsewhere. */
	combo: string;
	label: string;
}
export const SHORTCUTS: Shortcut[] = [
	{ id: 'new-chat', combo: 'mod+shift+o', label: 'New chat' },
	{ id: 'search', combo: 'mod+k', label: 'Search sessions' },
	{ id: 'sidebar', combo: 'mod+shift+s', label: 'Show or hide the sidebar' },
	{ id: 'settings', combo: 'mod+.', label: 'Settings' },
	{ id: 'shortcuts', combo: 'mod+/', label: 'Keyboard shortcuts' },
	{ id: 'stop', combo: 'escape', label: 'Stop the agent' }
];
/** What the message box answers to; listed, not handled here. */
export const COMPOSER_KEYS: [string, string][] = [
	['enter', 'Send (or a new line, see General)'],
	['shift+enter', 'New line'],
	['mod+enter', 'Queue for when the agent is done'],
	['/', 'Commands and skills'],
	['@', 'Mention an agent or a file']
];

export interface KeyLike {
	key: string;
	ctrlKey: boolean;
	metaKey: boolean;
	shiftKey: boolean;
	altKey: boolean;
}

export const isMac = (platform: string) => /mac|iphone|ipad/i.test(platform);

export function matches(e: KeyLike, combo: string, mac: boolean): boolean {
	const parts = combo.split('+');
	const key = parts[parts.length - 1];
	const mod = parts.includes('mod');
	const shift = parts.includes('shift');
	if (e.altKey) return false;
	if (mod !== (mac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey)) return false;
	// `?` and `>` are shifted keys on some layouts: shift is only held to account when a letter is meant.
	if (/^[a-z]$/.test(key) || key === 'escape' || key === 'enter') {
		if (shift !== e.shiftKey) return false;
	}
	return e.key.toLowerCase() === key;
}

/** The shortcut `e` is, if any. */
export function shortcutFor(e: KeyLike, mac: boolean): Shortcut | null {
	return SHORTCUTS.find((s) => matches(e, s.combo, mac)) ?? null;
}

/** The keys of a combo as they are printed on the keyboard. */
export function keysOf(combo: string, mac: boolean): string[] {
	return combo
		.split('+')
		.map((k) =>
			k === 'mod'
				? mac
					? '⌘'
					: 'Ctrl'
				: k === 'shift'
					? mac
						? '⇧'
						: 'Shift'
					: k === 'escape'
						? 'Esc'
						: k === 'enter'
							? mac
								? '↵'
								: 'Enter'
							: k.toUpperCase()
		);
}
