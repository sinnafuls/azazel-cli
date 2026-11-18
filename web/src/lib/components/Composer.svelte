<script lang="ts">
	// The message box. `/` at the start opens the command menu, `@` a list of agents and files.
	// While the agent works a message either steers it or waits in the queue, and Stop is held.
	import type { Session } from '$lib/session/session.svelte';
	import type { Image } from '$lib/protocol';
	import { MODES, THINKING_LEVELS } from '$lib/protocol';
	import { toImage } from '$lib/images';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { isMac } from '$lib/shortcuts';
	import {
		filterSlash,
		groupSlash,
		needsArgs,
		parseSlash,
		slashEntries,
		slashLabel,
		slashQuery,
		unknownSlash,
		type SlashEntry
	} from '$lib/slash';
	import ArrowUp from '@lucide/svelte/icons/arrow-up';
	import Square from '@lucide/svelte/icons/square';
	import Plus from '@lucide/svelte/icons/plus';
	import X from '@lucide/svelte/icons/x';
	import Pencil from '@lucide/svelte/icons/pencil';
	import Clock from '@lucide/svelte/icons/clock';

	let { session, canControl }: { session: Session; canControl: boolean } = $props();

	const HOLD_MS = 600;
	const mac = isMac(typeof navigator === 'undefined' ? '' : navigator.platform);

	let text = $state('');
	let followUp = $state(false);
	let images = $state<(Image & { preview: string })[]>([]);
	let sending = $state(false);
	let ta = $state<HTMLTextAreaElement>();
	let fileInput = $state<HTMLInputElement>();
	let mentionIdx = $state(0);

	const busy = $derived(session.busy);
	const roster = $derived(session.model.roster.filter((m) => m.id !== 'lead'));
	const queued = $derived(session.model.queue);

	// --- `/`: commands and skills -----------------------------------------------------------

	const entries = $derived(slashEntries(session.commands));
	const query = $derived(canControl ? slashQuery(text) : null);
	/** Escape closes the menu until the text stops being this very query. */
	let dismissed = $state<string | null>(null);
	const matches = $derived(query === null || dismissed === text ? [] : filterSlash(entries, query));
	const groups = $derived(groupSlash(matches));
	const slashOpen = $derived(query !== null && dismissed !== text);
	let slashIdx = $state(0);
	let slashList = $state<HTMLDivElement>();
	$effect(() => {
		if (query !== null) void session.loadCommands().catch(() => {});
	});
	$effect(() => {
		void query;
		slashIdx = 0;
	});
	$effect(() => {
		void slashIdx;
		slashList?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
	});

	/** Enter runs a command that needs nothing more; anything else is completed for its arguments. */
	function pick(i: number, complete = false) {
		const e = matches[i];
		if (!e) return;
		if (complete || e.args) {
			text = `/${e.name} `;
			ta?.focus();
		} else void runSlash(e, '');
	}

	const fail = (e: unknown) => toasts.push(e instanceof Error ? e.message : String(e), 'error');

	async function runSlash(e: SlashEntry, args: string) {
		if (needsArgs(e) && !args) {
			text = `/${e.name} `;
			toasts.push(`/${e.name} needs ${e.args}.`, 'warn');
			ta?.focus();
			return;
		}
		sending = true;
		text = '';
		try {
			if (!e.builtin) {
				const r = await session.runCommand(e.name, args);
				if (r.outcome === 'done') toasts.push(`Ran ${slashLabel(e)}.`, 'success', 2500);
				return;
			}
			switch (e.name) {
				case 'compact':
					await session.compact(args);
					toasts.push('Compacting the conversation.', 'info', 2500);
					break;
				case 'retry':
					await session.retry();
					break;
				case 'name':
					await session.setName(args);
					void sessions.refresh();
					break;
				case 'fork':
					await session.fork();
					toasts.push('Forked: this session continues in a copy.', 'success');
					void sessions.refresh();
					break;
				case 'new':
					await session.newSession();
					void sessions.refresh();
					break;
				case 'export': {
					const format = (['markdown', 'html', 'jsonl'] as const).find((f) => f === args);
					if (args && !format) throw new Error('Export as markdown, html or jsonl.');
					const r = await session.exportTo(format);
					toasts.push(`Exported to ${r.path}`, 'success', 9000);
					break;
				}
				case 'btw':
					await session.btw(args);
					break;
				case 'model':
					if (args) await session.setModel(args);
					else ui.picker = 'model';
					break;
				case 'thinking':
					if (!args) ui.picker = 'thinking';
					else if (!(THINKING_LEVELS as readonly string[]).includes(args))
						throw new Error(`Thinking is one of: ${THINKING_LEVELS.join(', ')}.`);
					else await session.setThinking(args);
					break;
				case 'mode':
					// Bypass is confirmed in the picker, never from a typed line.
					if (!args || args === 'bypass') ui.picker = 'mode';
					else if (!(MODES as readonly string[]).includes(args))
						throw new Error(
							`The mode is one of: ${MODES.filter((m) => m !== 'bypass').join(', ')}.`
						);
					else await session.setMode(args);
					break;
				case 'rewind':
					ui.rewind = {};
					break;
			}
		} catch (err) {
			fail(err);
		} finally {
			sending = false;
			ta?.focus();
		}
	}

	// --- `@`: agents and files ----------------------------------------------------------------

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

	// --- edit and resend ----------------------------------------------------------------------

	let editing = $state<{ entryId: string | null } | null>(null);
	$effect(() => {
		const e = ui.edit;
		if (!e) return;
		ui.edit = null;
		editing = { entryId: e.entryId };
		text = e.text;
		ta?.focus();
	});
	function cancelEdit() {
		editing = null;
		text = '';
	}

	async function submit(queue = followUp) {
		const body = text.trim();
		if ((!body && !images.length) || sending || !canControl) return;
		// A `/word` typed before the command list arrived waits for it, or it would reach the model as text.
		if (!images.length && /^\/[\w:.-]+(\s|$)/.test(body) && !session.commands.length) {
			await session.loadCommands().catch(() => {});
			if (!session.commands.length) {
				toasts.push('The commands could not be loaded yet. Try again in a moment.', 'warn');
				return;
			}
		}
		const slash = images.length ? null : parseSlash(body, slashEntries(session.commands));
		if (slash) return void runSlash(slash.entry, slash.args);
		// A lone `/word` that is no command stays in the box: the model is not asked to guess at it.
		const unknown = images.length ? null : unknownSlash(body, slashEntries(session.commands));
		if (unknown) {
			toasts.push(
				`/${unknown} isn't a command this browser can run. With full access it can run the terminal's own commands.`,
				'warn'
			);
			return;
		}
		sending = true;
		try {
			const imgs: Image[] = images.map(({ mime, data }) => ({ mime, data }));
			const at = /^@([\w-]+)\s+([\s\S]+)$/.exec(body);
			if (editing && !busy) {
				// The old prompt and what followed leave the conversation, then the new one is sent.
				if (editing.entryId) await session.backBefore(editing.entryId);
				await session.promptNow(body);
			} else if (at && roster.some((a) => a.id === at[1] || a.def === at[1])) {
				if (!(await session.mention(at[1], at[2]))) await session.prompt(body, imgs);
			} else if (at && !busy && (await session.mention(at[1], at[2]).catch(() => false))) {
				/* started an agent from a definition */
			} else if (busy && queue) await session.followUp(body);
			else await session.prompt(body, imgs);
			text = '';
			images = [];
			followUp = false;
			editing = null;
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : 'Could not send that.', 'error');
		} finally {
			sending = false;
			ta?.focus();
		}
	}

	/** Whether a bare Enter sends: the choice of Settings > General, or by the kind of keyboard. */
	const enterSends = $derived(app.enter === 'send' || (app.enter === 'auto' && !viewport.coarse));

	function onkeydown(e: KeyboardEvent) {
		if (slashOpen && matches.length) {
			if (e.key === 'ArrowDown') slashIdx = (slashIdx + 1) % matches.length;
			else if (e.key === 'ArrowUp') slashIdx = (slashIdx - 1 + matches.length) % matches.length;
			else if (e.key === 'Tab') pick(slashIdx, true);
			else if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) pick(slashIdx);
			else if (e.key === 'Escape') dismissed = text;
			else return;
			e.preventDefault();
			return;
		}
		if (slashOpen && e.key === 'Escape') {
			e.preventDefault();
			dismissed = text;
			return;
		}
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
			} else if (e.key === 'Escape') {
				e.preventDefault();
				text = text.replace(/@[\w./-]*$/, '');
			}
			return;
		}
		if (e.key === 'Escape' && editing) {
			e.preventDefault();
			cancelEdit();
			return;
		}
		if (e.key !== 'Enter' || e.shiftKey || e.isComposing) return;
		// Command (Control) + Enter always sends; while the agent works it queues instead of steering.
		if (mac ? e.metaKey : e.ctrlKey) {
			e.preventDefault();
			void submit(busy ? true : followUp);
		} else if (enterSends) {
			e.preventDefault();
			void submit();
		}
	}

	// --- stop: held, so that a stray tap does not throw the run away --------------------------

	let holding = $state(false);
	let holdTimer: ReturnType<typeof setTimeout> | null = null;
	let hint = $state(false);
	const stop = () => session.abort().catch(fail);
	function holdStart(e: PointerEvent) {
		if (e.button !== 0) return;
		holding = true;
		hint = false;
		holdTimer = setTimeout(() => {
			holding = false;
			holdTimer = null;
			void stop();
		}, HOLD_MS);
	}
	function holdEnd() {
		if (!holdTimer) return;
		clearTimeout(holdTimer);
		holdTimer = null;
		holding = false;
		hint = true;
		setTimeout(() => (hint = false), 2200);
	}

	const label = $derived(busy ? (followUp ? 'Queue follow-up' : 'Steer') : 'Send');
	const placeholder = $derived(
		!canControl
			? 'View link: you can watch but not send.'
			: busy
				? followUp
					? 'Queue something for when it is done…'
					: 'Steer the agent…'
				: 'Ask azazel to do something, or type / for commands'
	);
	const activeId = $derived(
		slashOpen && matches[slashIdx] ? `slash-${matches[slashIdx].name}` : undefined
	);
</script>

<form
	class="relative shrink-0 pt-1 pb-[max(0.75rem,env(safe-area-inset-bottom))] md:pb-4"
	onsubmit={(e) => {
		e.preventDefault();
		void submit();
	}}
	data-composer
>
	<div class="col relative">
		{#if slashOpen}
			<div
				bind:this={slashList}
				id="slash-menu"
				class="pop scroll-thin absolute inset-x-4 bottom-full z-20 mb-2 max-h-[min(20rem,48dvh)] overflow-y-auto md:inset-x-8"
				role="listbox"
				aria-label="Commands"
				data-slash-menu
			>
				{#each groups as g (g.group)}
					<div role="group" aria-labelledby="slash-g-{g.group}">
						<div class="pop-label" id="slash-g-{g.group}">{g.group}</div>
						{#each g.items as e, j (e.name)}
							{@const i = g.start + j}
							<!-- svelte-ignore a11y_click_events_have_key_events -->
							<div
								id="slash-{e.name}"
								role="option"
								tabindex="-1"
								aria-selected={i === slashIdx}
								class="pop-item cursor-pointer"
								onpointermove={() => (slashIdx = i)}
								onmousedown={(ev) => ev.preventDefault()}
								onclick={() => pick(i)}
							>
								<span class="shrink-0 font-mono text-[13px]"
									><span class="text-foreground">{slashLabel(e)}</span>{#if e.args}<span
											class="text-faint">{` ${e.args}`}</span
										>{/if}</span
								>
								<span class="text-muted-foreground min-w-0 flex-1 truncate text-right"
									>{e.description}</span
								>
							</div>
						{/each}
					</div>
				{:else}
					<p class="text-muted-foreground px-2 py-3" role="presentation">
						{session.commands.length || query
							? `No command matches /${query}. Enter sends it as a message.`
							: 'Loading commands…'}
					</p>
				{/each}
			</div>
		{/if}

		{#if mention}
			<ul class="pop mb-2 overflow-hidden" role="listbox" aria-label="Mentions">
				{#each mention.items as it, i (it.kind + it.label)}
					<li role="option" aria-selected={i === mentionIdx}>
						<button
							type="button"
							class="pop-item font-mono"
							data-active={i === mentionIdx}
							onmousedown={(e) => {
								e.preventDefault();
								applyMention(i);
							}}
						>
							<span class="text-faint w-10 shrink-0 text-xs">{it.kind}</span><span
								class="min-w-0 flex-1 truncate">{it.label}</span
							>{#if i === mentionIdx}<span class="kbd hidden fine:inline-grid" aria-hidden="true"
									>↵</span
								>{/if}
						</button>
					</li>
				{/each}
			</ul>
		{/if}

		{#if queued.length}
			<ul class="mb-2 space-y-1" aria-label="Queued messages" data-queue>
				{#each queued as q, i (i)}
					<li
						class="bg-card text-ink-2 flex items-center gap-2 rounded-xl px-3 py-1.5 text-[13px] shadow-[inset_0_0_0_1px_var(--border)]"
					>
						<Clock class="text-muted-foreground size-3.5 shrink-0" />
						<span class="min-w-0 flex-1 truncate">{q.text}</span>
						<span class="text-muted-foreground shrink-0 text-xs"
							>{q.kind === 'steer' ? 'next step' : 'when it is done'}</span
						>
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
							class="bg-inset border-border-strong size-16 rounded-xl border object-cover"
						/>
						<button
							type="button"
							class="bg-overlay border-input text-ink-2 hover:text-foreground absolute -top-2 -right-2 grid size-6 place-items-center rounded-full border transition-colors duration-100 before:absolute before:-inset-2.5"
							aria-label="Remove image {i + 1}"
							onclick={() => (images = images.filter((_, j) => j !== i))}
							><X class="size-3" /></button
						>
					</li>
				{/each}
			</ul>
		{/if}

		<div class="composer">
			{#if editing}
				<div
					class="text-muted-foreground flex items-center gap-2 border-b py-1.5 pr-2 pl-4 text-xs"
					data-editing
				>
					<Pencil class="size-3.5 shrink-0" />
					<span class="min-w-0 flex-1"
						>Editing your last prompt. Sending replaces it and what followed.</span
					>
					<button type="button" class="icon-btn" aria-label="Cancel editing" onclick={cancelEdit}
						><X class="size-3.5" /></button
					>
				</div>
			{/if}
			<div class="flex items-end gap-1 p-1.5">
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
					class="round quiet"
					aria-label="Attach an image"
					title="Attach an image"
					disabled={!canControl}
					onclick={() => fileInput?.click()}><Plus class="size-[18px]" /></button
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
					aria-haspopup="listbox"
					aria-expanded={slashOpen}
					aria-controls={slashOpen ? 'slash-menu' : undefined}
					aria-activedescendant={activeId}
					aria-autocomplete="list"
					role="combobox"
					disabled={!canControl}
					enterkeyhint={enterSends ? 'send' : 'enter'}
					class="placeholder:text-faint max-h-[200px] min-h-9 min-w-0 flex-1 resize-none bg-transparent px-1.5 py-[7px] text-base leading-[22px] outline-none md:text-[15px]"
				></textarea>
				{#if busy}
					<button
						type="button"
						class="round stop"
						class:holding
						style="--hold: {HOLD_MS}ms"
						aria-label="Stop the agent"
						title="Hold to stop (or press Esc)"
						disabled={!canControl}
						onpointerdown={holdStart}
						onpointerup={holdEnd}
						onpointerleave={holdEnd}
						onpointercancel={holdEnd}
						oncontextmenu={(e) => e.preventDefault()}
						onclick={(e) => {
							// A keyboard or a screen reader activates it at once; a pointer holds.
							if (e.detail === 0) void stop();
						}}><Square class="size-3 fill-current" /></button
					>
				{/if}
				<button
					type="submit"
					class="round send"
					aria-label={label}
					title={label}
					disabled={!canControl || sending || (!text.trim() && !images.length)}
					><ArrowUp class="size-[18px]" /></button
				>
			</div>
		</div>

		{#if busy && canControl}
			<div
				class="text-muted-foreground mt-1.5 flex min-h-7 items-center justify-between gap-3 px-2 text-xs"
			>
				<span class="min-w-0 flex-1 truncate" role={hint ? 'status' : undefined}
					>{hint
						? 'Hold the button to stop, or press Esc.'
						: followUp
							? 'Your message waits until the agent is done.'
							: 'Your message reaches the agent before its next step.'}</span
				>
				<div class="mini-seg" role="radiogroup" aria-label="When to deliver">
					<button
						type="button"
						role="radio"
						aria-checked={!followUp}
						onclick={() => (followUp = false)}>Steer</button
					>
					<button
						type="button"
						role="radio"
						aria-checked={followUp}
						aria-label="Follow-up"
						onclick={() => (followUp = true)}>Queue</button
					>
				</div>
			</div>
		{/if}
	</div>
</form>

<style>
	/* The round buttons inside the box: 32px with a fine pointer, 40 on touch. */
	.round {
		position: relative;
		display: grid;
		flex-shrink: 0;
		place-items: center;
		width: 2.25rem;
		height: 2.25rem;
		border-radius: 999px;
		transition:
			background-color var(--dur-fast) var(--ease-out),
			color var(--dur-fast) var(--ease-out),
			opacity var(--dur-fast) var(--ease-out),
			transform var(--dur-press) var(--ease-out);
	}
	.round:active:not(:disabled) {
		transform: scale(0.94);
	}
	.round:disabled {
		opacity: 0.4;
	}
	.quiet {
		color: var(--muted-foreground);
	}
	.send {
		background: var(--primary);
		color: var(--primary-foreground);
	}
	.stop {
		color: var(--foreground);
		box-shadow: inset 0 0 0 1.5px var(--input);
		touch-action: none;
		user-select: none;
	}
	/* The ring that fills while the button is held. */
	.stop::before {
		content: '';
		position: absolute;
		inset: 0;
		border-radius: inherit;
		background: conic-gradient(var(--destructive) var(--p, 0%), transparent 0);
		mask: radial-gradient(farthest-side, transparent calc(100% - 2.5px), black calc(100% - 2px));
		opacity: 0;
	}
	.stop.holding::before {
		opacity: 1;
		animation: hold-ring var(--hold) linear both;
	}
	.stop.holding {
		color: var(--destructive);
	}
	@property --p {
		syntax: '<percentage>';
		inherits: false;
		initial-value: 0%;
	}
	@keyframes hold-ring {
		to {
			--p: 100%;
		}
	}
	@media (hover: hover) {
		.quiet:hover:not(:disabled),
		.stop:hover:not(:disabled) {
			background: var(--hover);
			color: var(--foreground);
		}
		.send:hover:not(:disabled) {
			background: var(--primary-hover);
		}
	}
	@media (pointer: coarse) {
		.round {
			width: 2.5rem;
			height: 2.5rem;
		}
		.round::after {
			content: '';
			position: absolute;
			inset: -0.125rem;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.stop.holding::before {
			animation: none;
			--p: 100%;
		}
	}

	.mini-seg {
		display: flex;
		flex-shrink: 0;
		gap: 2px;
		padding: 2px;
		border-radius: 999px;
		box-shadow: inset 0 0 0 1px var(--border-strong);
	}
	.mini-seg > button {
		min-height: 1.5rem;
		padding: 0 0.625rem;
		border-radius: 999px;
		font-weight: 500;
		color: var(--muted-foreground);
		transition:
			background-color var(--dur-fast) var(--ease-out),
			color var(--dur-fast) var(--ease-out);
	}
	.mini-seg > button[aria-checked='true'] {
		background: var(--active);
		color: var(--foreground);
	}
	@media (pointer: coarse) {
		.mini-seg > button {
			min-height: 2.25rem;
			padding: 0 0.875rem;
		}
	}
</style>
