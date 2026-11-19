<script lang="ts">
	// The message box, an InputGroup: the text on top, and under it the attach button, the session's
	// model, thinking and mode, then Stop and Send. `/` at the start opens the command menu, `@` a list of agents and files.
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
	import { resume } from '$lib/resume.svelte';
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
	import { cn } from '$lib/utils';
	import { Button } from '$lib/components/ui/button';
	import * as InputGroup from '$lib/components/ui/input-group';
	import * as Command from '$lib/components/ui/command';
	import * as Item from '$lib/components/ui/item';
	import { Kbd } from '$lib/components/ui/kbd';
	import ArrowUp from '@lucide/svelte/icons/arrow-up';
	import Square from '@lucide/svelte/icons/square';
	import StatusBar from './StatusBar.svelte';
	import Segmented from './Segmented.svelte';
	import Tip from './Tip.svelte';
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
	let ta = $state<HTMLTextAreaElement | null>(null);
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
	$effect(() => {
		if (query !== null) void session.loadCommands().catch(() => {});
	});
	$effect(() => {
		void query;
		slashIdx = 0;
	});
	$effect(() => {
		const name = matches[slashIdx]?.name;
		if (name) document.getElementById(`slash-${name}`)?.scrollIntoView({ block: 'nearest' });
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
				case 'handoff': {
					// Writing the brief takes a while: say so until it is done, and let it be cancelled.
					const wait = toasts.loading('Writing the hand-off brief…', {
						label: 'Cancel',
						onClick: () => void session.abort().catch(fail)
					});
					try {
						await session.handoff(args);
					} finally {
						toasts.dismiss(wait);
					}
					toasts.push('Handed off: this is a new session that starts from the brief.', 'success');
					void sessions.refresh();
					break;
				}
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
				case 'profile':
				case 'preset': {
					const save = /^save(?:\s+(\S+))?$/.exec(args);
					if (!args) ui.picker = 'profile';
					else if (!save) await session.setProfile(args);
					else if (!save[1]) throw new Error('Name the profile to save: /profile save <name>.');
					else {
						await session.saveProfile(save[1]);
						toasts.push(`Saved the roles as the profile ${save[1]}.`, 'success');
					}
					break;
				}
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
				case 'resume':
					if (!resume.available)
						throw new Error('Continuing an earlier session needs a control link.');
					resume.show();
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

	const sendHint = $derived(
		enterSends
			? 'Enter sends, Shift+Enter adds a line'
			: `Enter adds a line, ${mac ? '⌘' : 'Ctrl'}+Enter sends`
	);
	const label = $derived(busy ? (followUp ? 'Queue follow-up' : 'Steer') : 'Send');
	const placeholder = $derived(
		!canControl
			? 'View link: you can watch but not send'
			: busy
				? followUp
					? 'Queue something for when it is done…'
					: 'Steer the agent…'
				: 'Ask azazel to do something, or type / for commands'
	);
	const activeId = $derived(
		slashOpen && matches[slashIdx] ? `slash-${matches[slashIdx].name}` : undefined
	);
	// The `/` and `@` lists are a Command above the box. Not a Popover: the caret stays in the
	// message, which is the combobox that owns the list, and a popover would take the focus.
	const floating =
		'absolute inset-x-0 bottom-full z-20 mb-2 h-auto! rounded-lg! shadow-md ring-1 ring-foreground/10';
</script>

<form
	class="relative shrink-0 pt-1 pb-[max(0.75rem,env(safe-area-inset-bottom))] md:pb-4"
	onsubmit={(e) => {
		e.preventDefault();
		void submit();
	}}
	data-composer
>
	<div class="mx-auto w-full max-w-3xl px-4 md:px-6">
		{#if queued.length}
			<Item.Group class="mb-2 gap-1" aria-label="Queued messages" data-queue>
				{#each queued as q, i (i)}
					<Item.Root variant="outline" size="xs" class="flex-nowrap" role="listitem">
						<Item.Media variant="icon" class="text-muted-foreground"><Clock /></Item.Media>
						<Item.Content class="min-w-0">
							<Item.Title class="block w-full truncate font-normal">{q.text}</Item.Title>
						</Item.Content>
						<Item.Actions class="shrink-0 text-xs text-muted-foreground"
							>{q.kind === 'steer' ? 'next step' : 'when it is done'}</Item.Actions
						>
					</Item.Root>
				{/each}
			</Item.Group>
		{/if}

		{#if images.length}
			<ul class="mb-2 flex gap-3 overflow-x-auto pt-2 pr-2">
				{#each images as im, i (i)}
					<li class="relative shrink-0">
						<img
							src={im.preview}
							alt="Attached {i + 1}"
							class="size-16 rounded-lg border object-cover"
						/>
						<Button
							variant="secondary"
							size="icon-xs"
							class="absolute -top-2 -right-2 ring-1 ring-foreground/10"
							aria-label="Remove image {i + 1}"
							onclick={() => (images = images.filter((_, j) => j !== i))}><X /></Button
						>
					</li>
				{/each}
			</ul>
		{/if}

		<div class="relative">
			{#if slashOpen}
				<Command.Root
					shouldFilter={false}
					value={matches[slashIdx]?.name ?? ''}
					onValueChange={(v) => {
						const i = matches.findIndex((m) => m.name === v);
						if (i >= 0) slashIdx = i;
					}}
					class={floating}
					onmousedown={(e) => e.preventDefault()}
					data-slash-menu
				>
					<Command.List
						id="slash-menu"
						aria-label="Commands"
						class="scroll-thin max-h-[min(20rem,48dvh)]"
					>
						{#each groups as g (g.group)}
							<Command.Group heading={g.group}>
								{#each g.items as e, j (e.name)}
									<Command.Item
										id="slash-{e.name}"
										value={e.name}
										onSelect={() => pick(g.start + j)}
									>
										<span class="shrink-0 font-mono text-xs"
											>{slashLabel(e)}{#if e.args}<span class="text-muted-foreground"
													>{` ${e.args}`}</span
												>{/if}</span
										>
										<span class="min-w-0 flex-1 truncate text-right text-muted-foreground"
											>{e.description}</span
										>
									</Command.Item>
								{/each}
							</Command.Group>
						{:else}
							<!-- Not Command.Empty: the list is filtered here, so the component cannot tell. -->
							<p class="px-2 py-3 text-sm text-muted-foreground" role="presentation">
								{session.commands.length || query
									? `No command matches /${query}. Enter sends it as a message.`
									: 'Loading commands…'}
							</p>
						{/each}
					</Command.List>
				</Command.Root>
			{:else if mention}
				<Command.Root
					shouldFilter={false}
					value={mention.items[mentionIdx]
						? `${mention.items[mentionIdx].kind}:${mention.items[mentionIdx].label}`
						: ''}
					onValueChange={(v) => {
						const i = mention.items.findIndex((m) => `${m.kind}:${m.label}` === v);
						if (i >= 0) mentionIdx = i;
					}}
					class={floating}
					onmousedown={(e) => e.preventDefault()}
				>
					<Command.List aria-label="Mentions">
						{#each mention.items as it, i (it.kind + it.label)}
							<Command.Item value="{it.kind}:{it.label}" onSelect={() => applyMention(i)}>
								<span class="w-10 shrink-0 text-xs text-muted-foreground">{it.kind}</span>
								<span class="min-w-0 flex-1 truncate font-mono text-xs">{it.label}</span>
								{#if i === mentionIdx}
									<Kbd class="pointer-coarse:hidden" aria-hidden="true">↵</Kbd>
								{/if}
							</Command.Item>
						{/each}
					</Command.List>
				</Command.Root>
			{/if}

			<InputGroup.Root>
				{#if editing}
					<InputGroup.Addon align="block-start" class="border-b text-xs font-normal" data-editing>
						<Pencil class="size-3.5" />
						<span class="min-w-0 flex-1"
							>Editing your last prompt. Sending replaces it and what followed.</span
						>
						<InputGroup.Button size="icon-xs" aria-label="Cancel editing" onclick={cancelEdit}
							><X /></InputGroup.Button
						>
					</InputGroup.Addon>
				{/if}
				<InputGroup.Textarea
					bind:ref={ta}
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
					rows={1}
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
					class="scroll-thin max-h-50"
				/>
				<InputGroup.Addon align="block-end" class="gap-1">
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
					<Tip
						text="Attach an image"
						class="-ml-1 shrink-0"
						aria-label="Attach an image"
						disabled={!canControl}
						onclick={() => fileInput?.click()}><Plus /></Tip
					>
					<div class="min-w-0 flex-1"><StatusBar {session} {canControl} /></div>
					{#if busy}
						<!-- Held: the wash fills the button while the pointer is down, then the run stops. -->
						<Tip
							text="Hold to stop (or press Esc)"
							variant="outline"
							class={cn(
								'relative shrink-0 touch-none overflow-hidden select-none',
								holding && 'text-destructive'
							)}
							aria-label="Stop the agent"
							data-holding={holding ? '' : undefined}
							disabled={!canControl}
							onpointerdown={holdStart}
							onpointerup={holdEnd}
							onpointerleave={holdEnd}
							onpointercancel={holdEnd}
							oncontextmenu={(e) => e.preventDefault()}
							onclick={(e) => {
								// A keyboard or a screen reader activates it at once; a pointer holds.
								if (e.detail === 0) void stop();
							}}
						>
							<span
								class="pointer-events-none absolute inset-0 origin-bottom bg-destructive/25 transition-transform ease-linear {holding
									? 'scale-y-100'
									: 'scale-y-0'}"
								style="transition-duration: {holding ? HOLD_MS : 120}ms"
								aria-hidden="true"
							></span>
							<Square class="relative size-3 fill-current" />
						</Tip>
					{/if}
					<Tip
						type="submit"
						text={label}
						variant="default"
						class="-mr-1 shrink-0"
						aria-label={label}
						disabled={!canControl || sending || (!text.trim() && !images.length)}><ArrowUp /></Tip
					>
				</InputGroup.Addon>
			</InputGroup.Root>
		</div>

		<!-- A row of fixed height: the stop hint and the steer/queue switch come and go inside it, so nothing moves. -->
		<div
			class="mt-1.5 flex min-h-8 items-center justify-between gap-3 px-1 text-xs text-muted-foreground"
		>
			{#if busy && canControl}
				<span class="min-w-0 flex-1 truncate" role={hint ? 'status' : undefined}
					>{hint
						? 'Hold the button to stop, or press Esc.'
						: followUp
							? 'Your message waits until the agent is done.'
							: 'Your message reaches the agent before its next step.'}</span
				>
				<Segmented
					class="w-auto shrink-0"
					label="When to deliver"
					value={followUp ? 'queue' : 'steer'}
					onchange={(v) => (followUp = v === 'queue')}
					options={[
						{ value: 'steer', label: 'Steer' },
						{ value: 'queue', label: 'Queue' }
					]}
				/>
			{:else}
				<span class="min-w-0 flex-1 truncate">{canControl ? sendHint : ''}</span>
			{/if}
		</div>
	</div>
</form>
