<script lang="ts">
	// A native <dialog>: focus is trapped, Escape works, the scrim is free. On a phone it is a bottom
	// sheet with a grab handle that you can swipe down to dismiss; on a wide screen a `docked` sheet
	// sits in the flow above the composer so the transcript stays readable while you decide.
	import type { Snippet } from 'svelte';
	import { viewport } from '$lib/media.svelte';
	import X from '@lucide/svelte/icons/x';
	import { cn } from '$lib/utils';

	let {
		open,
		title,
		subtitle,
		docked = false,
		dismissible = true,
		attention = false,
		onclose,
		children,
		footer,
		class: className
	}: {
		open: boolean;
		title: string;
		subtitle?: string;
		docked?: boolean;
		dismissible?: boolean;
		/** Draws the accent edge: this sheet is waiting for you. */
		attention?: boolean;
		onclose?: () => void;
		children: Snippet;
		footer?: Snippet;
		class?: string;
	} = $props();

	let el = $state<HTMLDialogElement>();
	let heading = $state<HTMLHeadingElement>();
	const modal = $derived(!(docked && viewport.wide));

	$effect(() => {
		const d = el;
		if (!d) return;
		if (open && !d.open) {
			if (modal) d.showModal();
			else d.show();
			heading?.focus({ preventScroll: true });
		} else if (!open && d.open) d.close();
	});

	// Swipe down on the handle area to dismiss.
	let dragY = $state(0);
	let startY = $state<number | null>(null);
	function down(e: PointerEvent) {
		if (!dismissible || !onclose || !modal) return;
		startY = e.clientY;
		(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
	}
	function move(e: PointerEvent) {
		if (startY !== null) dragY = Math.max(0, e.clientY - startY);
	}
	function up() {
		if (startY === null) return;
		const far = dragY > 90;
		startY = null;
		dragY = 0;
		if (far) onclose?.();
	}
</script>

<dialog
	bind:this={el}
	class={cn(
		'sheet m-0 w-full max-w-none p-0 text-foreground',
		modal
			? 'float mt-auto max-h-[90dvh] rounded-t-2xl border-b-0 md:m-auto md:max-h-[80dvh] md:max-w-lg md:rounded-2xl md:border-b'
			: 'float relative mx-auto max-h-[52dvh] max-w-3xl rounded-xl',
		className
	)}
	style:transform={dragY ? `translateY(${dragY}px)` : undefined}
	style:transition={startY === null ? 'transform 180ms var(--ease-out)' : 'none'}
	aria-labelledby="sheet-title"
	oncancel={(e) => {
		if (!dismissible) e.preventDefault();
		else onclose?.();
	}}
>
	{#if open}
		<div class="flex flex-col" style="max-height: inherit">
			{#if attention}
				<div
					class="bg-primary absolute inset-x-6 top-0 h-px rounded-full opacity-80"
					aria-hidden="true"
				></div>
			{/if}
			{#if modal}
				<div
					class="grid shrink-0 touch-none place-items-center pt-2.5 pb-1 md:hidden"
					onpointerdown={down}
					onpointermove={move}
					onpointerup={up}
					onpointercancel={up}
					role="presentation"
				>
					<span class="bg-input h-1 w-9 rounded-full" aria-hidden="true"></span>
				</div>
			{/if}
			<header class="flex shrink-0 items-start gap-3 px-5 pt-3 pb-2 md:pt-4">
				<div class="min-w-0 flex-1">
					<h2
						id="sheet-title"
						bind:this={heading}
						tabindex="-1"
						class="text-base leading-6 font-semibold tracking-[-0.015em] outline-none"
					>
						{title}
					</h2>
					{#if subtitle}<p class="text-muted-foreground mt-0.5 truncate text-xs">{subtitle}</p>{/if}
				</div>
				{#if dismissible && onclose}
					<button
						type="button"
						class="text-muted-foreground hover:bg-raised hover:text-foreground press -mt-1 -mr-2 grid size-11 place-items-center rounded-lg md:size-8"
						aria-label="Close"
						onclick={onclose}
					>
						<X class="size-4" />
					</button>
				{/if}
			</header>
			<div class="scroll-thin min-h-0 flex-1 overflow-y-auto px-5 pb-4">{@render children()}</div>
			{#if footer}
				<footer
					class="bg-card/60 shrink-0 rounded-b-[inherit] border-t px-5 pt-3 pb-[max(0.875rem,env(safe-area-inset-bottom))]"
				>
					{@render footer()}
				</footer>
			{:else}
				<div class="h-[env(safe-area-inset-bottom)] shrink-0"></div>
			{/if}
		</div>
	{/if}
</dialog>

<style>
	dialog::backdrop {
		background: oklch(0.1 0.01 268 / 0.6);
		animation: fade-in var(--dur) ease-out;
	}
	dialog[open]:modal {
		animation: sheet-in var(--dur-slow) var(--ease-out);
	}
	dialog[open]:not(:modal) {
		animation: enter var(--dur) var(--ease-out);
	}
	@keyframes sheet-in {
		from {
			transform: translateY(40px);
			opacity: 0;
		}
	}
	@media (min-width: 768px) {
		dialog[open]:modal {
			animation: enter var(--dur) var(--ease-out);
		}
	}
</style>
