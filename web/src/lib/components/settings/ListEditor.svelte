<script lang="ts" module>
	let count = 0;
</script>

<script lang="ts">
	// A short list of strings (rules, a chain of models, hosts): each with a remove button, and a
	// field that adds one on Enter. Read-only when `disabled`.
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

	const listId = `list-ed-${++count}`;
	let draft = $state('');
	function add() {
		const v = draft.trim();
		draft = '';
		if (v && !items.includes(v)) onchange?.([...items, v]);
	}
</script>

<div class="min-w-0">
	<ul class="flex flex-wrap gap-1.5" aria-label={label}>
		{#each items as it, i (it)}
			<li class="tag max-w-full font-mono {disabled ? '' : 'pr-0.5'}">
				<span class="truncate">{it}</span>
				{#if !disabled}
					<button
						type="button"
						class="hover:bg-active grid size-4 shrink-0 place-items-center rounded-full"
						aria-label="Remove {it}"
						onclick={() => onchange?.(items.filter((_, j) => j !== i))}><X class="size-3" /></button
					>
				{/if}
			</li>
		{:else}
			<li class="text-muted-foreground text-xs leading-[1.375rem]">None</li>
		{/each}
	</ul>
	{#if !disabled}
		<input
			class="field mt-2 h-8 font-mono text-[13px]"
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
