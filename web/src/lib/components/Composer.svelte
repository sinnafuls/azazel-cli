<script lang="ts">
	import type { Session } from '$lib/session/session.svelte';
	import type { Image } from '$lib/protocol';
	import { toImage } from '$lib/images';
	import { viewport } from '$lib/media.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import ArrowUp from '@lucide/svelte/icons/arrow-up';
	import Square from '@lucide/svelte/icons/square';
	import Paperclip from '@lucide/svelte/icons/paperclip';
	import X from '@lucide/svelte/icons/x';

	let { session, canControl }: { session: Session; canControl: boolean } = $props();

	let text = $state('');
	let followUp = $state(false);
	let images = $state<(Image & { preview: string })[]>([]);
	let sending = $state(false);
	let ta = $state<HTMLTextAreaElement>();
	let fileInput = $state<HTMLInputElement>();
	let mentionIdx = $state(0);

	const busy = $derived(session.busy);
	const roster = $derived(session.model.roster.filter((m) => m.id !== 'lead'));

	// `@name` at the caret opens a list of agents, then files.
	const mentionQuery = $derived(/(?:^|\s)@([\w./-]*)$/.exec(text)?.[1] ?? null);
	const mention = $derived.by(() => {
		if (mentionQuery === null) return null;
		const q = mentionQuery.toLowerCase();
		const agents = roster
			.filter((a) => a.id.toLowerCase().includes(q))
			.map((a) => ({ label: a.id, kind: 'agent' as const }));
		const files = (session.files ?? [])
			.filter((f) => f.toLowerCase().includes(q))
			.slice(0, 6)
			.map((f) => ({ label: f, kind: 'file' as const }));
		const all = [...agents, ...files].slice(0, 8);
		return all.length ? { q: mentionQuery, items: all } : null;
	});
	// The files are fetched as soon as an `@` is typed: with no sub-agents the list is empty until then.
	$effect(() => {
		if (mentionQuery !== null) void session.loadFiles().catch(() => {});
	});
	$effect(() => {
		void mention;
		mentionIdx = 0;
	});

	function applyMention(i: number) {
		const item = mention?.items[i];
		if (!item || !mention) return;
		text = text.replace(/@[\w./-]*$/, `@${item.label} `);
		ta?.focus();
	}

	function grow() {
		if (!ta) return;
		ta.style.height = 'auto';
		ta.style.height = `${Math.min(ta.scrollHeight, 200)}px`;
	}
	$effect(() => {
		void text;
		grow();
	});

	async function addFiles(files: Iterable<File>) {
		for (const f of files) {
			if (!f.type.startsWith('image/')) continue;
			try {
				images = [...images, await toImage(f)];
			} catch (e) {
				toasts.push(e instanceof Error ? e.message : 'That image could not be read.', 'error');
			}
		}
	}

	async function submit() {
		const body = text.trim();
		if ((!body && !images.length) || sending || !canControl) return;
		sending = true;
		try {
			const imgs: Image[] = images.map(({ mime, data }) => ({ mime, data }));
			const at = /^@([\w-]+)\s+([\s\S]+)$/.exec(body);
			if (at && roster.some((a) => a.id === at[1] || a.def === at[1])) {
				if (!(await session.mention(at[1], at[2]))) await session.prompt(body, imgs);
			} else if (at && !busy && (await session.mention(at[1], at[2]).catch(() => false))) {
				/* started an agent from a definition */
			} else if (busy && followUp) await session.followUp(body);
			else await session.prompt(body, imgs);
			text = '';
			images = [];
			followUp = false;
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : 'Could not send that.', 'error');
		} finally {
			sending = false;
			ta?.focus();
		}
	}

	function onkeydown(e: KeyboardEvent) {
		if (mention) {
			if (e.key === 'ArrowDown') {
				e.preventDefault();
				mentionIdx = (mentionIdx + 1) % mention.items.length;
			} else if (e.key === 'ArrowUp') {
				e.preventDefault();
				mentionIdx = (mentionIdx - 1 + mention.items.length) % mention.items.length;
			} else if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
				e.preventDefault();
				applyMention(mentionIdx);
			} else if (e.key === 'Escape') text = text.replace(/@[\w./-]*$/, '');
			return;
		}
		// Enter sends on a keyboard; on a touch screen it is a line break and the button sends.
		if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && !viewport.coarse) {
			e.preventDefault();
			void submit();
		}
	}

	const label = $derived(busy ? (followUp ? 'Queue follow-up' : 'Steer') : 'Send');
	const placeholder = $derived(
		!canControl
			? 'View link: you can watch but not send.'
			: busy
				? followUp
					? 'Queue something for when it is done…'
					: 'Steer the agent…'
				: 'Ask azazel to do something…'
	);
</script>

<form
	class="relative shrink-0 pt-1 pb-[max(0.75rem,env(safe-area-inset-bottom))] md:pb-5"
	onsubmit={(e) => {
		e.preventDefault();
		void submit();
	}}
	data-composer
>
	<div class="col">
		{#if mention}
			<ul
				class="float menu mb-2 overflow-hidden rounded-xl p-1"
				role="listbox"
				aria-label="Mentions"
			>
				{#each mention.items as it, i (it.kind + it.label)}
					<li role="option" aria-selected={i === mentionIdx}>
						<button
							type="button"
							class="opt font-mono text-xs {i === mentionIdx ? 'is-active' : ''}"
							onmousedown={(e) => {
								e.preventDefault();
								applyMention(i);
							}}
						>
							<span class="text-faint w-10 shrink-0">{it.kind}</span><span
								class="min-w-0 flex-1 truncate">{it.label}</span
							>{#if i === mentionIdx}<span class="kbd hidden fine:inline-grid" aria-hidden="true"
									>↵</span
								>{/if}
						</button>
					</li>
				{/each}
			</ul>
		{/if}

		{#if images.length}
			<ul class="no-scrollbar mb-2 flex gap-3 overflow-x-auto pt-2.5 pr-2.5 pl-0.5">
				{#each images as im, i (i)}
					<li class="relative shrink-0 animate-enter">
						<img
							src={im.preview}
							alt="Attached {i + 1}"
							class="bg-inset border-border-strong size-16 rounded-lg border object-cover shadow-e1"
						/>
						<button
							type="button"
							class="bg-overlay border-input text-ink-2 hover:text-foreground absolute -top-2 -right-2 grid size-6 place-items-center rounded-full border shadow-e1 transition-colors duration-100 before:absolute before:-inset-2.5"
							aria-label="Remove image {i + 1}"
							onclick={() => (images = images.filter((_, j) => j !== i))}
							><X class="size-3" /></button
						>
					</li>
				{/each}
			</ul>
		{/if}

		<div class="composer flex items-end gap-1 p-1.5">
			<input
				bind:this={fileInput}
				type="file"
				accept="image/*"
				multiple
				hidden
				onchange={(e) => {
					// A copy: clearing the input empties its live list while the images are still being read.
					void addFiles([...(e.currentTarget.files ?? [])]);
					e.currentTarget.value = '';
				}}
			/>
			<button
				type="button"
				class="btn btn-quiet btn-icon"
				aria-label="Attach an image"
				disabled={!canControl}
				onclick={() => fileInput?.click()}><Paperclip class="size-[18px]" /></button
			>
			<textarea
				bind:this={ta}
				bind:value={text}
				{onkeydown}
				onpaste={(e) => {
					const files = [...(e.clipboardData?.files ?? [])].filter((f) =>
						f.type.startsWith('image/')
					);
					if (files.length) {
						e.preventDefault();
						void addFiles(files);
					}
				}}
				rows="1"
				{placeholder}
				aria-label="Message"
				disabled={!canControl}
				enterkeyhint="enter"
				class="placeholder:text-faint max-h-[200px] min-h-11 min-w-0 flex-1 resize-none bg-transparent px-1.5 py-[11px] text-[15px] leading-[22px] outline-none fine:min-h-9 fine:py-[7px]"
			></textarea>
			{#if busy}
				<button
					type="button"
					class="btn btn-danger btn-icon"
					aria-label="Stop the agent"
					disabled={!canControl}
					onclick={() => session.abort().catch((e) => toasts.push(String(e.message ?? e), 'error'))}
					><Square class="size-3.5 fill-current" /></button
				>
			{/if}
			<button
				type="submit"
				class="btn btn-primary btn-icon"
				aria-label={label}
				disabled={!canControl || sending || (!text.trim() && !images.length)}
				><ArrowUp class="size-[18px]" /></button
			>
		</div>

		{#if busy && canControl}
			<div
				class="text-muted-foreground mt-1.5 flex items-center justify-between gap-3 px-1 text-xs"
			>
				<span class="min-w-0 flex-1"
					>The agent is working. Your message {followUp
						? 'waits until it is done'
						: 'reaches it before its next step'}.</span
				>
				<label class="text-ink-2 flex min-h-11 shrink-0 items-center gap-2 fine:min-h-8">
					<input type="checkbox" bind:checked={followUp} class="size-4 accent-(--ember)" /> Follow-up
				</label>
			</div>
		{/if}
	</div>
</form>
