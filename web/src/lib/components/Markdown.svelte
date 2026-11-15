<script lang="ts">
	// Model text is untrusted: renderBlocks() sanitises every block before it gets here.
	import { loadLanguages, renderBlocks } from '$lib/markdown';
	let { text, streaming = false }: { text: string; streaming?: boolean } = $props();
	let rev = $state(0);
	$effect(() => {
		const t = text;
		void loadLanguages(t).then((changed) => {
			if (changed && t === text) rev++;
		});
	});
	const blocks = $derived.by(() => {
		void rev;
		return renderBlocks(text, !streaming);
	});
</script>

<div class="md">
	{#each blocks as html, i (i)}
		<!-- eslint-disable-next-line svelte/no-at-html-tags -->
		{@html html}
	{/each}
	{#if streaming}<span
			class="bg-primary ml-0.5 inline-block h-[1.05em] w-[2px] translate-y-[3px] animate-[caret_1s_steps(1)_infinite]"
			aria-hidden="true"
		></span>{/if}
</div>
