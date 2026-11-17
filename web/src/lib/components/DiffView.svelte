<script lang="ts">
	import { parseDiff } from '$lib/tools';
	let {
		diff,
		max = 18,
		file,
		flush = false
	}: {
		diff: string;
		max?: number;
		file?: string;
		/** Inside a card that already names the file: no frame and no caption, only the lines. */
		flush?: boolean;
	} = $props();
	const lines = $derived(parseDiff(diff));
	const body = $derived(lines.filter((l) => l.kind !== 'meta'));
	let all = $state(false);
	const shown = $derived(all ? body : body.slice(0, max));
	const stats = $derived({
		add: body.filter((l) => l.kind === 'add').length,
		del: body.filter((l) => l.kind === 'del').length
	});
	const name = $derived(
		file ?? lines.find((l) => l.text.startsWith('+++'))?.text.replace(/^\+\+\+ (b\/)?/, '') ?? ''
	);
</script>

<figure
	class="bg-inset overflow-hidden font-mono text-xs leading-[1.6] {flush
		? 'border-t'
		: 'rounded-lg border'}"
>
	{#if !flush}
		<figcaption class="bg-card flex items-center gap-3 border-b px-3 py-2">
			<span class="text-ink-2 min-w-0 flex-1 truncate">{name || 'diff'}</span>
			<span class="num shrink-0"
				><span class="text-diff-add-fg">+{stats.add}</span>
				<span class="text-diff-del-fg">−{stats.del}</span></span
			>
		</figcaption>
	{/if}
	<div class="scroll-thin overflow-x-auto">
		<div class="w-max min-w-full py-1.5">
			{#each shown as l, i (i)}
				{#if l.kind === 'hunk'}
					<div class="text-faint px-3 py-0.5 select-none">{l.text}</div>
				{:else}
					<div
						class="flex px-3 {l.kind === 'add'
							? 'bg-diff-add text-diff-add-fg shadow-[inset_2px_0_0_var(--diff-add-fg)]'
							: l.kind === 'del'
								? 'bg-diff-del text-diff-del-fg shadow-[inset_2px_0_0_var(--diff-del-fg)]'
								: 'text-muted-foreground'}"
					>
						<span class="w-5 shrink-0 select-none" aria-hidden="true"
							>{l.kind === 'add' ? '+' : l.kind === 'del' ? '−' : ' '}</span
						>
						<span class="pr-3 whitespace-pre">{l.text || ' '}</span>
						{#if l.kind === 'add'}<span class="sr-only">added</span>{:else if l.kind === 'del'}<span
								class="sr-only">removed</span
							>{/if}
					</div>
				{/if}
			{/each}
		</div>
	</div>
	{#if body.length > max}
		<button
			type="button"
			class="text-muted-foreground hover:text-foreground hover:bg-hover block min-h-11 w-full border-t font-sans text-xs -outline-offset-2 transition-colors duration-100 fine:min-h-8"
			onclick={() => (all = !all)}
		>
			{all ? 'Show less' : `Show all ${body.length} lines`}
		</button>
	{/if}
</figure>
