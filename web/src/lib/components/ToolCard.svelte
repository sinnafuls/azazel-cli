<script lang="ts">
	import type { ToolItem } from '$lib/session/reduce';
	import { parseDiff, previewDiff, prettyInput, summarize } from '$lib/tools';
	import { duration } from '$lib/format';
	import DiffView from './DiffView.svelte';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import Terminal from '@lucide/svelte/icons/terminal';
	import FileDiff from '@lucide/svelte/icons/file-diff';
	import FileText from '@lucide/svelte/icons/file-text';
	import Search from '@lucide/svelte/icons/search';
	import Globe from '@lucide/svelte/icons/globe';
	import Users from '@lucide/svelte/icons/users';
	import Cpu from '@lucide/svelte/icons/cpu';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';

	let { tool }: { tool: ToolItem } = $props();

	let open = $state(false);
	const summary = $derived(summarize(tool.name, tool.input));
	const diff = $derived(tool.diff ?? previewDiff(tool.name, tool.input));
	const body = $derived(tool.output || tool.live);
	const failed = $derived(tool.status === 'error');
	const running = $derived(tool.status === 'running' || tool.status === 'pending');
	const lines = $derived(body ? body.split('\n').length : 0);
	const isShell = $derived(tool.name === 'shell');
	const edits = $derived(tool.name === 'edit' || tool.name === 'write');
	// The size of the change sits in the header, so the diff below needs no caption of its own.
	const stats = $derived.by(() => {
		if (!diff) return null;
		const ls = parseDiff(diff);
		return {
			add: ls.filter((l) => l.kind === 'add').length,
			del: ls.filter((l) => l.kind === 'del').length
		};
	});
	const Icon = $derived(
		isShell
			? Terminal
			: edits
				? FileDiff
				: tool.name === 'read'
					? FileText
					: tool.name === 'grep' || tool.name === 'glob'
						? Search
						: tool.name.startsWith('web')
							? Globe
							: tool.name === 'task'
								? Users
								: Cpu
	);
	// A failed call opens by itself: the reason is what the reader needs.
	$effect(() => {
		if (failed) open = true;
	});
</script>

<!-- A finished call is quiet: colour is kept for the two states that need the reader, running and failed. -->
<div
	class="tool surface overflow-hidden rounded-lg"
	data-tool={tool.name}
	data-status={tool.status}
>
	<button
		type="button"
		class="hover:bg-hover flex min-h-11 w-full items-center gap-2.5 px-3 text-left -outline-offset-2 transition-colors duration-100 fine:min-h-9"
		aria-expanded={open}
		onclick={() => (open = !open)}
	>
		<Icon
			class="size-4 shrink-0 {failed ? 'text-destructive' : running ? 'text-info' : 'text-faint'}"
			aria-hidden="true"
		/>
		<span class="shrink-0 font-mono text-xs font-medium">{tool.name}</span>
		<span class="text-muted-foreground min-w-0 flex-1 truncate font-mono text-xs">{summary}</span>
		<span class="flex shrink-0 items-center gap-2.5">
			{#if stats && (stats.add || stats.del)}
				<span class="meta"
					><span class="text-diff-add-fg">+{stats.add}</span>
					<span class="text-diff-del-fg">−{stats.del}</span></span
				>
			{/if}
			{#if running}
				<span class="tag tag-info"
					><LoaderCircle class="size-3 animate-spin" aria-hidden="true" />running</span
				>
			{:else if failed}
				<span class="tag tag-danger">failed</span>
			{:else if tool.durationMs !== undefined}
				<span class="text-faint meta">{duration(tool.durationMs)}</span>
			{/if}
			<ChevronRight
				class="text-faint size-4 transition-transform duration-150 {open ? 'rotate-90' : ''}"
				aria-hidden="true"
			/>
		</span>
	</button>

	{#if !open}
		{#if diff}
			<DiffView {diff} max={7} flush />
		{:else if isShell && body}
			<pre class="well text-ink-2 max-h-24 overflow-hidden rounded-none border-t px-3 py-2.5">{body
					.split('\n')
					.slice(-4)
					.join('\n')}</pre>
		{/if}
	{:else}
		{#if diff}<DiffView {diff} max={60} flush />{/if}
		{#if !edits}
			<div class="border-t px-3 py-3">
				<div class="label mb-1.5">Input</div>
				<pre class="well scroll-thin max-h-48 overflow-auto border p-3">{prettyInput(
						tool.input
					)}</pre>
			</div>
		{/if}
		{#if body}
			<div class="border-t px-3 py-3">
				<div class="label mb-1.5 flex justify-between">
					<span>{tool.output ? 'Result' : 'Output so far'}</span><span class="meta text-faint"
						>{lines} line{lines === 1 ? '' : 's'}</span
					>
				</div>
				<pre
					class="well scroll-thin max-h-72 overflow-auto border p-3 {failed
						? 'text-destructive'
						: 'text-ink-2'}">{body}</pre>
			</div>
		{:else if tool.status === 'done' && !diff}
			<p class="text-muted-foreground border-t px-3 py-3 text-xs">No output.</p>
		{/if}
		{#if tool.imageCount}<p class="text-muted-foreground border-t px-3 py-3 text-xs">
				{tool.imageCount} image{tool.imageCount > 1 ? 's' : ''} in the result (not shown here).
			</p>{/if}
	{/if}
</div>

<style>
	.tool {
		transition: border-color var(--dur) var(--ease-out);
	}
	.tool[data-status='running'],
	.tool[data-status='pending'] {
		border-color: color-mix(in oklab, var(--info) 38%, transparent);
	}
	.tool[data-status='error'] {
		border-color: color-mix(in oklab, var(--destructive) 50%, transparent);
	}
</style>
