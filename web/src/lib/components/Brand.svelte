<script lang="ts">
	let { size = 24, word = true }: { size?: number; word?: boolean } = $props();
</script>

<span class="inline-flex items-center gap-2.5">
	<!-- The mark: an ink tile with an angular A cut out of it and an ember spark at its apex. -->
	<svg viewBox="0 0 32 32" width={size} height={size} class="mark" aria-hidden="true">
		<rect width="32" height="32" rx="8" fill="var(--foreground)" />
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
		box-shadow: var(--elev-1);
	}
</style>
