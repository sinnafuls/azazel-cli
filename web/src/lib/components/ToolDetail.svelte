<script lang="ts">
	// One tool call as a card in the transcript: what it is in the header (Bash, or the file), then
	// the command or the input, the change it made with line numbers, and what it answered in a block
	// that scrolls by itself once it is longer than about twenty lines.
	import type { ToolItem } from '$lib/session/reduce';
	import { diffStats, prettyInput, previewDiff, toolKind, toolLabel } from '$lib/tools';
	import { highlightCode, loadLanguage } from '$lib/markdown';
	import { duration } from '$lib/format';
	import DiffView from './DiffView.svelte';
	import * as CodeBlock from '$lib/components/ui/code-block';
	import { Badge } from '$lib/components/ui/badge';
	import { Spinner } from '$lib/components/ui/spinner';

	let { tool }: { tool: ToolItem } = $props();

	const input = $derived((tool.input ?? {}) as Record<string, unknown>);
	const kind = $derived(toolKind(tool.name));
	const label = $derived(toolLabel(tool.name, tool.input));
	const diff = $derived(tool.diff ?? previewDiff(tool.name, tool.input));
	const stats = $derived(diffStats(diff));
	const body = $derived(tool.output || tool.live);
	const failed = $derived(tool.status === 'error');
	const running = $derived(tool.status === 'running' || tool.status === 'pending');
	const command = $derived(
		kind === 'command' && typeof input.command === 'string' ? input.command : null
	);
	// What the call was asked, unless the header already says it (a file's path).
	const asked = $derived(command !== null || label.path ? null : prettyInput(tool.input));
	const lang = $derived(command !== null ? 'bash' : 'json');
	// What the button of the header copies, and what it is called.
	const copies = $derived.by((): [string, string] => {
		if (command !== null) return ['command', command];
		if (asked !== null) return ['input', asked];
		if (kind === 'create' && typeof input.content === 'string') return ['file', input.content];
		if (kind === 'edit' && diff) return ['changes', diff];
		return ['path', label.text];
	});

	// The colours arrive with the language: plain until then, never a jump in size.
	let rev = $state(0);
	$effect(() => {
		void loadLanguage(lang).then((fresh) => fresh && rev++);
	});
	const code = $derived.by(() => {
		void rev;
		return highlightCode(command ?? asked ?? '', lang);
	});

	// Nothing between the header and what the call answered (a file that was read).
	const flush = $derived(command === null && asked === null && !diff);
	// About twenty lines, then the block scrolls by itself.
	const TALL = 'scroll-thin max-h-[calc(20lh+1.25rem)]';
	// The copy button of the output: over its corner, there on hover and focus, always on touch.
	const floating =
		'absolute top-1.5 right-3.5 z-10 bg-background opacity-0 ring-1 ring-foreground/10 transition-opacity group-focus-within/out:opacity-100 group-hover/out:opacity-100 pointer-coarse:opacity-100';
</script>

<!-- Outside the <pre>, where the whitespace around it is trimmed. The code is highlighted and
     sanitised (src/lib/markdown.ts) before it gets here. -->
{#snippet coloured()}
	<!-- eslint-disable-next-line svelte/no-at-html-tags -->
	{@html code}
{/snippet}

<CodeBlock.Root size="sm" data-tool={tool.name} data-status={tool.status}>
	<!-- One height whatever stands in it; with nothing under it but the answer, room above the rule. -->
	<CodeBlock.Header class={flush && body ? 'min-h-5 pb-2' : 'min-h-5'}>
		<CodeBlock.Title
			role="heading"
			aria-level={3}
			class={label.path ? 'font-mono font-normal' : 'font-normal text-muted-foreground'}
			title={label.path ? label.text : undefined}>{label.text}</CodeBlock.Title
		>
		<div class="ml-auto flex shrink-0 items-center gap-2 text-xs" data-tool-meta>
			{#if stats && (stats.add || stats.del)}
				<span class="num flex gap-1.5"
					>{#if stats.add}<span class="text-success">+{stats.add}</span>{/if}{#if stats.del}<span
							class="text-destructive">−{stats.del}</span
						>{/if}</span
				>
			{/if}
			{#if tool.agent !== 'lead'}<span>{tool.agent}</span>{/if}
			{#if running}
				<Spinner class="size-3.5 text-brand" aria-label="running" />
			{:else if failed}
				<Badge variant="destructive">Failed</Badge>
			{:else if tool.durationMs !== undefined}
				<span class="num">{duration(tool.durationMs)}</span>
			{/if}
		</div>
		<CodeBlock.Actions class="ms-0">
			<CodeBlock.CopyButton value={copies[1]} aria-label="Copy the {copies[0]}" />
		</CodeBlock.Actions>
	</CodeBlock.Header>

	{#if command !== null || asked !== null}
		<CodeBlock.Content wrap class="scroll-thin max-h-[calc(8lh+1rem)]" data-tool-command>
			<!-- One line box for the whole command, so the prompt and the code share their first line. -->
			<CodeBlock.Line
				>{#if command !== null}<span class="me-[1ch] text-muted-foreground select-none">$</span
					>{/if}{@render coloured()}</CodeBlock.Line
			>
		</CodeBlock.Content>
	{/if}

	{#if diff}<DiffView {diff} bare max={400} class={TALL} />{/if}

	{#if body}
		<div class="group/out relative border-t" data-tool-output>
			<CodeBlock.Content
				code={body}
				class="{TALL} pt-2.5! focus-visible:inset-ring-2 focus-visible:inset-ring-ring/50 {failed
					? 'text-destructive'
					: 'text-muted-foreground'}"
				aria-label={failed ? 'Error' : tool.output ? 'Output' : 'Output so far'}
			/>
			<CodeBlock.CopyButton value={body} aria-label="Copy the output" class={floating} />
		</div>
	{:else if running}
		<p
			class="animate-pulse px-2.5 pb-2.5 font-sans text-xs text-muted-foreground {flush
				? 'pt-1.5'
				: ''}"
		>
			Waiting for output
		</p>
	{:else if tool.status === 'done' && !diff}
		<p class="px-2.5 pb-2.5 font-sans text-xs text-muted-foreground {flush ? 'pt-1.5' : ''}">
			No output.
		</p>
	{/if}
	{#if tool.imageCount}
		<p class="border-t px-2.5 py-2 font-sans text-xs text-muted-foreground">
			{tool.imageCount} image{tool.imageCount > 1 ? 's' : ''} in the result (not shown here).
		</p>
	{/if}
</CodeBlock.Root>
