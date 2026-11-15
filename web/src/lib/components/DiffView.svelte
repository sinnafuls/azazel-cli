<script lang="ts">
	import { parseDiff } from '$lib/tools';
	let { diff, max = 18 }: { diff: string; max?: number } = $props();
	const lines = $derived(parseDiff(diff));
	let all = $state(false);
	const shown = $derived(all ? lines : lines.slice(0, max));
	const stats = $derived({
		add: lines.filter((l) => l.kind === 'add').length,
		del: lines.filter((l) => l.kind === 'del').length
	});
</script>

<div class="bg-background overflow-hidden rounded-md border font-mono text-xs leading-5">
	<div class="scroll-thin overflow-x-auto">
		<div class="w-max min-w-full py-1">
			{#each shown as l, i (i)}
				{#if l.kind === 'meta'}
					<div class="text-muted-foreground px-3">{l.text}</div>
				{:else if l.kind === 'hunk'}
					<div class="bg-raised text-muted-foreground px-3">{l.text}</div>
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
						<span class="whitespace-pre">{l.text || ' '}</span>
						{#if l.kind === 'add'}<span class="sr-only">added</span>{:else if l.kind === 'del'}<span
								class="sr-only">removed</span
							>{/if}
					</div>
				{/if}
			{/each}
		</div>
	</div>
	<div
		class="text-muted-foreground flex items-center justify-between border-t px-3 py-1 text-[11px]"
	>
		<span class="num"
			><span class="text-diff-add-fg">+{stats.add}</span>
			<span class="text-diff-del-fg">−{stats.del}</span></span
		>
		{#if lines.length > max}
			<button
				type="button"
				class="hover:text-foreground py-1 underline underline-offset-2"
				onclick={() => (all = !all)}
			>
				{all ? 'Show less' : `Show all ${lines.length} lines`}
			</button>
		{/if}
	</div>
</div>
