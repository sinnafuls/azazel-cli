<script lang="ts">
	let { size = 24, word = true }: { size?: number; word?: boolean } = $props();
</script>

<span class="inline-flex items-center gap-2.5">
	<!-- The mark: an angular A (two blades and a crossbar) with an ember spark at its apex. -->
	<svg viewBox="0 0 32 32" width={size} height={size} aria-hidden="true">
		<rect width="32" height="32" rx="9" fill="var(--raised)" />
		<rect
			x="0.5"
			y="0.5"
			width="31"
			height="31"
			rx="8.5"
			fill="none"
			stroke="var(--border-strong)"
		/>
		<path
			d="M8.5 24 14.6 9.2a1.5 1.5 0 0 1 2.8 0L23.5 24M11.6 19.4h8.8"
			fill="none"
			stroke="var(--foreground)"
			stroke-width="2.2"
			stroke-linecap="round"
			stroke-linejoin="round"
		/>
		<circle cx="16" cy="6.2" r="1.9" fill="var(--primary)" />
	</svg>
	{#if word}<span class="text-[15px] font-semibold tracking-[-0.02em]">azazel</span>{/if}
</span>
