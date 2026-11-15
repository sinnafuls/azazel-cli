<script lang="ts">
	// A native <dialog>: focus is trapped, Escape works, the backdrop is free. On a phone it is a
	// bottom sheet that covers the screen modally; on a wide screen a `docked` sheet sits in the flow
	// above the composer so the transcript stays readable while you decide.
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
</script>

<dialog
	bind:this={el}
	class={cn(
		'sheet m-0 w-full max-w-none bg-card p-0 text-foreground',
		modal
			? 'mt-auto max-h-[88dvh] rounded-t-xl border border-b-0 shadow-2xl md:m-auto md:max-h-[80dvh] md:max-w-xl md:rounded-xl md:border-b'
			: 'relative max-h-[50dvh] rounded-lg border',
		className
	)}
	aria-labelledby="sheet-title"
	oncancel={(e) => {
		if (!dismissible) e.preventDefault();
		else onclose?.();
	}}
>
	{#if open}
		<div class="flex max-h-[inherit] flex-col" style="max-height: inherit">
			{#if modal}
				<div
					class="mx-auto mt-2 h-1 w-9 shrink-0 rounded-full bg-input md:hidden"
					aria-hidden="true"
				></div>
			{/if}
			<header class="flex shrink-0 items-start gap-3 px-4 pt-3 pb-2">
				<div class="min-w-0 flex-1">
					<h2
						id="sheet-title"
						bind:this={heading}
						tabindex="-1"
						class="text-[15px] leading-6 font-semibold outline-none"
					>
						{title}
					</h2>
					{#if subtitle}<p class="text-muted-foreground truncate text-xs">{subtitle}</p>{/if}
				</div>
				{#if dismissible && onclose}
					<button
						type="button"
						class="text-muted-foreground hover:bg-raised hover:text-foreground -mt-1 -mr-2 grid size-11 place-items-center rounded-md md:size-8"
						aria-label="Close"
						onclick={onclose}
					>
						<X class="size-4" />
					</button>
				{/if}
			</header>
			<div class="scroll-thin min-h-0 flex-1 overflow-y-auto px-4 pb-3">{@render children()}</div>
			{#if footer}
				<footer class="shrink-0 border-t px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]">
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
		background: rgb(0 0 0 / 0.55);
		animation: fade-in 160ms ease-out;
	}
	dialog[open]:modal {
		animation: sheet-up 190ms cubic-bezier(0.2, 0.8, 0.2, 1);
	}
</style>
