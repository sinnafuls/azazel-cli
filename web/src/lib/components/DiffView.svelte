<script lang="ts">
	import { parseDiff } from '$lib/tools';
	let { diff, max = 18, file }: { diff: string; max?: number; file?: string } = $props();
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

<figure class="bg-inset overflow-hidden rounded-lg border font-mono text-[12px] leading-[1.55]">
	<figcaption class="bg-card flex items-center gap-2 border-b px-3 py-1.5 font-sans text-xs">
		<span class="text-ink-2 min-w-0 flex-1 truncate font-mono">{name || 'diff'}</span>
		<span class="num shrink-0 font-mono"
			><span class="text-diff-add-fg">+{stats.add}</span>
			<span class="text-diff-del-fg">−{stats.del}</span></span
		>
	</figcaption>
	<div class="scroll-thin overflow-x-auto">
		<div class="w-max min-w-full py-1">
			{#each shown as l, i (i)}
				{#if l.kind === 'hunk'}
					<div class="text-faint px-3 py-0.5 select-none">{l.text}</div>
				{:else}
					<div
						class="flex px-3 {l.kind === 'add'
							? 'bg-diff-add text-diff-add-fg'
							: l.kind === 'del'
								? 'bg-diff-del text-diff-del-fg'
								: 'text-muted-foreground'}"
					>
						<span class="w-4 shrink-0 select-none" aria-hidden="true"
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
			class="text-muted-foreground hover:text-foreground hover:bg-raised bg-card block min-h-9 w-full border-t font-sans text-xs"
			onclick={() => (all = !all)}
		>
			{all ? 'Show less' : `Show all ${body.length} lines`}
		</button>
	{/if}
</figure>
