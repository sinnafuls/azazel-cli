<script lang="ts">
	// The conversation: your messages, the agent's replies, its runs of tool calls and what slash
	// commands answered. It follows its own bottom while you are there and leaves you alone when you
	// scrolled up.
	import type { Item, ToolItem } from '$lib/session/reduce';
	import Markdown from './Markdown.svelte';
	import ToolSteps from './ToolSteps.svelte';
	import Brand from './Brand.svelte';
	import Tip from './Tip.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Spinner } from '$lib/components/ui/spinner';
	import * as Alert from '$lib/components/ui/alert';
	import * as Empty from '$lib/components/ui/empty';
	import * as Collapsible from '$lib/components/ui/collapsible';
	import * as CodeBlock from '$lib/components/ui/code-block';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import ArrowDown from '@lucide/svelte/icons/arrow-down';
	import Copy from '@lucide/svelte/icons/copy';
	import Check from '@lucide/svelte/icons/check';
	import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
	import GitFork from '@lucide/svelte/icons/git-fork';
	import History from '@lucide/svelte/icons/history';
	import Pencil from '@lucide/svelte/icons/pencil';
	import Terminal from '@lucide/svelte/icons/terminal';
	import { tokens } from '$lib/format';
	import { tick } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';

	let {
		items,
		busy = false,
		canLoadEarlier = false,
		loadingEarlier = false,
		onEarlier,
		showAgent = false,
		empty = 'Nothing has been said in this session yet.',
		onaction
	}: {
		items: Item[];
		busy?: boolean;
		canLoadEarlier?: boolean;
		loadingEarlier?: boolean;
		onEarlier?: () => void;
		showAgent?: boolean;
		empty?: string;
		/** Given when the reader may act on a message: retry, fork, rewind, edit. Copy needs nothing. */
		onaction?: (action: 'retry' | 'fork' | 'rewind' | 'edit', item: Item) => void;
	} = $props();

	// The last prompt can be rewritten and the last reply asked for again, once the agent is done.
	const lastUser = $derived(
		[...items].reverse().find((i) => i.kind === 'user' && i.agent === 'lead')
	);
	const lastReply = $derived(
		[...items].reverse().find((i) => i.kind === 'assistant' && i.agent === 'lead')
	);

	let copied = $state<string | null>(null);
	async function copy(id: string, text: string) {
		try {
			await navigator.clipboard.writeText(text);
			copied = id;
			setTimeout(() => copied === id && (copied = null), 1600);
		} catch {
			/* no clipboard here (an insecure origin): nothing to say */
		}
	}
	const replyText = (blocks: { type: string; text: string }[]) =>
		blocks
			.filter((b) => b.type === 'text')
			.map((b) => b.text)
			.join('\n\n');

	// Tool calls that follow one another are told as one run.
	type Part = Exclude<Item, ToolItem> | { kind: 'steps'; id: string; tools: ToolItem[] };
	const parts = $derived.by(() => {
		const out: Part[] = [];
		for (const item of items) {
			const last = out[out.length - 1];
			if (item.kind !== 'tool') out.push(item);
			else if (last?.kind === 'steps') last.tools.push(item);
			else out.push({ kind: 'steps', id: `steps-${item.id}`, tools: [item] });
		}
		return out;
	});

	let scroller = $state<HTMLDivElement>();
	let pinned = $state(true);
	let announce = $state('');

	const nearBottom = (el: HTMLElement) => el.scrollHeight - el.scrollTop - el.clientHeight < 80;
	function onscroll() {
		if (scroller) pinned = nearBottom(scroller);
	}
	async function toBottom(smooth = false) {
		await tick();
		scroller?.scrollTo({ top: scroller.scrollHeight, behavior: smooth ? 'smooth' : 'instant' });
		pinned = true;
	}

	// Follow the stream while the reader is at the bottom; leave them alone when they scrolled up.
	let last = '';
	$effect(() => {
		const tail = items[items.length - 1];
		const sig = `${items.length}:${tail?.id}:${tail && 'blocks' in tail ? tail.blocks.map((b) => b.text.length).join(',') : ''}:${tail && 'live' in tail ? tail.live.length + tail.output.length : ''}`;
		if (sig === last) return;
		last = sig;
		if (pinned) void toBottom();
	});

	// A screen reader gets finished replies, not every delta.
	$effect(() => {
		const done = [...items].reverse().find((i) => i.kind === 'assistant' && !i.streaming);
		if (done && done.kind === 'assistant')
			announce = done.blocks
				.filter((b) => b.type === 'text')
				.map((b) => b.text)
				.join(' ')
				.slice(0, 300);
	});

	// The terminal's `tui.show_thinking`, unless Settings > General > Thinking says otherwise.
	const showThinking = $derived(app.showsThinking(ui.session?.state?.show_thinking));
	let thinkingOpen = $state<Record<string, boolean>>({});
	const isOpen = (id: string, streaming: boolean) => thinkingOpen[id] ?? streaming;

	// The actions of a message: there on hover and on focus, always there on touch.
	const quiet =
		'opacity-0 transition-opacity duration-150 group-focus-within/msg:opacity-100 group-hover/msg:opacity-100 pointer-coarse:opacity-100';
</script>

{#snippet copyBtn(id: string, text: string)}
	<Tip
		text={copied === id ? 'Copied' : 'Copy'}
		aria-label={copied === id ? 'Copied' : 'Copy'}
		onclick={() => copy(id, text)}
		>{#if copied === id}<Check class="text-success" />{:else}<Copy />{/if}</Tip
	>
{/snippet}
{#snippet act(label: string, Icon: typeof Copy, run: () => void)}
	<Tip text={label} aria-label={label} onclick={run}><Icon /></Tip>
{/snippet}

<div class="relative min-h-0 flex-1">
	<div
		bind:this={scroller}
		{onscroll}
		class="scroll-thin absolute inset-0 overflow-y-auto overscroll-contain"
		data-transcript
	>
		<div class="mx-auto flex min-h-full w-full max-w-3xl flex-col gap-4 px-4 py-6 md:px-6">
			{#if canLoadEarlier}
				<Button
					variant="outline"
					size="sm"
					class="mx-auto"
					disabled={loadingEarlier}
					onclick={onEarlier}
				>
					{#if loadingEarlier}<Spinner />{/if}
					{loadingEarlier ? 'Loading…' : 'Load earlier messages'}
				</Button>
			{/if}

			{#if items.length === 0}
				<Empty.Root class="flex-1">
					<Empty.Header>
						<Empty.Media><Brand size={32} word={false} /></Empty.Media>
						<Empty.Description>{empty}</Empty.Description>
					</Empty.Header>
				</Empty.Root>
			{/if}

			{#each parts as item (item.id)}
				{#if item.kind === 'user'}
					<div class="group/msg flex flex-col items-end" data-item="user">
						<!-- The person's own message: no component is a chat bubble, so this is the secondary
						     surface at the card's radius. -->
						<div
							class="max-w-[86%] rounded-xl bg-secondary px-3 py-2 text-secondary-foreground md:max-w-[78%]"
						>
							{#if item.previews?.length}
								<div class="mb-2 flex flex-wrap justify-end gap-2" data-previews>
									{#each item.previews as src, i (i)}
										<img
											{src}
											alt="Sent image {i + 1}"
											class="max-h-40 max-w-full rounded-md border object-contain"
										/>
									{/each}
								</div>
							{/if}
							<p class="text-sm leading-relaxed break-words whitespace-pre-wrap">{item.text}</p>
						</div>
						<div class="mt-1.5 flex gap-2 px-1 text-xs text-muted-foreground empty:hidden">
							{#if item.via !== 'prompt'}<span class="text-warning"
									>{item.via === 'steer' ? 'steering' : 'follow-up'}</span
								>{/if}
							{#if showAgent && item.agent !== 'lead'}<span>{item.agent}</span>{/if}
							{#if item.images}<span>{item.images} image{item.images > 1 ? 's' : ''}</span>{/if}
						</div>
						<div class="-mr-1.5 mt-1 flex items-center gap-0.5 {quiet}" data-actions>
							{@render copyBtn(item.id, item.text)}
							{#if onaction && item.agent === 'lead'}
								{#if item === lastUser && !busy}
									{@render act('Edit and resend', Pencil, () => onaction('edit', item))}
								{/if}
								{@render act('Fork from here', GitFork, () => onaction('fork', item))}
								{@render act('Rewind to here', History, () => onaction('rewind', item))}
							{/if}
						</div>
					</div>
				{:else if item.kind === 'assistant'}
					<!-- One column that cannot grow past the chat: a long code line scrolls inside its block. -->
					<div class="group/msg grid grid-cols-1 gap-3" data-item="assistant">
						{#if showAgent && item.agent !== 'lead'}
							<div class="text-xs text-muted-foreground">{item.agent}</div>
						{/if}
						{#each item.blocks as b (b.index + b.type)}
							{#if b.type === 'thinking'}
								{#if showThinking}
									{@const waiting = item.streaming && !item.blocks.some((x) => x.type === 'text')}
									<Collapsible.Root
										open={isOpen(item.id, item.streaming)}
										onOpenChange={(v) => (thinkingOpen[item.id] = v)}
									>
										<Collapsible.Trigger>
											{#snippet child({ props })}
												<Button
													{...props}
													variant="ghost"
													size="xs"
													class="-ml-2 font-normal text-muted-foreground"
												>
													<ChevronRight
														class="transition-transform duration-200 {isOpen(
															item.id,
															item.streaming
														)
															? 'rotate-90'
															: ''}"
													/>
													<span class={waiting ? 'animate-pulse' : ''}
														>{waiting ? 'Thinking' : 'Thought'}</span
													>
												</Button>
											{/snippet}
										</Collapsible.Trigger>
										<Collapsible.Content>
											<p
												class="mt-1 ml-1 border-l pl-3 text-sm leading-relaxed whitespace-pre-wrap text-muted-foreground"
											>
												{b.text}
											</p>
										</Collapsible.Content>
									</Collapsible.Root>
								{/if}
							{:else}
								<Markdown
									text={b.text}
									streaming={item.streaming && b === item.blocks[item.blocks.length - 1]}
								/>
							{/if}
						{/each}
						{#if item.streaming && item.blocks.length === 0}
							<span class="flex items-center gap-2 text-xs text-muted-foreground"
								><Spinner class="size-3.5" aria-hidden="true" /> Waiting for the model</span
							>
						{/if}
						{#if !item.streaming && item.blocks.some((b) => b.type === 'text')}
							<div
								class="-ml-1.5 flex items-center gap-0.5 {item === lastReply && !busy ? '' : quiet}"
								data-actions
							>
								{@render copyBtn(item.id, replyText(item.blocks))}
								{#if onaction && item.agent === 'lead'}
									{#if item === lastReply && !busy}
										{@render act('Retry', RotateCcw, () => onaction('retry', item))}
									{/if}
									{@render act('Fork from here', GitFork, () => onaction('fork', item))}
								{/if}
								{#if item.tokens}
									<!-- What the turn cost: beside the icons, shown with them on hover or focus. -->
									<span
										class="num ml-2 text-xs text-muted-foreground opacity-0 transition-opacity duration-150 group-focus-within/msg:opacity-100 group-hover/msg:opacity-100 pointer-coarse:hidden"
										data-turn-meta
										>{tokens(item.tokens.input)} in · {tokens(item.tokens.output)} out{item.model
											? ` · ${item.model}`
											: ''}</span
									>
								{/if}
							</div>
						{/if}
					</div>
				{:else if item.kind === 'steps'}
					<ToolSteps tools={item.tools} />
				{:else if item.kind === 'command'}
					<!-- What a slash command answered: shown here only, it is not part of the conversation. -->
					<CodeBlock.Root
						size="sm"
						role="group"
						aria-label="Result of /{item.name}"
						data-item="command"
						data-command={item.name}
					>
						<CodeBlock.Header>
							<CodeBlock.Title class="font-mono font-normal">
								<Terminal /><span>/{item.name.replace(/^skill:/, '')}</span>{#if item.args}<span
										class="text-muted-foreground">{` ${item.args}`}</span
									>{/if}
							</CodeBlock.Title>
							<span class="ml-auto hidden text-xs sm:inline">Only you see this</span>
							<CodeBlock.Actions class="ms-0 sm:ms-0">
								<CodeBlock.CopyButton value={item.text} />
							</CodeBlock.Actions>
						</CodeBlock.Header>
						{#if item.name === 'btw'}
							<div class="px-2.5 pt-1.5 pb-2.5"><Markdown text={item.text} /></div>
						{:else}
							<CodeBlock.Content code={item.text} class="scroll-thin max-h-96" />
						{/if}
					</CodeBlock.Root>
				{:else if item.summary}
					<!-- What happened on the branch that was left, told to the agent: a quiet note, not a message of yours. -->
					<aside
						class="border-l-2 py-1 pl-3 text-xs leading-relaxed text-muted-foreground"
						aria-label="Summary of the branch you left"
						data-item="branch-summary"
					>
						<p class="mb-0.5 font-medium">
							The agent was told what happened on the branch you left
						</p>
						<p class="break-words whitespace-pre-wrap">{item.text}</p>
					</aside>
				{:else}
					<Alert.Root
						variant={item.level === 'error' ? 'destructive' : 'default'}
						role={item.level === 'error' ? 'alert' : 'note'}
						data-level={item.level}
					>
						<Alert.Description class="break-words {item.level === 'warn' ? 'text-warning' : ''}"
							>{item.text}</Alert.Description
						>
					</Alert.Root>
				{/if}
			{/each}

			{#if busy && items[items.length - 1]?.kind !== 'assistant'}
				<div class="flex items-center gap-2 text-sm text-muted-foreground" aria-hidden="true">
					<Spinner /> <span>Working</span>
				</div>
			{/if}
		</div>
	</div>

	{#if !pinned}
		<Button
			variant="secondary"
			size="sm"
			class="absolute bottom-3 left-1/2 z-10 -translate-x-1/2"
			onclick={() => toBottom(true)}
		>
			<ArrowDown /> Latest
		</Button>
	{/if}
	<div class="sr-only" aria-live="polite" aria-atomic="true">{announce}</div>
</div>
