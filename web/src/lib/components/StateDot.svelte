<script lang="ts">
	// One glyph for every state, so the same thing looks the same in the list, the header and the hub.
	// Only work in progress moves; a session that waits for you holds still inside a ring.
	import { cn } from '$lib/utils';
	let { state, class: className }: { state: string; class?: string } = $props();
	const kind = $derived(
		state === 'working' || state === 'running'
			? 'working'
			: state === 'waiting for you' || state === 'waiting for approval'
				? 'waiting'
				: state === 'interrupted' || state === 'failed'
					? 'failed'
					: state === 'done'
						? 'done'
						: state === 'queued'
							? 'queued'
							: 'idle'
	);
</script>

<span
	class={cn('relative inline-grid size-2.5 shrink-0 place-items-center', className)}
	aria-hidden="true"
>
	{#if kind === 'working'}
		<span class="bg-info absolute inset-0 animate-ping-soft rounded-full"></span>
		<span class="bg-info size-2 rounded-full"></span>
	{:else if kind === 'waiting'}
		<span class="bg-ember/30 absolute -inset-[3px] rounded-full"></span>
		<span class="bg-ember relative size-2 rounded-full"></span>
	{:else if kind === 'failed'}
		<span class="bg-destructive size-2 rounded-full"></span>
	{:else if kind === 'done'}
		<span class="bg-success size-2 rounded-full"></span>
	{:else if kind === 'queued'}
		<span class="border-faint size-2 rounded-full border-[1.5px]"></span>
	{:else}
		<span class="bg-faint size-2 rounded-full"></span>
	{/if}
</span>
