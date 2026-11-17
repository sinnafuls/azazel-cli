<script lang="ts" module>
	let count = 0;
</script>

<script lang="ts">
	// A native <dialog>: focus is trapped, Escape works, the scrim is free. On a phone it is a bottom
	// sheet with a grab handle that you can swipe down to dismiss; on a wide screen a `docked` sheet
	// sits in the flow above the composer so the transcript stays readable while you decide.
	import type { Snippet } from 'svelte';
	import { viewport } from '$lib/media.svelte';
	import StateDot from './StateDot.svelte';
	import X from '@lucide/svelte/icons/x';
	import ChevronLeft from '@lucide/svelte/icons/chevron-left';
	import { cn } from '$lib/utils';

	let {
		open,
		title,
		subtitle,
		docked = false,
		dismissible = true,
		attention = false,
		onclose,
		onback,
		children,
		footer,
		actions,
		class: className
	}: {
		open: boolean;
		title: string;
		subtitle?: string;
		docked?: boolean;
		dismissible?: boolean;
		/** Marks the sheet with the accent: it is waiting for you. */
		attention?: boolean;
		onclose?: () => void;
		/** Shows a back button instead of the close button: the sheet is a step into something. */
		onback?: () => void;
		children: Snippet;
		footer?: Snippet;
		/** Buttons on the right of the title. */
		actions?: Snippet;
		class?: string;
	} = $props();

	// Several sheets can be open at once (a picker over a docked request): each names its own title.
	const titleId = `sheet-title-${++count}`;

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
		'sheet text-foreground m-0 w-full max-w-none overflow-hidden p-0',
		modal
			? 'float mt-auto max-h-[90dvh] rounded-t-[28px] border-x-0 border-b-0 md:m-auto md:max-h-[82dvh] md:max-w-lg md:rounded-[24px] md:border'
			: 'surface-2 relative mx-auto mb-3 max-h-[56dvh] w-[calc(100%-2rem)] max-w-[43rem] rounded-[24px] md:w-[calc(100%-4rem)]',
		className
	)}
	data-attention={attention && !modal ? '' : undefined}
	style:transform={dragY ? `translateY(${dragY}px)` : undefined}
	style:transition={startY === null ? 'transform 200ms var(--ease-out)' : 'none'}
	aria-labelledby={titleId}
	oncancel={(e) => {
		if (!dismissible) e.preventDefault();
		else onclose?.();
	}}
>
	{#if open}
		<div class="flex flex-col" style="max-height: inherit">
			{#if modal}
				<div
					class="grid shrink-0 touch-none place-items-center pt-2 pb-0.5 md:hidden"
					onpointerdown={down}
					onpointermove={move}
					onpointerup={up}
					onpointercancel={up}
					role="presentation"
				>
					<span class="bg-input/50 h-[5px] w-10 rounded-full" aria-hidden="true"></span>
				</div>
			{/if}
			<header
				class="grid shrink-0 grid-cols-[2.75rem_minmax(0,1fr)_2.75rem] items-center gap-2 px-3 pt-2 pb-3 md:px-4 md:pt-4 fine:grid-cols-[2.25rem_minmax(0,1fr)_2.25rem]"
			>
				<div class="flex justify-start">
					{#if onback}
						<button type="button" class="btn btn-round btn-icon" aria-label="Back" onclick={onback}>
							<ChevronLeft class="size-[18px]" />
						</button>
					{:else if dismissible && onclose}
						<button
							type="button"
							class="btn btn-round btn-icon"
							aria-label="Close"
							onclick={onclose}
						>
							<X class="size-[18px]" />
						</button>
					{:else if attention}
						<StateDot state="waiting for you" class="ml-2" />
					{/if}
				</div>
				<div class="min-w-0 text-center">
					<h2
						id={titleId}
						bind:this={heading}
						tabindex="-1"
						class="truncate text-[17px] leading-6 font-semibold tracking-[-0.02em] outline-none"
					>
						{title}
					</h2>
					{#if subtitle}<p class="text-muted-foreground mt-0.5 truncate text-xs">{subtitle}</p>{/if}
				</div>
				<div class="flex justify-end">
					{#if actions}{@render actions()}{/if}
				</div>
			</header>
			<div class="scroll-thin min-h-0 flex-1 overflow-y-auto px-4 pb-5 md:px-5">
				{@render children()}
			</div>
			{#if footer}
				<footer
					class="bg-inset/60 shrink-0 border-t px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))] md:px-5"
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
	/* The scrim is the same in both themes: a custom property does not reach ::backdrop everywhere. */
	dialog::backdrop {
		background: oklch(0.1 0.004 260 / 0.56);
		animation: fade-in 200ms cubic-bezier(0.22, 1, 0.36, 1);
	}
	dialog[open]:modal {
		animation: sheet-in var(--dur-slow) var(--ease-out);
	}
	dialog[open]:not(:modal) {
		animation: enter var(--dur-slow) var(--ease-out);
	}
	/* A docked request that waits for you: the accent holds its edge, solid so it reads at 3:1. */
	dialog[data-attention] {
		border-color: var(--ring);
		box-shadow:
			0 0 0 3px color-mix(in oklab, var(--ember) 16%, transparent),
			var(--elev-2),
			var(--highlight);
	}
	@keyframes sheet-in {
		from {
			transform: translateY(40px);
			opacity: 0;
		}
	}
	@keyframes dialog-in {
		from {
			transform: translateY(8px) scale(0.98);
			opacity: 0;
		}
	}
	@media (min-width: 768px) {
		dialog[open]:modal {
			animation: dialog-in 200ms var(--ease-out);
		}
	}
</style>
