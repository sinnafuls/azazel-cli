<script lang="ts">
	// The agents' browser, live: the picture of the tab it shows, scaled to fit, with what it is
	// doing. A device that may control can take it over (the agents wait), and then its clicks, the
	// wheel and its keys over the picture go to the page, until it hands the browser back.
	// The keyboard is taken by a field nobody sees, in the picture: only a field brings up the
	// keyboard of a phone and is given what an input method, a dead key or a dictation composes.
	// What is typed there goes to the page and never stays in the field.
	// Pictures are asked for only while `active` (the tab of this panel is the one shown) and the
	// page itself is visible; otherwise the host makes none for this device.
	import { onDestroy, onMount, tick, untrack } from 'svelte';
	import type { Session } from '$lib/session/session.svelte';
	import type { BrowserInput } from '$lib/protocol';
	import {
		address,
		Clicks,
		composes,
		composingKey,
		editFor,
		fraction,
		keysFor,
		scaleOf,
		Scrolls,
		statusOf,
		Typing,
		wheelDelta,
		type BrowserStatus
	} from '$lib/browser/input';
	import { toasts } from '$lib/toasts.svelte';
	import { viewport } from '$lib/media.svelte';
	import { cn } from '$lib/utils';
	import { Button } from '$lib/components/ui/button';
	import * as Alert from '$lib/components/ui/alert';
	import * as Empty from '$lib/components/ui/empty';
	import * as InputGroup from '$lib/components/ui/input-group';
	import { Skeleton } from '$lib/components/ui/skeleton';
	import { Kbd } from '$lib/components/ui/kbd';
	import StateDot from './StateDot.svelte';
	import Tip from './Tip.svelte';
	import ArrowLeft from '@lucide/svelte/icons/arrow-left';
	import RotateCw from '@lucide/svelte/icons/rotate-cw';

	let {
		session,
		canControl,
		active = true
	}: { session: Session; canControl: boolean; active?: boolean } = $props();
	const live = $derived(session.browser);

	let pageVisible = $state(true);
	onMount(() => {
		const see = () => (pageVisible = document.visibilityState === 'visible');
		see();
		document.addEventListener('visibilitychange', see);
		return () => document.removeEventListener('visibilitychange', see);
	});
	$effect(() => {
		if (!active || !pageVisible) return;
		const b = live;
		// Untracked: asking reads the state it then changes.
		return untrack(() => b.want());
	});

	const status = $derived(statusOf(live.state, session.busy));
	const holding = $derived(status === 'you');
	const picture = $derived(live.picture);
	const LABEL: Record<BrowserStatus, string> = {
		none: 'No browser open',
		watching: 'Watching',
		agent: 'Agent working',
		you: 'You have control',
		other: 'Held by another device'
	};
	// The mark of each: grey for nothing, green for a live picture, the accent while the agent
	// works, amber while the agent waits for a person.
	const DOT: Record<BrowserStatus, string> = {
		none: 'idle',
		watching: 'done',
		agent: 'working',
		you: 'waiting for you',
		other: 'waiting for you'
	};
	const NOTE: Partial<Record<BrowserStatus, string>> = {
		you: 'The agent waits until you hand back. It is then told what you did, never what you typed.',
		other: 'Another device has taken the browser over. The agent waits until it hands back.'
	};

	const send = (input: BrowserInput) => void live.input(input);
	const clicks = new Clicks((x, y, count, continues) =>
		send({ kind: 'click', x, y, count, ...(continues && { continues }) })
	);
	const scrolls = new Scrolls((x, y, dy) => send({ kind: 'scroll', x, y, dy }));
	const typing = new Typing(send);
	/** Sends what still waits, in the order it was done. */
	function flush() {
		clicks.flush();
		typing.flush();
	}
	function forget() {
		clicks.drop();
		scrolls.cancel();
		typing.cancel();
	}
	$effect(() => {
		if (!holding) forget();
	});
	onDestroy(forget);

	let box = $state<HTMLElement | null>(null);
	let img = $state<HTMLImageElement | null>(null);
	let handBack = $state<HTMLButtonElement | null>(null);
	// The field that takes the keyboard, and where in the box it is: at the last click, so that a
	// phone keeps that place above its keyboard and an input method opens its list beside it.
	let keys = $state<HTMLInputElement | null>(null);
	let caret = $state({ x: 0, y: 0 });
	// Whether an input method is composing text in the field, and whether what the field is about
	// to report as an edit was sent already: text that was composed (some browsers report it once
	// more, right after), or an edit that could not be kept out of the field.
	let composing = false;
	let reported = false;
	/** What the field reports next, in this turn of the event loop, is not sent again. */
	function sentAlready() {
		reported = true;
		setTimeout(() => (reported = false));
	}
	$effect(() => {
		if (!holding) composing = false;
	});

	async function take(on: boolean) {
		if (!on) flush();
		if ((await live.take(on)) && on) {
			await tick();
			// A touch screen would cover half the picture with its keyboard at once: there the field
			// takes the keyboard at the first tap on the picture.
			if (!viewport.coarse) keys?.focus({ preventScroll: true });
		}
	}
	async function signIn() {
		if (await live.signIn())
			toasts.push(
				"The agent's browser is open in a window on the computer that runs azazel. Sign in there.",
				'success',
				9000
			);
	}

	/** A point of the screen as fractions of the picture; null beside it. */
	function at(e: { clientX: number; clientY: number }) {
		return img && picture ? fraction(e, img.getBoundingClientRect(), picture) : null;
	}
	function onmousedown(e: MouseEvent) {
		// The field keeps the keyboard through a click on the picture: a phone would otherwise put
		// its keyboard away and bring it back at every tap.
		if (holding) e.preventDefault();
	}
	/** Puts the keyboard on the field, at the place of the click. */
	function focusKeys(e: MouseEvent) {
		if (!keys || !box) return;
		// Leaving the field ends text that is still being composed, which belongs before the click.
		// A phone whose keyboard was put away brings it back only for a field it enters anew.
		if (composing || viewport.coarse) keys.blur();
		const r = box.getBoundingClientRect();
		const within = (n: number, size: number) => Math.round(Math.min(Math.max(n, 0), size - 2));
		caret = { x: within(e.clientX - r.left, r.width), y: within(e.clientY - r.top, r.height) };
		keys.focus({ preventScroll: true });
	}
	function onclick(e: MouseEvent) {
		if (!holding) return;
		focusKeys(e);
		const p = at(e);
		if (!p) return;
		typing.flush();
		clicks.click(p.x, p.y, e.detail || 1);
	}
	function oncontextmenu(e: MouseEvent) {
		const p = holding ? at(e) : null;
		if (!p) return;
		e.preventDefault();
		flush();
		send({ kind: 'click', x: p.x, y: p.y, button: 'right' });
	}
	function onwheel(e: WheelEvent) {
		const p = holding ? at(e) : null;
		if (!p) return;
		// The page of the agent scrolls, not this panel.
		e.preventDefault();
		scrolls.add(p.x, p.y, wheelDelta(e));
	}
	function onkeydown(e: KeyboardEvent) {
		if (!holding) return;
		// Tab goes to the page too, so there is a key that leaves the picture.
		if (e.key === 'Escape' && e.shiftKey) {
			e.preventDefault();
			e.stopPropagation();
			handBack?.focus();
			return;
		}
		// Pasting sends this device's clipboard (see `onpaste`), not the one of the host.
		if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 'v') return;
		// A key the input method takes is its own: Escape there drops what is being composed, and
		// does not stop the agent.
		if (composingKey(e)) return e.stopPropagation();
		const k = keysFor(e);
		if (!k) return;
		// The key is the page's: not a shortcut of this page, and not what stops the agent.
		e.stopPropagation();
		// A character is not sent from here: the field is given what the key types (`onbeforeinput`),
		// which is the character, or what a dead key before it or the layout makes of it.
		if (k.kind === 'text') return;
		e.preventDefault();
		clicks.flush();
		typing.press(k, e.repeat);
	}
	/** Sends text for the page, after the click that put the caret where it belongs. */
	function type(text: string) {
		clicks.flush();
		typing.press({ kind: 'text', text });
	}
	function onbeforeinput(e: InputEvent) {
		// Text being composed cannot be kept out of the field; it is taken out when it is done.
		if (composes(e)) return;
		// Nothing else gets into the field: it goes to the page, or nowhere.
		e.preventDefault();
		const k = holding ? editFor(e) : null;
		if (!k) return;
		// An edit that cannot be refused gets into the field and is reported again (`oninput`).
		if (!e.defaultPrevented) sentAlready();
		clicks.flush();
		// A phone's Backspace that is held down is paced as a held key is.
		typing.press(k, k.kind === 'key' && k.keys !== 'Enter');
	}
	function oncompositionend(e: CompositionEvent) {
		composing = false;
		const text = e.data || keys?.value || '';
		if (keys) keys.value = '';
		sentAlready();
		if (holding && text) type(text);
	}
	function oninput(e: Event) {
		if (!keys || composing || (e as InputEvent).isComposing) return;
		// What got into the field without being composed (a browser without `beforeinput`, a
		// keyboard that types past it) goes to the page as well, and out of the field.
		const text = keys.value;
		keys.value = '';
		if (holding && text && !reported) type(text);
	}
	function onpaste(e: ClipboardEvent) {
		const text = holding ? e.clipboardData?.getData('text/plain') : '';
		if (!text) return;
		e.preventDefault();
		flush();
		send({ kind: 'text', text });
	}
	// A finger that drags over the picture scrolls the page under it.
	let drag: { id: number; y: number } | null = null;
	function onpointerdown(e: PointerEvent) {
		if (holding && e.pointerType === 'touch') drag = { id: e.pointerId, y: e.clientY };
	}
	function onpointermove(e: PointerEvent) {
		if (!drag || e.pointerId !== drag.id || !img || !picture) return;
		const p = at(e);
		const dy = drag.y - e.clientY;
		drag.y = e.clientY;
		if (p && dy) scrolls.add(p.x, p.y, dy * scaleOf(img.getBoundingClientRect(), picture));
	}
	function onpointerup(e: PointerEvent) {
		if (drag?.id === e.pointerId) drag = null;
	}

	// The address field shows where the browser is until something is typed into it.
	let typed = $state('');
	let editing = $state(false);
	let bad = $state(false);
	$effect(() => {
		const url = live.state?.url ?? '';
		if (!editing) typed = url;
	});
	function go(e: SubmitEvent) {
		e.preventDefault();
		const url = address(typed);
		bad = !url;
		if (!url) return;
		editing = false;
		flush();
		send({ kind: 'navigate', url });
	}
	function nav(kind: 'back' | 'reload') {
		flush();
		send({ kind });
	}
</script>

<section aria-label="Browser" class="grid gap-3" data-browser data-status={status}>
	{#if live.denied}
		<Empty.Root class="items-start gap-1 p-0 text-left" data-browser-denied>
			<Empty.Title>Watching needs a control link</Empty.Title>
			<Empty.Description>
				This device has a view link. The picture of the agent's browser can show anything a page
				shows, so only a control link sees it, unless the computer that runs azazel allows view
				links to watch.
			</Empty.Description>
		</Empty.Root>
	{:else}
		<!-- The status has a line of its own, with what it means under it, and the buttons under
		     that: beside the status they fit a phone but not the pane, and a button that moves when
		     the status changes is harder to find again. -->
		<div class="grid gap-2">
			<p
				class="flex min-w-0 items-center gap-2 text-sm leading-5 font-medium"
				role="status"
				data-browser-status
			>
				<StateDot state={DOT[status]} />
				<span class="truncate">{LABEL[status]}</span>
			</p>
			{#if NOTE[status]}
				<p class="text-xs leading-relaxed text-muted-foreground" data-browser-note>
					{NOTE[status]}
				</p>
			{/if}
			{#if canControl}
				<div class="flex items-center gap-2" data-browser-actions>
					{#if holding}
						<Button
							bind:ref={handBack}
							size="sm"
							disabled={live.working}
							onclick={() => take(false)}>Hand back</Button
						>
					{:else if status !== 'none'}
						<Button
							size="sm"
							disabled={live.working || status === 'other'}
							onclick={() => take(true)}>Take over</Button
						>
					{/if}
					<Tip
						text="Opens the agent's browser profile in a window on the computer that runs azazel, so you can sign in to sites there. The agent keeps the sign-in."
						side="bottom"
						variant="outline"
						size="sm"
						disabled={live.working}
						onclick={signIn}>Sign in</Tip
					>
				</div>
			{/if}
		</div>
		{#if live.error}
			<Alert.Root variant="destructive" role="alert">
				<Alert.Description>{live.error}</Alert.Description>
			</Alert.Root>
		{/if}

		{#if status === 'none'}
			<Empty.Root class="items-start gap-1 p-0 text-left" data-browser-empty>
				<Empty.Title>The agent has not opened a browser</Empty.Title>
				<Empty.Description>
					When it does, the page it is on shows here, live.
					{#if canControl}
						You can then take over to click and type, and hand back. Sign in opens its browser in a
						window on the computer that runs azazel.
					{/if}
				</Empty.Description>
			</Empty.Root>
		{:else}
			<div class="grid min-w-0 gap-1.5">
				<h3 class="truncate text-sm leading-5 font-medium" data-browser-title>
					{live.state?.title || 'Untitled page'}
				</h3>
				{#if holding}
					<form onsubmit={go} novalidate data-browser-address>
						<!-- The stock group, with no clip: the empty padding of its addons reaches a few pixels
						     past its border, which shows nothing and scrolls nothing. -->
						<InputGroup.Root>
							<InputGroup.Addon>
								<InputGroup.Button size="icon-xs" aria-label="Back" onclick={() => nav('back')}>
									<ArrowLeft />
								</InputGroup.Button>
								<InputGroup.Button size="icon-xs" aria-label="Reload" onclick={() => nav('reload')}>
									<RotateCw />
								</InputGroup.Button>
							</InputGroup.Addon>
							<InputGroup.Input
								bind:value={typed}
								aria-label="Address"
								aria-invalid={bad || undefined}
								placeholder="example.com"
								inputmode="url"
								autocomplete="off"
								autocapitalize="off"
								spellcheck="false"
								oninput={() => {
									editing = true;
									bad = false;
								}}
								onkeydown={(e) => {
									if (e.key !== 'Escape' || !editing) return;
									// Escape takes back what was typed; it does not stop the agent.
									e.preventDefault();
									e.stopPropagation();
									editing = false;
									bad = false;
								}}
							/>
							<InputGroup.Addon align="inline-end">
								<InputGroup.Button type="submit" size="xs" variant="secondary">Go</InputGroup.Button
								>
							</InputGroup.Addon>
						</InputGroup.Root>
					</form>
					{#if bad}
						<p class="text-xs text-destructive" role="alert">
							Enter a web address, such as example.com.
						</p>
					{/if}
				{:else}
					<p class="truncate font-mono text-xs leading-5 text-muted-foreground" data-browser-url>
						{live.state?.url ?? ''}
					</p>
				{/if}
			</div>

			<!-- The box has the shape of the picture until it would be taller than most of the screen;
			     then the picture keeps its shape inside it, with bars beside it. -->
			<!-- It takes the keyboard only while this device holds the browser, as an `application`:
			     the field in it has the focus then, and the box shows that it has. -->
			<div
				bind:this={box}
				class={cn(
					'relative max-h-[70svh] w-full overflow-hidden rounded-lg border bg-muted outline-none',
					holding &&
						'touch-none focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50'
				)}
				style:aspect-ratio="{picture?.width ?? 16} / {picture?.height ?? 9}"
				data-browser-picture
				data-vaul-no-drag={holding ? '' : undefined}
				role={holding ? 'application' : undefined}
				aria-label={holding
					? "The agent's browser. Clicks and keys go to its page. Shift and Escape leave it."
					: undefined}
				{onmousedown}
				{onclick}
				{oncontextmenu}
				{onwheel}
				{onkeydown}
				{onpaste}
				{onpointerdown}
				{onpointermove}
				{onpointerup}
				onpointercancel={onpointerup}
			>
				{#if holding}
					<!-- One pixel, out of the flow and transparent: it moves nothing and shows nothing.
					     Its type is 16px at every text size, or a phone zooms in when the field gets the focus. -->
					<input
						bind:this={keys}
						class="pointer-events-none absolute size-px appearance-none border-0 bg-transparent p-0 text-[16px] opacity-0 outline-none"
						style:left="{caret.x}px"
						style:top="{caret.y}px"
						aria-label="Type into the agent's page"
						type="text"
						autocomplete="off"
						autocapitalize="off"
						autocorrect="off"
						spellcheck="false"
						enterkeyhint="enter"
						data-browser-keys
						{onbeforeinput}
						{oninput}
						oncompositionstart={() => (composing = true)}
						{oncompositionend}
					/>
				{/if}
				{#if picture}
					<img
						bind:this={img}
						src={picture.src}
						alt="The page the agent's browser shows{live.state?.title
							? `: ${live.state.title}`
							: ''}"
						class="size-full object-contain select-none"
						draggable="false"
					/>
				{:else}
					<Skeleton class="size-full rounded-none" aria-label="Waiting for the picture" />
				{/if}
			</div>
			{#if holding}
				<p class="text-xs leading-relaxed text-muted-foreground" data-browser-hint>
					{#if viewport.coarse}
						Tap the picture to click in the page and to type there, drag to scroll it.
					{:else}
						Click the picture to use the page: keys and the wheel go to it.
						<Kbd>Shift</Kbd>
						<Kbd>Esc</Kbd> leaves it.
					{/if}
				</p>
			{/if}
		{/if}
	{/if}
</section>
