<script lang="ts">
	let { size = 24, word = true }: { size?: number; word?: boolean } = $props();
</script>

<span class="inline-flex items-center gap-2.5">
	<!-- The mark: a pinwheel of three angular blades, drawn in the text colour. -->
	<svg viewBox="0 0 32 32" width={size} height={size} fill="currentColor" aria-hidden="true">
		<path
			d="M15.08 14.82 L7.45 28.50 L1.50 28.50 L12.11 9.48 Z M16.56 3.50 L26.71 21.70 L20.75 21.70 L13.02 7.84 L15.44 3.50 Z M27.60 23.30 L30.50 28.50 L9.29 28.50 L12.19 23.30 Z"
		/>
	</svg>
	{#if word}<span class="text-[15px] font-semibold tracking-[-0.03em]">azazel</span>{/if}
</span>
