// Page-wide state: who this device is, the theme, toasts.
import { api, ApiError, pairSecret } from '$lib/api';
import type { Me } from '$lib/protocol';

export type Auth = 'checking' | 'pairing' | 'ok' | 'needs-link' | 'error';

class App {
	me = $state<Me | null>(null);
	auth = $state<Auth>('checking');
	error = $state<string | null>(null);
	theme = $state<'system' | 'light' | 'dark'>('system');

	applyTheme() {
		const stored = (localStorage.getItem('azazel-theme') as App['theme'] | null) ?? 'system';
		this.theme = stored;
		this.paint();
	}
	setTheme(t: App['theme']) {
		this.theme = t;
		if (t === 'system') localStorage.removeItem('azazel-theme');
		else localStorage.setItem('azazel-theme', t);
		this.paint();
	}
	private paint() {
		const el = document.documentElement;
		el.classList.toggle('light', this.theme === 'light');
		el.classList.toggle('dark', this.theme === 'dark');
		const dark =
			this.theme === 'dark' ||
			(this.theme === 'system' && matchMedia('(prefers-color-scheme: dark)').matches);
		document
			.querySelector('meta[name=theme-color]')
			?.setAttribute('content', dark ? '#0f1012' : '#f6f6f7');
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
	async logout() {
		await api.logout().catch(() => {});
		this.me = null;
		this.auth = 'needs-link';
	}
}

export const app = new App();
