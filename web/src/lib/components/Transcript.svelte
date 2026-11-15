<script lang="ts">
	import type { Item } from '$lib/session/reduce';
	import Markdown from './Markdown.svelte';
	import ToolCard from './ToolCard.svelte';
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
</script>

<div class="relative min-h-0 flex-1">
	<div
		bind:this={scroller}
		{onscroll}
		class="scroll-thin absolute inset-0 overflow-y-auto overscroll-contain"
		data-transcript
	>
		<div class="mx-auto flex w-full max-w-3xl flex-col gap-4 px-4 py-5 md:px-6">
			{#if canLoadEarlier}
				<button
					type="button"
					class="text-muted-foreground hover:text-foreground hover:bg-raised mx-auto min-h-11 rounded-full border px-4 text-xs md:min-h-8"
					disabled={loadingEarlier}
					onclick={onEarlier}
				>
					{loadingEarlier ? 'Loading…' : 'Load earlier messages'}
				</button>
			{/if}

			{#if items.length === 0}
				<p class="text-muted-foreground py-16 text-center text-sm">{empty}</p>
			{/if}

			{#each items as item (item.id)}
				{#if item.kind === 'user'}
					<div
						class="bg-raised ml-auto max-w-[92%] rounded-xl rounded-tr-sm border px-3.5 py-2.5 md:max-w-[80%]"
						data-item="user"
					>
						<div class="text-muted-foreground mb-0.5 flex gap-2 text-[11px]">
							<span
								>{item.via === 'steer'
									? 'Steering'
									: item.via === 'follow_up'
										? 'Queued follow-up'
										: 'You'}</span
							>
							{#if showAgent && item.agent !== 'lead'}<span class="font-mono">{item.agent}</span
								>{/if}
							{#if item.images}<span>{item.images} image{item.images > 1 ? 's' : ''}</span>{/if}
						</div>
						<p class="text-[15px] leading-relaxed break-words whitespace-pre-wrap">{item.text}</p>
					</div>
				{:else if item.kind === 'assistant'}
					<div class="space-y-3" data-item="assistant">
						{#if showAgent && item.agent !== 'lead'}<div
								class="text-muted-foreground font-mono text-[11px]"
							>
								{item.agent}
							</div>{/if}
						{#each item.blocks as b (b.index + b.type)}
							{#if b.type === 'thinking'}
								<div class="text-muted-foreground">
									<button
										type="button"
										class="hover:text-foreground flex min-h-8 items-center gap-1.5 text-xs"
										aria-expanded={thinkingOpen[item.id] ?? item.streaming}
										onclick={() =>
											(thinkingOpen[item.id] = !(thinkingOpen[item.id] ?? item.streaming))}
									>
										<ChevronRight
											class="size-3.5 transition-transform {(thinkingOpen[item.id] ??
											item.streaming)
												? 'rotate-90'
												: ''}"
										/>
										{item.streaming && !item.blocks.some((x) => x.type === 'text')
											? 'Thinking…'
											: 'Thought'}
									</button>
									{#if thinkingOpen[item.id] ?? item.streaming}
										<p
											class="border-input mt-1 ml-1.5 border-l-2 pl-3.5 text-[13px] leading-relaxed whitespace-pre-wrap italic"
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
							<span class="text-muted-foreground text-xs">Waiting for the model…</span>
						{/if}
						{#if item.tokens && !item.streaming}
							<div class="text-muted-foreground num text-[11px]">
								{tokens(item.tokens.input)} in · {tokens(item.tokens.output)} out{item.model
									? ` · ${item.model}`
									: ''}
							</div>
						{/if}
					</div>
				{:else if item.kind === 'tool'}
					<ToolCard tool={item} />
				{:else}
					<div
						class="rounded-lg border px-3 py-2 text-[13px] {item.level === 'error'
							? 'border-destructive/50 text-destructive'
							: item.level === 'warn'
								? 'text-accent-text'
								: 'text-muted-foreground'}"
						role={item.level === 'error' ? 'alert' : undefined}
					>
						{item.text}
					</div>
				{/if}
			{/each}

			{#if busy && items[items.length - 1]?.kind !== 'assistant'}
				<div class="text-muted-foreground flex items-center gap-2 text-xs" aria-hidden="true">
					<span
						class="bg-primary size-1.5 animate-[pulse-dot_1.2s_ease-in-out_infinite] rounded-full"
					></span> Working…
				</div>
			{/if}
		</div>
	</div>

	{#if !pinned}
		<button
			type="button"
			class="bg-card hover:bg-raised absolute right-4 bottom-3 z-10 flex min-h-11 items-center gap-1.5 rounded-full border px-3.5 text-xs shadow-lg md:min-h-8"
			onclick={() => toBottom(true)}
		>
			<ArrowDown class="size-3.5" /> Latest
		</button>
	{/if}
	<div class="sr-only" aria-live="polite" aria-atomic="true">{announce}</div>
</div>
