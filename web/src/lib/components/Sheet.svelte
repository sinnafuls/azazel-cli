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
		/** Marks the sheet with the accent: it is waiting for you. */
		attention?: boolean;
		onclose?: () => void;
		children: Snippet;
		footer?: Snippet;
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
			? 'float mt-auto max-h-[88dvh] rounded-t-2xl border-x-0 border-b-0 md:m-auto md:max-h-[80dvh] md:max-w-md md:rounded-xl md:border'
			: 'surface-2 relative mx-auto mb-3 max-h-[56dvh] w-[calc(100%-4rem)] max-w-[43rem] rounded-xl',
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
					<span class="bg-input/60 h-1 w-9 rounded-full" aria-hidden="true"></span>
				</div>
			{/if}
			<header class="flex shrink-0 items-start gap-3 px-4 pt-3 pb-3 md:px-5 md:pt-4">
				{#if attention}<StateDot state="waiting for you" class="mt-[7px]" />{/if}
				<div class="min-w-0 flex-1">
					<h2
						id={titleId}
						bind:this={heading}
						tabindex="-1"
						class="text-[15px] leading-6 font-semibold tracking-[-0.02em] outline-none"
					>
						{title}
					</h2>
					{#if subtitle}<p class="text-muted-foreground meta mt-0.5 truncate">{subtitle}</p>{/if}
				</div>
				{#if dismissible && onclose}
					<button
						type="button"
						class="btn btn-quiet btn-icon -mt-1.5 -mr-2 md:-mt-1"
						aria-label="Close"
						onclick={onclose}
					>
						<X class="size-4" />
					</button>
				{/if}
			</header>
			<div class="scroll-thin min-h-0 flex-1 overflow-y-auto px-4 pb-4 md:px-5">
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
