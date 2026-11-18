<script lang="ts">
	// A bar with its label, its figure and a note beneath: a usage window, the context.
	let {
		label,
		value,
		figure,
		note = '',
		bad = false
	}: { label: string; value: number; figure?: string; note?: string; bad?: boolean } = $props();
	const pct = $derived(Math.max(0, Math.min(100, value)));
	const tone = $derived(
		bad || pct > 85 ? 'bg-destructive' : pct > 65 ? 'bg-ember' : 'bg-foreground'
	);
</script>

<div class="py-2.5">
	<div class="flex items-baseline justify-between gap-3">
		<span class="min-w-0 truncate text-[13px] font-medium">{label}</span>
		<span class="text-muted-foreground num shrink-0 text-xs"
			>{figure ?? `${Math.round(pct)}% used`}</span
		>
	</div>
	<div
		class="bg-hover mt-2 h-1.5 overflow-hidden rounded-full shadow-[inset_0_0_0_1px_var(--border)]"
		role="meter"
		aria-valuenow={Math.round(pct)}
		aria-valuemin="0"
		aria-valuemax="100"
		aria-label="{label} used"
	>
		<div class="h-full rounded-full {tone}" style="width: {Math.max(pct, 1.5)}%"></div>
	</div>
	{#if note}<p class="text-muted-foreground num mt-1.5 text-xs">{note}</p>{/if}
</div>
