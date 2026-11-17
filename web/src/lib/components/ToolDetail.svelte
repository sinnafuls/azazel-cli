<script lang="ts">
	// One tool call in full: what it was asked, what it changed and what it answered.
	import type { ToolItem } from '$lib/session/reduce';
	import { diffStats, prettyInput, previewDiff, summarize } from '$lib/tools';
	import { duration } from '$lib/format';
	import DiffView from './DiffView.svelte';
	import Copy from '@lucide/svelte/icons/copy';
	import Check from '@lucide/svelte/icons/check';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';

	let { tool }: { tool: ToolItem } = $props();

	const input = $derived((tool.input ?? {}) as Record<string, unknown>);
	const summary = $derived(summarize(tool.name, tool.input));
	const diff = $derived(tool.diff ?? previewDiff(tool.name, tool.input));
	const stats = $derived(diffStats(diff));
	const body = $derived(tool.output || tool.live);
	const failed = $derived(tool.status === 'error');
	const running = $derived(tool.status === 'running' || tool.status === 'pending');
	const edits = $derived(tool.name === 'edit' || tool.name === 'write');
	const command = $derived(
		tool.name === 'shell' && typeof input.command === 'string' ? input.command : null
	);
	const lines = $derived(body ? body.split('\n').length : 0);

	let copied = $state<string | null>(null);
	async function copy(what: string, text: string) {
		try {
			await navigator.clipboard.writeText(text);
			copied = what;
			setTimeout(() => (copied = copied === what ? null : copied), 1500);
		} catch {
			// The page may not be allowed to write the clipboard; the text stays selectable.
		}
	}
</script>

{#snippet block(label: string, text: string, tone = 'text-ink-2', meta = '')}
	<section class="space-y-2">
		<div class="flex items-center justify-between gap-3 px-1">
			<h3 class="text-muted-foreground text-sm font-medium">{label}</h3>
			<span class="flex items-center gap-2">
				{#if meta}<span class="text-faint meta">{meta}</span>{/if}
				<button
					type="button"
					class="btn btn-quiet btn-icon"
					aria-label="Copy {label.toLowerCase()}"
					onclick={() => copy(label, text)}
				>
					{#if copied === label}<Check class="text-success size-4" />{:else}<Copy
							class="size-4"
						/>{/if}
				</button>
			</span>
		</div>
		<pre
			class="well scroll-thin max-h-[22rem] overflow-auto rounded-[18px] border p-4 {tone}">{text}</pre>
	</section>
{/snippet}

<div class="space-y-5" data-tool={tool.name} data-status={tool.status}>
	<div class="space-y-1.5 px-1">
		<p class="text-[15px] leading-6 font-medium break-words">{summary || tool.name}</p>
		<p class="text-muted-foreground flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
			<span class="font-mono">{tool.name}</span>
			{#if running}
				<span class="tag tag-info"
					><LoaderCircle class="size-3 animate-spin" aria-hidden="true" />running</span
				>
			{:else if failed}
				<span class="tag tag-danger">failed</span>
			{:else if tool.durationMs !== undefined}
				<span aria-hidden="true">·</span><span class="meta">{duration(tool.durationMs)}</span>
			{/if}
			{#if tool.agent !== 'lead'}<span aria-hidden="true">·</span><span>{tool.agent}</span>{/if}
		</p>
	</div>

	{#if command !== null}
		{@render block('Command', command)}
	{:else if !edits}
		{@render block('Input', prettyInput(tool.input))}
	{/if}

	{#if diff}
		<section class="space-y-2">
			<div class="flex items-center justify-between px-1">
				<h3 class="text-muted-foreground text-sm font-medium">Changes</h3>
				{#if stats}<span class="meta"
						><span class="text-diff-add-fg">+{stats.add}</span>
						<span class="text-diff-del-fg">−{stats.del}</span></span
					>{/if}
			</div>
			<div class="overflow-hidden rounded-[18px] border">
				<DiffView {diff} max={200} flush />
			</div>
		</section>
	{/if}

	{#if body}
		{@render block(
			failed ? 'Error' : tool.output ? 'Output' : 'Output so far',
			body,
			failed ? 'text-destructive' : 'text-ink-2',
			`${lines} line${lines === 1 ? '' : 's'}`
		)}
	{:else if tool.status === 'done' && !diff}
		<p class="text-muted-foreground px-1 text-sm">No output.</p>
	{:else if running}
		<p class="shimmer px-1 text-sm">Waiting for output</p>
	{/if}
	{#if tool.imageCount}<p class="text-muted-foreground px-1 text-sm">
			{tool.imageCount} image{tool.imageCount > 1 ? 's' : ''} in the result (not shown here).
		</p>{/if}
</div>
