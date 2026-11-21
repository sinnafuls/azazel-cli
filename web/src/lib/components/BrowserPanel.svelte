<script lang="ts">
	// The agents' browser, live. One header row says what it is on (the page's title over its
	// address), what it is doing (a badge) and holds the buttons; the picture of the tab fills the
	// panel under it from edge to edge, in width and in height: a device that may control tells the
	// host how much room the picture has, and the host lays the agent's page out for that shape
	// while the device watches. Until that size lands, and on a device that may only watch, the
	// picture keeps its own shape inside the room. A device that may control can take it over
	// (the agents wait): an address row then stands under the header (back, forward, reload and the
	// address), and clicks, the wheel and keys over the picture go to the page, until the browser is
	// handed back.
	// Back, forward and reload are not in the header: five buttons there leave the title 9px of the
	// 215px pane of a 1024px window, and 32px of the 331px pane of a 1440px one. In a pane as
	// narrow as those they are a line of their own over the address, which then has the whole width.
	// The parts answer to the width of the panel, not of the window: the pane beside the chat is
	// about 215px wide in a 1024px window, where the globe goes and the badge is its mark alone.
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
		pageTitle,
		scaleOf,
		Scrolls,
		statusOf,
		Typing,
		wheelDelta,
		type BrowserStatus
	} from '$lib/browser/input';
	import { ROOM_SETTLE_MS } from '$lib/browser/live.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { viewport } from '$lib/media.svelte';
	import { cn } from '$lib/utils';
	import * as Alert from '$lib/components/ui/alert';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import * as Empty from '$lib/components/ui/empty';
	import * as InputGroup from '$lib/components/ui/input-group';
	import { Kbd } from '$lib/components/ui/kbd';
	import { Spinner } from '$lib/components/ui/spinner';
	import StateDot from './StateDot.svelte';
	import Tip from './Tip.svelte';
	import ArrowLeft from '@lucide/svelte/icons/arrow-left';
	import ArrowRight from '@lucide/svelte/icons/arrow-right';
	import Globe from '@lucide/svelte/icons/globe';
	import LogIn from '@lucide/svelte/icons/log-in';
	import MousePointerClick from '@lucide/svelte/icons/mouse-pointer-click';
	import RotateCw from '@lucide/svelte/icons/rotate-cw';
	import Undo2 from '@lucide/svelte/icons/undo-2';

	let {
		session,
		canControl,
		active = true,
		fill = false,
		class: className
	}: {
		session: Session;
		canControl: boolean;
		active?: boolean;
		/** The panel is as tall as what it is in (the pane), and the picture takes what is left. */
		fill?: boolean;
		class?: string;
	} = $props();
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
	// Short: the badge stands in the header beside the title, in a pane that may be narrow.
	const LABEL: Record<BrowserStatus, string> = {
		none: 'Not open',
		watching: 'Watching',
		agent: 'Working',
		you: 'Holding',
		other: 'Held elsewhere'
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

	let head = $state<HTMLElement | null>(null);
	let box = $state<HTMLElement | null>(null);
	let img = $state<HTMLImageElement | null>(null);
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

	// The room the picture has is told to the host: at once when it is first known, so that the
	// first pictures have its shape, and afterwards when it has stopped changing for a moment (a
	// pane that is dragged, a window that is resized, the address row that comes and goes). A box
	// that is not shown has no size and says nothing: the room it had is the one it comes back to.
	$effect(() => {
		const el = box;
		if (!el || !canControl || typeof ResizeObserver === 'undefined') return;
		const b = live;
		let timer: ReturnType<typeof setTimeout> | null = null;
		let first = true;
		const tell = () => {
			timer = null;
			const { clientWidth: width, clientHeight: height } = el;
			if (width > 0 && height > 0) {
				first = false;
				b.fit({ width, height });
			}
		};
		const seen = new ResizeObserver(() => {
			if (timer !== null) clearTimeout(timer);
			if (first) tell();
			else timer = setTimeout(tell, ROOM_SETTLE_MS);
		});
		seen.observe(el);
		return () => {
			seen.disconnect();
			if (timer !== null) clearTimeout(timer);
		};
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
	// Which of the buttons is waiting for the host: Open shows it, and keeps its width.
	let opening = $state(false);
	async function open() {
		opening = true;
		try {
			await live.open();
		} finally {
			opening = false;
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
			head?.querySelector<HTMLElement>('[data-browser-hand-back]')?.focus();
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
	function nav(kind: 'back' | 'forward' | 'reload') {
		flush();
		send({ kind });
	}
</script>

<section
	aria-label="Browser"
	class={cn('@container flex min-w-0 flex-col', fill && 'h-full min-h-0', className)}
	data-browser
	data-status={status}
>
	{#if live.denied}
		<Empty.Root class="rounded-none" data-browser-denied>
			<Empty.Header>
				<Empty.Title>Watching needs a control link</Empty.Title>
				<Empty.Description>
					This device has a view link. The picture of the agent's browser can show anything a page
					shows, so only a control link sees it, unless the computer that runs azazel allows view
					links to watch.
				</Empty.Description>
			</Empty.Header>
		</Empty.Root>
	{:else}
		<!-- One row, the same in every state: a button that moves when the state changes is harder
		     to find again. The title and the address are two lines of one block, so each has the
		     whole width between the globe and the badge. -->
		<header
			bind:this={head}
			class="flex h-12 shrink-0 items-center gap-2 border-b px-3"
			data-browser-header
		>
			<Globe class="size-4 shrink-0 text-muted-foreground @max-xs:hidden" aria-hidden="true" />
			<div class="min-w-0 flex-1" data-browser-heading>
				<h3 class="truncate text-sm leading-5 font-medium" data-browser-title>
					{status === 'none' ? "Agent's browser" : pageTitle(live.state)}
				</h3>
				{#if status !== 'none' && live.state?.url}
					<p class="truncate font-mono text-xs leading-4 text-muted-foreground" data-browser-url>
						{live.state.url}
					</p>
				{/if}
			</div>
			<!-- With no browser the empty state says so; a badge would say it twice. -->
			{#if status !== 'none'}
				<Badge
					variant="outline"
					role="status"
					class="gap-1 pl-1 @max-xs:border-transparent @max-xs:px-0"
					data-browser-status
				>
					<StateDot state={DOT[status]} />
					<span class="@max-xs:sr-only">{LABEL[status]}</span>
				</Badge>
			{/if}
			{#if canControl}
				<div class="flex shrink-0 items-center gap-0.5" data-browser-actions>
					{#if holding}
						<Tip
							text="Hand back to the agent"
							side="bottom"
							variant="default"
							aria-label="Hand back"
							disabled={live.working}
							onclick={() => take(false)}
							data-browser-hand-back><Undo2 /></Tip
						>
					{:else if status !== 'none'}
						<Tip
							text="Take over: the agent waits while you click and type"
							side="bottom"
							aria-label="Take over"
							disabled={live.working || status === 'other'}
							onclick={() => take(true)}><MousePointerClick /></Tip
						>
					{/if}
					<Tip
						text="Sign in: opens the agent's browser in a window on the computer that runs azazel, so you can sign in to sites there. The agent keeps the sign-in."
						side="bottom"
						aria-label="Sign in"
						disabled={live.working}
						onclick={signIn}><LogIn /></Tip
					>
				</div>
			{/if}
		</header>
		{#if live.error}
			<div class="shrink-0 border-b p-3" data-browser-error>
				<Alert.Root variant="destructive" role="alert">
					<Alert.Description>{live.error}</Alert.Description>
				</Alert.Root>
			</div>
		{/if}

		{#if status === 'none'}
			<Empty.Root class="rounded-none" data-browser-empty>
				<Empty.Header>
					<Empty.Title>The agent has no browser open</Empty.Title>
					<Empty.Description>
						When it opens a page, the page shows here, live.
						{#if canControl}
							You can open it yourself, on a blank page, and take it over.
						{/if}
					</Empty.Description>
				</Empty.Header>
				{#if canControl}
					<Empty.Content>
						<Button
							variant="outline"
							class="min-w-24"
							disabled={live.working}
							aria-busy={opening || undefined}
							onclick={open}
							data-browser-open
						>
							{#if opening}<Spinner role="presentation" aria-hidden="true" />{/if}
							Open
						</Button>
					</Empty.Content>
				{/if}
			</Empty.Root>
		{:else}
			{#if holding}
				<!-- As in a browser: back, forward and reload before the address they act on. They stand
				     beside the group, not in it; in a narrow pane the group takes the line under them,
				     so that an address can be read there. The field is as tall as the inside of the
				     group and the one addon gives up the padding and the margin the stock one reaches
				     past the border with, so nothing overhangs the group. -->
				<form
					class="grid shrink-0 gap-1.5 border-b px-3 py-2"
					onsubmit={go}
					novalidate
					data-browser-address
				>
					<div class="flex items-center gap-1 @max-sm:flex-wrap @max-sm:gap-y-1.5" data-browser-nav>
						<Tip text="Back" side="bottom" size="icon" aria-label="Back" onclick={() => nav('back')}
							><ArrowLeft /></Tip
						>
						<Tip
							text="Forward"
							side="bottom"
							size="icon"
							aria-label="Forward"
							onclick={() => nav('forward')}><ArrowRight /></Tip
						>
						<Tip
							text="Reload"
							side="bottom"
							size="icon"
							aria-label="Reload"
							onclick={() => nav('reload')}><RotateCw /></Tip
						>
						<InputGroup.Root class="flex-1 @max-sm:basis-full">
							<InputGroup.Input
								bind:value={typed}
								class="h-full"
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
							<InputGroup.Addon align="inline-end" class="py-0 pr-1 has-[>button]:mr-0">
								<InputGroup.Button type="submit" size="xs" variant="secondary">Go</InputGroup.Button
								>
							</InputGroup.Addon>
						</InputGroup.Root>
					</div>
					{#if bad}
						<p class="text-xs text-destructive" role="alert">
							Enter a web address, such as example.com.
						</p>
					{/if}
				</form>
			{/if}

			<div class={cn('flex min-w-0 flex-col', fill && 'min-h-0 flex-1')} data-browser-body>
				<!-- No box of its own: the room of the picture is as wide as the panel and as tall as
				     what is left of the pane, or half the screen in a sheet, which has no height of its
				     own to give. The host lays the page out for that room, so the picture fills it; one
				     of another shape (the first, one an agent sized, one laid out for another device)
				     keeps its shape inside, in the middle. A hairline under it ends a white page on a
				     white ground. -->
				<!-- It takes the keyboard only while this device holds the browser, as an `application`:
				     the field in it has the focus then, and a ring drawn over the picture shows that. -->
				<div
					bind:this={box}
					class={cn(
						'relative w-full overflow-hidden shadow-[0_1px_0_0_var(--border)] outline-none',
						fill ? 'min-h-0 flex-1' : 'h-[50svh] shrink-0',
						holding &&
							'touch-none after:pointer-events-none after:absolute after:inset-0 focus-within:after:ring-2 focus-within:after:ring-ring/60 focus-within:after:ring-inset'
					)}
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
						<!-- A browser is running and no picture of it has come yet: said in words, in the
						     place the picture will take, and not as a grey box that looks like a page. -->
						<div
							class="absolute inset-0 flex flex-col items-center justify-center gap-2 p-3 text-center text-muted-foreground"
							data-browser-waiting
						>
							<Spinner role="presentation" aria-hidden="true" />
							<p class="text-xs">Waiting for the first picture…</p>
						</div>
					{/if}
				</div>
				{#if NOTE[status]}
					<div
						class="grid shrink-0 gap-1 px-3 py-2 text-xs leading-relaxed text-muted-foreground"
						data-browser-note
					>
						<p>{NOTE[status]}</p>
						{#if holding}
							<p data-browser-hint>
								{#if viewport.coarse}
									Tap the picture to click in the page and to type there, drag to scroll it.
								{:else}
									Click the picture to use the page: keys and the wheel go to it.
									<Kbd>Shift</Kbd>
									<Kbd>Esc</Kbd> leaves it.
								{/if}
							</p>
						{/if}
					</div>
				{/if}
			</div>
		{/if}
	{/if}
</section>
