// The resume picker: whether it is open, and going on with an earlier session in the background.
import { goto } from '$app/navigation';
import { api } from '$lib/api';
import { app } from '$lib/app.svelte';
import { sessions } from '$lib/sessions.svelte';

class Resume {
	open = $state(false);

	/** Going on with a session starts a background session: a control device, on a server that can. */
	get available() {
		return app.me?.access === 'control' && !!app.me?.can_start;
	}
	show() {
		this.open = true;
	}
	close() {
		this.open = false;
	}
	/**
	 * Continues the saved session `id` in a background session and opens it. When one has it open
	 * already, the server answers with that one.
	 */
	async continueWith(id: string) {
		const host = await api.start({ resume: id, request_id: crypto.randomUUID() });
		this.close();
		void sessions.refresh();
		await goto(`/s/${encodeURIComponent(host)}`);
	}
}
export const resume = new Resume();
