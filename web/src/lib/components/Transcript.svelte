<script lang="ts">
	import type { Item } from '$lib/session/reduce';
	import Markdown from './Markdown.svelte';
	import ToolCard from './ToolCard.svelte';
	import Brand from './Brand.svelte';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import ArrowDown from '@lucide/svelte/icons/arrow-down';
	import { tokens } from '$lib/format';
	import { tick } from 'svelte';

	let {
		items,
		busy = false,
		canLoadEarlier = false,
		loadingEarlier = false,
		onEarlier,
		showAgent = false,
		empty = 'Nothing has been said in this session yet.'
	}: {
		items: Item[];
		busy?: boolean;
		canLoadEarlier?: boolean;
		loadingEarlier?: boolean;
		onEarlier?: () => void;
		showAgent?: boolean;
		empty?: string;
	} = $props();

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

<div class="relative min-h-0 flex-1">
	<div
		bind:this={scroller}
		{onscroll}
		class="scroll-thin absolute inset-0 overflow-y-auto overscroll-contain"
		data-transcript
	>
		<div class="col flex min-h-full flex-col gap-5 py-6 md:py-8">
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
				<div class="relative grid flex-1 place-items-center py-16">
					<div class="dotgrid" aria-hidden="true"></div>
					<div class="relative flex flex-col items-center gap-5 text-center">
						<Brand size={36} word={false} />
						<p class="text-muted-foreground max-w-[18rem] text-sm leading-relaxed">{empty}</p>
					</div>
				</div>
			{/if}

			{#each items as item (item.id)}
				{#if item.kind === 'user'}
					<div class="flex animate-enter flex-col items-end" data-item="user">
						<div class="surface max-w-[88%] rounded-xl px-3.5 py-2.5 md:max-w-[80%]">
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
							<p class="text-base break-words whitespace-pre-wrap">{item.text}</p>
						</div>
						<div class="text-faint meta mt-1.5 flex gap-2 px-1 empty:hidden">
							{#if item.via !== 'prompt'}<span class="text-accent-text"
									>{item.via === 'steer' ? 'steering' : 'queued follow-up'}</span
								>{/if}
							{#if showAgent && item.agent !== 'lead'}<span>{item.agent}</span>{/if}
							{#if item.images}<span>{item.images} image{item.images > 1 ? 's' : ''}</span>{/if}
						</div>
					</div>
				{:else if item.kind === 'assistant'}
					<div class="animate-enter space-y-3" data-item="assistant">
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
					</div>
				{:else if item.kind === 'tool'}
					<div class="animate-enter"><ToolCard tool={item} /></div>
				{:else}
					<div
						class="border-l-2 py-0.5 pl-3 text-sm leading-relaxed break-words {item.level ===
						'error'
							? 'border-destructive text-destructive'
							: item.level === 'warn'
								? 'border-ember text-accent-text'
								: 'border-border-strong text-muted-foreground'}"
						role={item.level === 'error' ? 'alert' : undefined}
					>
						{item.text}
					</div>
				{/if}
			{/each}

			{#if busy && items[items.length - 1]?.kind !== 'assistant'}
				<div class="text-muted-foreground flex items-center gap-2.5 text-xs" aria-hidden="true">
					<span class="relative grid size-2.5 place-items-center">
						<span class="bg-info absolute inset-0 animate-ping-soft rounded-full"></span>
						<span class="bg-info size-2 rounded-full"></span>
					</span>
					<span class="shimmer">Working</span>
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
	/* Centred with a margin, so the entrance can move it without fighting a translate. */
	.latest {
		translate: -50% 0;
		animation: pop-in var(--dur) var(--ease-out) both;
	}
</style>
