<script lang="ts">
	// A run of tool calls in the transcript: one quiet row ("Ran 3 commands") that opens in place,
	// on every screen, into a card per call. Closed, it costs one line; a command that is still
	// running shows its last lines beneath it.
	import type { ToolItem } from '$lib/session/reduce';
	import { describeSteps, diffStats, previewDiff, stepVerb, summarize, toolKind } from '$lib/tools';
	import ToolDetail from './ToolDetail.svelte';
	import { TOOL_ICONS } from './tool-icons';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Spinner } from '$lib/components/ui/spinner';
	import * as Collapsible from '$lib/components/ui/collapsible';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import ListChecks from '@lucide/svelte/icons/list';

	let { tools }: { tools: ToolItem[] } = $props();

	const isRunning = (t: ToolItem) => t.status === 'running' || t.status === 'pending';
	const statsOf = (t: ToolItem) => diffStats(t.diff ?? previewDiff(t.name, t.input));

	const one = $derived(tools.length === 1 ? tools[0] : null);
	const running = $derived(tools.find(isRunning) ?? null);
	const failures = $derived(tools.filter((t) => t.status === 'error').length);
	const label = $derived(
		one
			? stepVerb(one.name, isRunning(one))
			: describeSteps(tools.map((t) => t.name)) + (running ? '…' : '')
	);
	const target = $derived(one ? summarize(one.name, one.input) : '');
	const oneStats = $derived(one ? statsOf(one) : null);
	const RowIcon = $derived(one ? TOOL_ICONS[toolKind(one.name)] : ListChecks);

	let open = $state(false);
	// The cards are made when the row is first opened: a long session is mostly closed rows.
	let made = $state(false);
	// The last lines of a command that is still running, under the closed row.
	const tail = $derived(
		!open && running && running.name === 'shell' && running.live
			? running.live.trimEnd().split('\n').slice(-2).join('\n')
			: ''
	);
</script>

<Collapsible.Root
	{open}
	onOpenChange={(v) => {
		if (v) made = true;
		open = v;
	}}
	data-steps={tools.length}
	data-status={failures ? 'error' : running ? 'running' : 'done'}
>
	<Collapsible.Trigger>
		{#snippet child({ props })}
			<!-- Pulled out by its own padding, so its icon stands on the column's edge and the wash of a
			     hovered row reaches past it. -->
			<Button
				{...props}
				variant="ghost"
				class="-mx-2 w-[calc(100%+1rem)] justify-start gap-2 px-2 font-normal text-muted-foreground"
			>
				<RowIcon class={failures ? 'text-destructive' : running ? 'text-brand' : ''} />
				<!-- One call: the verb, then what it worked on, which gives way first. A run: its words alone. -->
				<span
					class="{one ? 'shrink-0' : 'min-w-0 flex-1 truncate text-left'} {failures
						? 'text-destructive'
						: 'text-foreground'}"
					data-row-text>{label}</span
				>
				{#if one}
					<span class="min-w-0 flex-1 truncate text-left font-mono text-xs">{target}</span>
				{/if}
				{#if oneStats && (oneStats.add || oneStats.del)}
					<span class="num flex shrink-0 gap-1.5 text-xs"
						>{#if oneStats.add}<span class="text-success">+{oneStats.add}</span
							>{/if}{#if oneStats.del}<span class="text-destructive">−{oneStats.del}</span
							>{/if}</span
					>
				{/if}
				{#if running}
					<Spinner class="text-brand" aria-label="running" />
				{:else if failures}
					<Badge variant="destructive">{one ? 'Failed' : `${failures} failed`}</Badge>
				{/if}
				<ChevronRight
					class="opacity-60 transition-transform duration-200 {open ? 'rotate-90' : ''}"
				/>
			</Button>
		{/snippet}
	</Collapsible.Trigger>
	{#if tail}
		<pre
			class="ml-6 line-clamp-2 overflow-hidden font-mono text-xs leading-5 whitespace-pre-wrap text-muted-foreground">{tail}</pre>
	{/if}
	<!-- The height runs from nothing to the cards' own and back; the margin leaves a focus ring whole. -->
	<Collapsible.Content
		class="-mx-1 overflow-hidden px-1 data-[state=closed]:animate-collapsible-up data-[state=open]:animate-collapsible-down"
	>
		{#if made}
			<div class="grid grid-cols-1 gap-2 pt-1.5 pb-1" data-tool-cards>
				{#each tools as t (t.id)}
					<ToolDetail tool={t} />
				{/each}
			</div>
		{/if}
	</Collapsible.Content>
</Collapsible.Root>
