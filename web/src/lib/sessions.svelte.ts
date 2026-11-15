import { api } from '$lib/api';
import type { SessionInfo } from '$lib/protocol';

class Sessions {
	list = $state.raw<SessionInfo[]>([]);
	loaded = $state(false);
	error = $state<string | null>(null);
	private timer: ReturnType<typeof setInterval> | null = null;
	private users = 0;

	async refresh() {
		try {
			this.list = await api.sessions();
			this.error = null;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loaded = true;
		}
	}
	/** Polls while at least one component wants the list and the page is visible. */
	watch() {
		this.users++;
		void this.refresh();
		this.timer ??= setInterval(() => {
			if (document.visibilityState === 'visible') void this.refresh();
		}, 5000);
		return () => {
			if (--this.users === 0 && this.timer) {
				clearInterval(this.timer);
				this.timer = null;
			}
		};
	}
}
export const sessions = new Sessions();

/** The states the list shows, in the words of the page. */
export const STATE_LABEL: Record<string, string> = {
	working: 'Working',
	'waiting for you': 'Needs you',
	idle: 'Idle',
	interrupted: 'Interrupted'
};
