<script lang="ts">
	// Where a change is kept: this session, every session (home), or this project.
	import { app } from '$lib/app.svelte';
	import type { SaveTo } from '$lib/protocol';

	let { value = $bindable(), project = true }: { value: SaveTo; project?: boolean } = $props();
</script>

<label class="text-muted-foreground flex shrink-0 items-center gap-2 text-xs">
	Save for
	<select class="select h-8" bind:value aria-label="Where a change is saved">
		<option value="session">This session</option>
		<option value="home" disabled={!app.fullAccess}
			>Every session{app.fullAccess ? '' : ' (needs full access)'}</option
		>
		<option value="project" disabled={!app.fullAccess || !project}
			>This project{app.fullAccess ? '' : ' (needs full access)'}</option
		>
	</select>
</label>
