<script lang="ts">
	// A unified diff as a CodeBlock: the file (or `title`) and the counts in its header, one
	// CodeBlock.Line per line with its number in the file, a green bar on what was added and a red
	// one on what was removed. `bare` leaves the frame and the header to the card it is put in.
	import { numberDiff, parseDiff } from '$lib/tools';
	import * as CodeBlock from '$lib/components/ui/code-block';
	import { Button } from '$lib/components/ui/button';
	import { cn } from '$lib/utils';

	let {
		diff,
		max = 18,
		file,
		title,
		bare = false,
		class: className
	}: {
		diff: string;
		max?: number;
		file?: string;
		/** What the header says instead of the file's name. */
		title?: string;
		/** Only the lines (and the "show all" row): the caller draws the frame and the header. */
		bare?: boolean;
		/** For the block that holds the lines, such as a height to scroll within. */
		class?: string;
	} = $props();
	const lines = $derived(numberDiff(parseDiff(diff)));
	// The file names are in the header, and the first hunk needs no rule above it.
	const body = $derived(
		lines.filter((l) => l.kind !== 'meta').filter((l, i) => !(i === 0 && l.kind === 'hunk'))
	);
	let all = $state(false);
	const shown = $derived(all ? body : body.slice(0, max));
	const stats = $derived({
		add: body.filter((l) => l.kind === 'add').length,
		del: body.filter((l) => l.kind === 'del').length
	});
	const name = $derived(
		file ?? lines.find((l) => l.text.startsWith('+++'))?.text.replace(/^\+\+\+ (b\/)?/, '') ?? ''
	);
	// The gutter is as wide as the longest number; a diff that does not say where it is has none.
	const digits = $derived(
		Math.max(0, ...body.map((l) => (l.n === undefined ? 0 : String(l.n).length)))
	);
	const SIGN = { add: '+', del: '−', ctx: ' ', hunk: ' ', meta: ' ' } as const;
	const LINE = {
		add: 'border-success bg-success/10',
		del: 'border-destructive bg-destructive/10',
		ctx: 'border-transparent text-muted-foreground',
		hunk: 'border-transparent text-muted-foreground select-none',
		meta: ''
	} as const;
</script>

{#snippet rows()}
	<CodeBlock.Content class={cn('scroll-thin', className)} data-diff-lines>
		{#each shown as l, i (i)}
			<CodeBlock.Line
				variant={l.kind === 'add' ? 'added' : l.kind === 'del' ? 'removed' : 'default'}
				data-line={l.kind}
				class={cn('border-l-2 after:content-none', LINE[l.kind])}
				>{#if digits}<span
						class="inline-block text-end text-muted-foreground select-none"
						style="width: {digits}ch"
						data-line-number>{l.n ?? ''}</span
					>{/if}<span class="inline-block w-[3ch] text-center select-none" aria-hidden="true"
					>{l.kind === 'hunk' ? '⋯' : SIGN[l.kind]}</span
				>{l.kind === 'hunk' ? '' : l.text || '\n'}{#if l.kind === 'add'}<span class="sr-only"
						>added</span
					>{:else if l.kind === 'del'}<span class="sr-only">removed</span>{/if}</CodeBlock.Line
			>
		{/each}
	</CodeBlock.Content>
	{#if body.length > max}
		<CodeBlock.Footer>
			<Button variant="ghost" size="xs" class="-ml-2" onclick={() => (all = !all)}>
				{all ? 'Show less' : `Show all ${body.length} lines`}
			</Button>
		</CodeBlock.Footer>
	{/if}
{/snippet}

{#if bare}
	{@render rows()}
{:else}
	<CodeBlock.Root size="sm" data-diff>
		<CodeBlock.Header>
			<CodeBlock.Title
				role="heading"
				aria-level={3}
				class={title ? '' : 'font-mono font-normal'}
				title={title ? undefined : name}>{title ?? (name || 'diff')}</CodeBlock.Title
			>
			<span class="num ml-auto shrink-0 text-xs"
				><span class="text-success">+{stats.add}</span>
				<span class="text-destructive">−{stats.del}</span></span
			>
		</CodeBlock.Header>
		{@render rows()}
	</CodeBlock.Root>
{/if}
