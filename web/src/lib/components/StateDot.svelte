<script lang="ts">
	// One mark for every state, so the same thing looks the same in the list, the header and the hub.
	// It fills the 16px box an icon fills, so a row with a mark and a row with an icon line up.
	// Only work in progress moves; the accent means "running", amber "needs you", red "failed".
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
	class={cn('relative grid size-4 shrink-0 place-items-center', className)}
	data-slot="state-dot"
	data-state={kind}
	aria-hidden="true"
>
	{#if kind === 'working'}
		<span class="absolute size-2 animate-ping rounded-full bg-brand opacity-60"></span>
		<span class="size-2 rounded-full bg-brand"></span>
	{:else if kind === 'waiting'}
		<span class="size-2 rounded-full bg-warning ring-2 ring-warning/30"></span>
	{:else if kind === 'failed'}
		<span class="size-2 rounded-full bg-destructive"></span>
	{:else if kind === 'done'}
		<span class="size-2 rounded-full bg-success"></span>
	{:else if kind === 'queued'}
		<span class="size-2 rounded-full border-[1.5px] border-muted-foreground"></span>
	{:else}
		<span class="size-2 rounded-full bg-muted-foreground/50"></span>
	{/if}
</span>
