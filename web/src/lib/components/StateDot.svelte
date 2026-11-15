<script lang="ts">
	import { cn } from '$lib/utils';
	let { state, class: className }: { state: string; class?: string } = $props();
	const look = $derived(
		state === 'working' || state === 'running'
			? 'bg-info animate-[pulse-dot_1.4s_ease-in-out_infinite]'
			: state === 'waiting for you' || state === 'waiting for approval'
				? 'bg-primary ring-4 ring-primary/25'
				: state === 'interrupted' || state === 'failed'
					? 'bg-destructive'
					: state === 'done'
						? 'bg-success'
						: state === 'queued'
							? 'border border-muted-foreground'
							: 'bg-muted-foreground/60'
	);
</script>

<span class={cn('inline-block size-2 shrink-0 rounded-full', look, className)} aria-hidden="true"
></span>
