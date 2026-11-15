<script lang="ts">
	import type { ToolItem } from '$lib/session/reduce';
	import { previewDiff, prettyInput, summarize } from '$lib/tools';
	import { duration } from '$lib/format';
	import DiffView from './DiffView.svelte';
	import StateDot from './StateDot.svelte';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import Terminal from '@lucide/svelte/icons/terminal';
	import FileDiff from '@lucide/svelte/icons/file-diff';
	import Cpu from '@lucide/svelte/icons/cpu';
	import Users from '@lucide/svelte/icons/users';

	let { tool }: { tool: ToolItem } = $props();

	let open = $state(false);
	const summary = $derived(summarize(tool.name, tool.input));
	const diff = $derived(tool.diff ?? previewDiff(tool.name, tool.input));
	const body = $derived(tool.output || tool.live);
	const failed = $derived(tool.status === 'error');
	const lines = $derived(body ? body.split('\n').length : 0);
	const isShell = $derived(tool.name === 'shell');
	// A failed call opens by itself: the reason is what the reader needs.
	$effect(() => {
		if (failed) open = true;
	});
</script>

<div
	class="bg-card overflow-hidden rounded-lg border {failed ? 'border-destructive/50' : ''}"
	data-tool={tool.name}
	data-status={tool.status}
>
	<button
		type="button"
		class="hover:bg-raised/60 flex min-h-11 w-full items-center gap-2.5 px-3 py-2 text-left md:min-h-9"
		aria-expanded={open}
		onclick={() => (open = !open)}
	>
		<span class="text-muted-foreground shrink-0" aria-hidden="true">
			{#if isShell}<Terminal class="size-3.5" />{:else if diff}<FileDiff
					class="size-3.5"
				/>{:else if tool.name === 'task'}<Users class="size-3.5" />{:else}<Cpu
					class="size-3.5"
				/>{/if}
		</span>
		<span class="font-mono text-[13px] font-medium">{tool.name}</span>
		<span class="text-muted-foreground min-w-0 flex-1 truncate font-mono text-xs">{summary}</span>
		<span class="flex shrink-0 items-center gap-2 text-xs">
			{#if tool.status === 'running' || tool.status === 'pending'}
				<StateDot state="working" />
				<span class="sr-only">running</span>
			{:else if failed}
				<span class="text-destructive font-medium">failed</span>
			{:else if tool.durationMs !== undefined}
				<span class="text-muted-foreground num">{duration(tool.durationMs)}</span>
			{/if}
			<ChevronRight
				class="text-muted-foreground size-3.5 transition-transform {open ? 'rotate-90' : ''}"
			/>
		</span>
	</button>

	{#if diff && !open}
		<div class="px-3 pb-3"><DiffView {diff} max={8} /></div>
	{/if}

	{#if open}
		<div class="space-y-2.5 border-t px-3 py-3">
			{#if diff}<DiffView {diff} max={40} />{/if}
			{#if !diff || (tool.name !== 'edit' && tool.name !== 'write')}
				<div>
					<div class="text-muted-foreground mb-1 text-[11px] tracking-wide uppercase">Input</div>
					<pre
						class="bg-background scroll-thin max-h-48 overflow-auto rounded-md border p-2.5 font-mono text-xs leading-5 whitespace-pre-wrap break-words">{prettyInput(
							tool.input
						)}</pre>
				</div>
			{/if}
			{#if body}
				<div>
					<div
						class="text-muted-foreground mb-1 flex justify-between text-[11px] tracking-wide uppercase"
					>
						<span>{tool.output ? 'Result' : 'Output so far'}</span><span class="num"
							>{lines} line{lines === 1 ? '' : 's'}</span
						>
					</div>
					<pre
						class="bg-background scroll-thin max-h-72 overflow-auto rounded-md border p-2.5 font-mono text-xs leading-5 whitespace-pre-wrap break-words {failed
							? 'text-destructive'
							: ''}">{body}</pre>
				</div>
			{:else if tool.status === 'done' && !diff}
				<p class="text-muted-foreground text-xs">No output.</p>
			{/if}
			{#if tool.imageCount}<p class="text-muted-foreground text-xs">
					{tool.imageCount} image{tool.imageCount > 1 ? 's' : ''} in the result (not shown here).
				</p>{/if}
		</div>
	{/if}
</div>
