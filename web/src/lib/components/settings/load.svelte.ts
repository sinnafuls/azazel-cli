// What a settings pane reads from the computer, with the three ways it can end: there, nothing to
// ask (no session), or not available from this device (refused, or a host that lacks the command).
import { NoSession, Refused } from '$lib/host';

export class Loaded<T> {
	data = $state.raw<T | null>(null);
	state = $state<'loading' | 'ready' | 'none' | 'unavailable'>('loading');
	/** What the computer said when it refused. */
	detail = $state('');
	private readonly fn: () => Promise<T>;

	constructor(fn: () => Promise<T>) {
		this.fn = fn;
	}
	async load(quiet = false) {
		if (!quiet || this.state !== 'ready') this.state = 'loading';
		try {
			const v = await this.fn();
			if (v === null || v === undefined) throw new Error('The computer sent nothing.');
			this.data = v;
			this.state = 'ready';
		} catch (e) {
			this.data = null;
			// A refusal is the answer, not a fault: nothing to show beyond "not available".
			this.detail = e instanceof Refused ? '' : e instanceof Error ? e.message : String(e);
			this.state = e instanceof NoSession ? 'none' : 'unavailable';
		}
	}
	set(v: T) {
		this.data = v;
		this.state = 'ready';
	}
}
