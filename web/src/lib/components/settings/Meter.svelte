<script lang="ts">
	// A Progress with its label, its figure and a note beneath: a usage window, the context.
	import { Progress } from '$lib/components/ui/progress';

	let {
		label,
		value,
		figure,
		note = '',
		bad = false
	}: { label: string; value: number; figure?: string; note?: string; bad?: boolean } = $props();
	const pct = $derived(Math.max(0, Math.min(100, value)));
	// The fill: the accent, amber when it runs high, red when it is nearly full or refused.
	const fill = $derived(
		bad || pct > 85
			? '[&>[data-slot=progress-indicator]]:bg-destructive'
			: pct > 65
				? '[&>[data-slot=progress-indicator]]:bg-warning'
				: '[&>[data-slot=progress-indicator]]:bg-brand'
	);
</script>

<div class="grid gap-1.5 py-2">
	<div class="flex items-baseline justify-between gap-3">
		<span class="min-w-0 truncate text-sm font-medium">{label}</span>
		<span class="num shrink-0 text-xs text-muted-foreground"
			>{figure ?? `${Math.round(pct)}% used`}</span
		>
	</div>
	<Progress value={Math.round(pct)} max={100} aria-label="{label} used" class={fill} />
	{#if note}<p class="num text-xs text-muted-foreground">{note}</p>{/if}
</div>
