<script lang="ts">
	// The quiet end of a pane that could not be read: never an error screen.
	import type { Loaded } from './load.svelte';

	let { of, what }: { of: Loaded<unknown>; what: string } = $props();
</script>

{#if of.state === 'loading'}
	<div class="space-y-2.5 py-2" aria-busy="true" aria-label="Loading {what}">
		{#each [72, 54, 63] as wd (wd)}
			<span class="skeleton block h-3 rounded-sm" style="width: {wd}%"></span>
		{/each}
	</div>
{:else if of.state === 'none'}
	<p class="text-muted-foreground py-2 text-[13px] leading-relaxed" data-unavailable="none">
		Open or start a session to see {what}: it is read through a running session.
	</p>
{:else if of.state === 'unavailable'}
	<p class="text-muted-foreground py-2 text-[13px] leading-relaxed" data-unavailable>
		{what.charAt(0).toUpperCase() + what.slice(1)}: not available from this device.
		{#if of.detail}<span class="text-faint block text-xs">{of.detail}</span>{/if}
	</p>
{/if}
