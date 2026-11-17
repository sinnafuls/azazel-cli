// Whether the "New chat" sheet is open, and the folder it opens on.
import { app } from '$lib/app.svelte';

class NewChat {
	open = $state(false);
	/** The folder to start in, when the sheet was opened from a session. */
	cwd = $state<string | null>(null);

	get available() {
		return app.me?.access === 'control' && !!app.me?.can_start;
	}
	show(cwd: string | null = null) {
		this.cwd = cwd;
		this.open = true;
	}
	close() {
		this.open = false;
	}
}
export const newChat = new NewChat();
