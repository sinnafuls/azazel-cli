<script lang="ts">
	import type { ToolItem } from '$lib/session/reduce';
	import { previewDiff, prettyInput, summarize } from '$lib/tools';
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
	import Check from '@lucide/svelte/icons/check';
	import X from '@lucide/svelte/icons/x';

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

<div
	class="surface overflow-hidden rounded-xl transition-colors {failed
		? 'border-destructive/50'
		: running
			? 'border-info/35'
			: ''}"
	data-tool={tool.name}
	data-status={tool.status}
>
	<button
		type="button"
		class="hover:bg-raised/50 flex min-h-12 w-full items-center gap-3 px-3.5 py-2 text-left md:min-h-10"
		aria-expanded={open}
		onclick={() => (open = !open)}
	>
		<span
			class="bg-raised grid size-7 shrink-0 place-items-center rounded-md border {failed
				? 'text-destructive'
				: running
					? 'text-info'
					: 'text-muted-foreground'}"
			aria-hidden="true"
		>
			<Icon class="size-3.5" />
		</span>
		<span class="min-w-0 flex-1">
			<span class="flex items-baseline gap-2">
				<span class="text-[13px] font-medium">{tool.name}</span>
				<span class="text-muted-foreground min-w-0 flex-1 truncate font-mono text-xs"
					>{summary}</span
				>
			</span>
		</span>
		<span class="flex shrink-0 items-center gap-2 text-xs">
			{#if running}
				<span
					class="bg-inset relative hidden h-1 w-10 overflow-hidden rounded-full sm:block"
					aria-hidden="true"
				>
					<span
						class="bg-info absolute inset-y-0 left-0 w-1/3 animate-[bar_1.3s_ease-in-out_infinite] rounded-full"
					></span>
				</span>
				<span class="text-info">running</span>
			{:else if failed}
				<X class="text-destructive size-3.5" /><span class="text-destructive font-medium"
					>failed</span
				>
			{:else}
				<Check class="text-success size-3.5" />
				{#if tool.durationMs !== undefined}<span class="text-muted-foreground num font-mono"
						>{duration(tool.durationMs)}</span
					>{/if}
			{/if}
			<ChevronRight
				class="text-faint size-3.5 transition-transform duration-150 {open ? 'rotate-90' : ''}"
			/>
		</span>
	</button>

	{#if diff && !open}
		<div class="px-3 pb-3"><DiffView {diff} max={7} /></div>
	{:else if !open && isShell && body}
		<div class="px-3 pb-3">
			<pre
				class="bg-inset text-ink-2 max-h-24 overflow-hidden rounded-lg border px-3 py-2 text-[12px] leading-[1.55] whitespace-pre-wrap break-words">{body
					.split('\n')
					.slice(-4)
					.join('\n')}</pre>
		</div>
	{/if}

	{#if open}
		<div class="space-y-3 border-t px-3.5 py-3">
			{#if diff}<DiffView {diff} max={60} />{/if}
			{#if !edits}
				<div>
					<div class="label mb-1.5">Input</div>
					<pre
						class="bg-inset scroll-thin max-h-48 overflow-auto rounded-lg border p-3 text-[12px] leading-[1.55] whitespace-pre-wrap break-words">{prettyInput(
							tool.input
						)}</pre>
				</div>
			{/if}
			{#if body}
				<div>
					<div class="label mb-1.5 flex justify-between">
						<span>{tool.output ? 'Result' : 'Output so far'}</span><span
							class="num normal-case tracking-normal">{lines} line{lines === 1 ? '' : 's'}</span
						>
					</div>
					<pre
						class="bg-inset scroll-thin max-h-72 overflow-auto rounded-lg border p-3 text-[12px] leading-[1.55] whitespace-pre-wrap break-words {failed
							? 'text-destructive'
							: 'text-ink-2'}">{body}</pre>
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
