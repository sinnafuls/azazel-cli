<script lang="ts">
	// Cycles dark, light, system. The icon is the theme in use; the name is in the label.
	import { app } from '$lib/app.svelte';
	import Sun from '@lucide/svelte/icons/sun';
	import Moon from '@lucide/svelte/icons/moon';
	import Monitor from '@lucide/svelte/icons/monitor';

	const Icon = $derived(app.theme === 'dark' ? Moon : app.theme === 'light' ? Sun : Monitor);
</script>

<button
	type="button"
	class="btn btn-quiet btn-icon"
	aria-label="Theme: {app.theme}"
	title="Theme: {app.theme}"
	onclick={() =>
		app.setTheme(app.theme === 'dark' ? 'light' : app.theme === 'light' ? 'system' : 'dark')}
	><Icon class="size-4" /></button
>
