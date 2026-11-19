<script lang="ts">
	// The quiet end of a pane that could not be read: never an error screen.
	import type { Loaded } from './load.svelte';
	import { Skeleton } from '$lib/components/ui/skeleton';

	let { of, what }: { of: Loaded<unknown>; what: string } = $props();
</script>

{#if of.state === 'loading'}
	<div class="grid gap-2 py-2" aria-busy="true" aria-label="Loading {what}">
		<Skeleton class="h-4 w-3/4" />
		<Skeleton class="h-4 w-1/2" />
		<Skeleton class="h-4 w-2/3" />
	</div>
{:else if of.state === 'none'}
	<p class="py-2 text-xs leading-relaxed text-muted-foreground" data-unavailable="none">
		Open or start a session to see {what}: it is read through a running session.
	</p>
{:else if of.state === 'unavailable'}
	<p class="py-2 text-xs leading-relaxed text-muted-foreground" data-unavailable>
		{what.charAt(0).toUpperCase() + what.slice(1)}: not available from this device.
		{#if of.detail}<span class="block">{of.detail}</span>{/if}
	</p>
{/if}
