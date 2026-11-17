<script lang="ts" module>
	let count = 0;
</script>

<script lang="ts">
	let { size = 24, word = true }: { size?: number; word?: boolean } = $props();
	// Each mark names its own gradients: one that is hidden (the other layout's) draws none.
	const id = `azazel-${++count}`;
</script>

<span class="inline-flex items-center gap-2.5">
	<!-- The mark: an ink tile with an angular A cut out of it and an ember spark at its apex. -->
	<svg viewBox="0 0 32 32" width={size} height={size} class="mark" aria-hidden="true">
		<defs>
			<linearGradient id="{id}-tile" x1="0" y1="0" x2="0" y2="1">
				<stop
					offset="0"
					stop-color="color-mix(in oklab, var(--foreground) 88%, var(--background))"
				/>
				<stop offset="1" stop-color="var(--foreground)" />
			</linearGradient>
			<radialGradient id="{id}-spark">
				<stop offset="0" stop-color="var(--ember)" stop-opacity="0.9" />
				<stop offset="1" stop-color="var(--ember)" stop-opacity="0" />
			</radialGradient>
		</defs>
		<rect width="32" height="32" rx="8" fill="url(#{id}-tile)" />
		<circle cx="16" cy="6.4" r="5.5" fill="url(#{id}-spark)" />
		<path
			d="M9 24.5 15 10.4a1.1 1.1 0 0 1 2 0l6 14.1M12 19.6h8"
			fill="none"
			stroke="var(--background)"
			stroke-width="2.4"
			stroke-linecap="round"
			stroke-linejoin="round"
		/>
		<circle cx="16" cy="6.4" r="2" fill="var(--ember)" />
	</svg>
	{#if word}<span class="text-[15px] font-semibold tracking-[-0.03em]">azazel</span>{/if}
</span>

<style>
	.mark {
		border-radius: 25%;
		box-shadow:
			0 4px 14px -4px color-mix(in oklab, var(--ember) 35%, transparent),
			var(--elev-1);
	}
</style>
