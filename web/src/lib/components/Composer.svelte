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
	const mention = $derived.by(() => {
		const m = /(?:^|\s)@([\w./-]*)$/.exec(text);
		if (!m) return null;
		const q = m[1].toLowerCase();
		const agents = roster
			.filter((a) => a.id.toLowerCase().includes(q))
			.map((a) => ({ label: a.id, kind: 'agent' as const }));
		const files = (session.files ?? [])
			.filter((f) => f.toLowerCase().includes(q))
			.slice(0, 6)
			.map((f) => ({ label: f, kind: 'file' as const }));
		const all = [...agents, ...files].slice(0, 8);
		return all.length ? { q: m[1], items: all } : null;
	});
	$effect(() => {
		if (mention) void session.loadFiles().catch(() => {});
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
				: 'Message azazel…'
	);
</script>

<form
	class="bg-background border-t px-3 pt-2.5 pb-[max(0.625rem,env(safe-area-inset-bottom))] md:px-6"
	onsubmit={(e) => {
		e.preventDefault();
		void submit();
	}}
	data-composer
>
	<div class="mx-auto max-w-3xl">
		{#if mention}
			<ul
				class="bg-card mb-2 overflow-hidden rounded-lg border shadow-lg"
				role="listbox"
				aria-label="Mentions"
			>
				{#each mention.items as it, i (it.kind + it.label)}
					<li role="option" aria-selected={i === mentionIdx}>
						<button
							type="button"
							class="flex min-h-11 w-full items-center gap-2 px-3 text-left font-mono text-[13px] md:min-h-8 {i ===
							mentionIdx
								? 'bg-raised'
								: ''}"
							onmousedown={(e) => {
								e.preventDefault();
								applyMention(i);
							}}
						>
							<span class="text-muted-foreground w-9 text-[10px] uppercase">{it.kind}</span
							>{it.label}
						</button>
					</li>
				{/each}
			</ul>
		{/if}

		{#if images.length}
			<ul class="mb-2 flex gap-2 overflow-x-auto">
				{#each images as im, i (i)}
					<li class="relative shrink-0">
						<img
							src={im.preview}
							alt="Attached {i + 1}"
							class="size-14 rounded-md border object-cover"
						/>
						<button
							type="button"
							class="bg-card absolute -top-1.5 -right-1.5 grid size-6 place-items-center rounded-full border"
							aria-label="Remove image {i + 1}"
							onclick={() => (images = images.filter((_, j) => j !== i))}
							><X class="size-3" /></button
						>
					</li>
				{/each}
			</ul>
		{/if}

		<div
			class="bg-card focus-within:border-ring flex items-end gap-1 rounded-xl border p-1.5 transition-colors"
		>
			<input
				bind:this={fileInput}
				type="file"
				accept="image/*"
				multiple
				hidden
				onchange={(e) => {
					void addFiles(e.currentTarget.files ?? []);
					e.currentTarget.value = '';
				}}
			/>
			<button
				type="button"
				class="text-muted-foreground hover:text-foreground hover:bg-raised grid size-11 shrink-0 place-items-center rounded-lg disabled:opacity-40 md:size-9"
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
				class="placeholder:text-muted-foreground max-h-[200px] min-h-11 min-w-0 flex-1 resize-none bg-transparent px-1.5 py-[11px] text-[15px] leading-[22px] outline-none md:min-h-9 md:py-[7px]"
			></textarea>
			{#if busy}
				<button
					type="button"
					class="bg-destructive/15 text-destructive hover:bg-destructive/25 grid size-11 shrink-0 place-items-center rounded-lg disabled:opacity-40 md:size-9"
					aria-label="Stop the agent"
					disabled={!canControl}
					onclick={() => session.abort().catch((e) => toasts.push(String(e.message ?? e), 'error'))}
					><Square class="size-4 fill-current" /></button
				>
			{/if}
			<button
				type="submit"
				class="bg-primary text-primary-foreground grid size-11 shrink-0 place-items-center rounded-lg transition-opacity disabled:opacity-35 md:size-9"
				aria-label={label}
				disabled={!canControl || sending || (!text.trim() && !images.length)}
				><ArrowUp class="size-[18px]" /></button
			>
		</div>

		{#if busy && canControl}
			<div class="text-muted-foreground mt-1.5 flex items-center justify-between px-1 text-xs">
				<span
					>The agent is working. Your message {followUp
						? 'waits until it is done'
						: 'reaches it before its next step'}.</span
				>
				<label class="flex min-h-8 items-center gap-1.5">
					<input type="checkbox" bind:checked={followUp} class="accent-primary size-4" /> Follow-up
				</label>
			</div>
		{/if}
	</div>
</form>
