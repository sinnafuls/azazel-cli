<script lang="ts">
	// A short list of strings (rules, a chain of models, hosts): each a Badge with a remove Button
	// beside it, and an Input that adds one on Enter. Read-only when `disabled`.
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import X from '@lucide/svelte/icons/x';

	let {
		items,
		label,
		onchange,
		placeholder = 'Add one and press Enter',
		disabled = false,
		suggestions = []
	}: {
		items: string[];
		label: string;
		onchange?: (items: string[]) => void;
		placeholder?: string;
		disabled?: boolean;
		suggestions?: string[];
	} = $props();

	const listId = $props.id();
	let draft = $state('');
	function add() {
		const v = draft.trim();
		draft = '';
		if (v && !items.includes(v)) onchange?.([...items, v]);
	}
</script>

<div class="grid min-w-0 gap-2">
	<ul class="flex min-h-6 flex-wrap items-center gap-x-2 gap-y-1" aria-label={label}>
		{#each items as it, i (it)}
			<li class="flex max-w-full min-w-0 items-center gap-0.5">
				<Badge variant="secondary" class="min-w-0 shrink justify-start font-mono font-normal">
					<span class="truncate">{it}</span>
				</Badge>
				{#if !disabled}
					<Button
						variant="ghost"
						size="icon-xs"
						class="shrink-0 text-muted-foreground"
						aria-label="Remove {it}"
						onclick={() => onchange?.(items.filter((_, j) => j !== i))}><X /></Button
					>
				{/if}
			</li>
		{:else}
			<li class="text-xs text-muted-foreground">None</li>
		{/each}
	</ul>
	{#if !disabled}
		<Input
			class="font-mono text-xs md:text-xs"
			bind:value={draft}
			{placeholder}
			aria-label="Add to {label}"
			list={suggestions.length ? listId : undefined}
			autocomplete="off"
			spellcheck="false"
			onkeydown={(e) => {
				if (e.key === 'Enter') {
					e.preventDefault();
					add();
				}
			}}
		/>
		{#if suggestions.length}
			<datalist id={listId}>
				{#each suggestions as s (s)}<option value={s}></option>{/each}
			</datalist>
		{/if}
	{/if}
</div>
