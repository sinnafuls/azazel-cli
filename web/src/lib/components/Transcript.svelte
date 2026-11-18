<script lang="ts">
	import type { Item, ToolItem } from '$lib/session/reduce';
	import Markdown from './Markdown.svelte';
	import ToolSteps from './ToolSteps.svelte';
	import Brand from './Brand.svelte';
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

	let thinkingOpen = $state<Record<string, boolean>>({});
	const isOpen = (id: string, streaming: boolean) => thinkingOpen[id] ?? streaming;
</script>

{#snippet copyBtn(id: string, text: string)}
	<button
		type="button"
		class="icon-btn"
		aria-label={copied === id ? 'Copied' : 'Copy'}
		title="Copy"
		onclick={() => copy(id, text)}
		>{#if copied === id}<Check class="text-success size-3.5" />{:else}<Copy
				class="size-3.5"
			/>{/if}</button
	>
{/snippet}
{#snippet act(label: string, Icon: typeof Copy, run: () => void)}
	<button type="button" class="icon-btn" aria-label={label} title={label} onclick={run}
		><Icon class="size-3.5" /></button
	>
{/snippet}

<div class="relative min-h-0 flex-1">
	<div
		bind:this={scroller}
		{onscroll}
		class="fade-foot scroll-thin absolute inset-0 overflow-y-auto overscroll-contain"
		data-transcript
	>
		<div class="col flex min-h-full flex-col gap-4 py-6 md:py-8">
			{#if canLoadEarlier}
				<button
					type="button"
					class="btn btn-secondary mx-auto text-xs"
					disabled={loadingEarlier}
					onclick={onEarlier}
				>
					{loadingEarlier ? 'Loading…' : 'Load earlier messages'}
				</button>
			{/if}

			{#if items.length === 0}
				<div class="grid flex-1 place-items-center py-16">
					<div class="flex flex-col items-center gap-5 text-center">
						<Brand size={36} word={false} />
						<p class="text-muted-foreground max-w-[18rem] text-sm leading-relaxed">{empty}</p>
					</div>
				</div>
			{/if}

			{#each parts as item (item.id)}
				{#if item.kind === 'user'}
					<div class="msg flex animate-enter flex-col items-end" data-item="user">
						<div
							class="bg-bubble text-bubble-foreground max-w-[86%] rounded-3xl px-4 py-2.5 md:max-w-[78%]"
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
							<p class="text-base leading-6 break-words whitespace-pre-wrap md:text-[15px]">
								{item.text}
							</p>
						</div>
						<div class="text-faint meta mt-1.5 flex gap-2 px-1 empty:hidden">
							{#if item.via !== 'prompt'}<span class="text-ember-text"
									>{item.via === 'steer' ? 'steering' : 'follow-up'}</span
								>{/if}
							{#if showAgent && item.agent !== 'lead'}<span>{item.agent}</span>{/if}
							{#if item.images}<span>{item.images} image{item.images > 1 ? 's' : ''}</span>{/if}
						</div>
						<div class="msg-actions mt-1 flex gap-0.5" data-actions>
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
					<div class="msg animate-enter space-y-3" data-item="assistant">
						{#if showAgent && item.agent !== 'lead'}<div class="text-muted-foreground meta">
								{item.agent}
							</div>{/if}
						{#each item.blocks as b (b.index + b.type)}
							{#if b.type === 'thinking'}
								<div>
									<button
										type="button"
										class="text-muted-foreground hover:text-foreground -my-1.5 flex min-h-11 items-center gap-1.5 rounded-md text-xs transition-colors duration-100 fine:my-0 fine:min-h-7"
										aria-expanded={isOpen(item.id, item.streaming)}
										onclick={() => (thinkingOpen[item.id] = !isOpen(item.id, item.streaming))}
									>
										<ChevronRight
											class="size-3.5 transition-transform duration-150 {isOpen(
												item.id,
												item.streaming
											)
												? 'rotate-90'
												: ''}"
										/>
										<span
											class={item.streaming && !item.blocks.some((x) => x.type === 'text')
												? 'shimmer'
												: ''}
											>{item.streaming && !item.blocks.some((x) => x.type === 'text')
												? 'Thinking'
												: 'Thought'}</span
										>
									</button>
									{#if isOpen(item.id, item.streaming)}
										<p
											class="text-muted-foreground border-border-strong mt-1 ml-[6.5px] animate-enter border-l pl-4 text-sm leading-[1.65] whitespace-pre-wrap"
										>
											{b.text}
										</p>
									{/if}
								</div>
							{:else}
								<Markdown
									text={b.text}
									streaming={item.streaming && b === item.blocks[item.blocks.length - 1]}
								/>
							{/if}
						{/each}
						{#if item.streaming && item.blocks.length === 0}
							<span class="shimmer text-xs">Waiting for the model</span>
						{/if}
						{#if item.tokens && !item.streaming}
							<div class="text-faint meta">
								{tokens(item.tokens.input)} in · {tokens(item.tokens.output)} out{item.model
									? ` · ${item.model}`
									: ''}
							</div>
						{/if}
						{#if !item.streaming && item.blocks.some((b) => b.type === 'text')}
							<div
								class="msg-actions -ml-1 flex gap-0.5 {item === lastReply && !busy
									? 'is-shown'
									: ''}"
								data-actions
							>
								{@render copyBtn(item.id, replyText(item.blocks))}
								{#if onaction && item.agent === 'lead'}
									{#if item === lastReply && !busy}
										{@render act('Retry', RotateCcw, () => onaction('retry', item))}
									{/if}
									{@render act('Fork from here', GitFork, () => onaction('fork', item))}
								{/if}
							</div>
						{/if}
					</div>
				{:else if item.kind === 'steps'}
					<div class="animate-enter"><ToolSteps tools={item.tools} /></div>
				{:else if item.kind === 'command'}
					<!-- What a slash command answered: shown here only, it is not part of the conversation. -->
					<section
						class="msg bg-card animate-enter overflow-hidden rounded-2xl shadow-[inset_0_0_0_1px_var(--border)]"
						aria-label="Result of /{item.name}"
						data-item="command"
						data-command={item.name}
					>
						<header class="flex h-9 items-center gap-2 border-b pr-1.5 pl-3.5">
							<Terminal class="text-muted-foreground size-3.5 shrink-0" />
							<span class="min-w-0 flex-1 truncate font-mono text-xs"
								><span class="text-foreground">/{item.name.replace(/^skill:/, '')}</span
								>{#if item.args}<span class="text-muted-foreground">{` ${item.args}`}</span
									>{/if}</span
							>
							<span class="text-faint hidden text-xs sm:inline">Only you see this</span>
							{@render copyBtn(item.id, item.text)}
						</header>
						{#if item.name === 'btw'}
							<div class="px-3.5 py-3"><Markdown text={item.text} /></div>
						{:else}
							<pre
								class="scroll-thin text-ink-2 max-h-[28rem] overflow-auto px-3.5 py-3 font-mono text-[13px] leading-[1.6]">{item.text}</pre>
						{/if}
					</section>
				{:else if item.summary}
					<!-- What happened on the branch that was left, told to the agent: a quiet note, not a message of yours. -->
					<aside
						class="text-muted-foreground border-border-strong animate-enter border-l-2 py-1 pl-3.5 text-[13px] leading-relaxed"
						aria-label="Summary of the branch you left"
						data-item="branch-summary"
					>
						<p class="text-faint meta mb-0.5">
							The agent was told what happened on the branch you left
						</p>
						<p class="break-words whitespace-pre-wrap">{item.text}</p>
					</aside>
				{:else}
					<div
						class="rounded-2xl px-4 py-3 text-sm leading-relaxed break-words {item.level === 'error'
							? 'bg-destructive/10 text-destructive'
							: item.level === 'warn'
								? 'bg-ember/10 text-ember-text'
								: 'bg-hover text-muted-foreground'}"
						role={item.level === 'error' ? 'alert' : undefined}
					>
						{item.text}
					</div>
				{/if}
			{/each}

			{#if busy && items[items.length - 1]?.kind !== 'assistant'}
				<div class="flex items-center gap-3 py-1" aria-hidden="true">
					<span class="dots"><span></span><span></span><span></span></span>
					<span class="shimmer text-sm">Working</span>
				</div>
			{/if}
		</div>
	</div>

	{#if !pinned}
		<button
			type="button"
			class="float latest absolute bottom-3 left-1/2 z-10 flex min-h-11 items-center gap-1.5 rounded-full pr-4 pl-3 text-xs font-medium fine:min-h-8"
			onclick={() => toBottom(true)}
		>
			<ArrowDown class="size-3.5" /> Latest
		</button>
	{/if}
	<div class="sr-only" aria-live="polite" aria-atomic="true">{announce}</div>
</div>

<style>
	/* What scrolls fades out just above the message box instead of being cut by a hard edge. */
	.fade-foot {
		mask-image: linear-gradient(to bottom, black calc(100% - 28px), transparent);
	}
	/* Centred with a margin, so the entrance can move it without fighting a translate. */
	.latest {
		translate: -50% 0;
		animation: pop-in var(--dur) var(--ease-out) both;
	}
	/* The actions of a message: there on hover and on focus; always there on touch and on the last reply. */
	.msg-actions {
		opacity: 0;
		transition: opacity var(--dur-fast) var(--ease-out);
	}
	.msg:hover .msg-actions,
	.msg:focus-within .msg-actions,
	.msg-actions.is-shown {
		opacity: 1;
	}
	@media (hover: none), (pointer: coarse) {
		.msg-actions {
			opacity: 1;
		}
	}
</style>
