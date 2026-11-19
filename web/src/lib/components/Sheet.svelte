<script lang="ts">
	// A titled surface for one task, made of the stock parts. On a wide screen it is a Dialog; on a
	// phone a Drawer from the bottom that is swiped down to close. A `docked` sheet on a wide screen
	// is a Card in the reading column above the composer, so the transcript stays readable while you
	// decide. The close button is always at the top right; a step back is at the top left.
	import type { Snippet } from 'svelte';
	import * as Dialog from '$lib/components/ui/dialog';
	import * as Drawer from '$lib/components/ui/drawer';
	import * as Card from '$lib/components/ui/card';
	import { Button } from '$lib/components/ui/button';
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
		class: className,
		...rest
	}: {
		open: boolean;
		title: string;
		subtitle?: string;
		docked?: boolean;
		dismissible?: boolean;
		/** Marks the sheet with the accent: it is waiting for you. */
		attention?: boolean;
		onclose?: () => void;
		/** Shows a back button on the left: the sheet is a step into something. */
		onback?: () => void;
		children: Snippet;
		/** Buttons, laid out by the footer: stacked on a phone, in a row at the right otherwise. */
		footer?: Snippet;
		/** Buttons left of the close button. */
		actions?: Snippet;
		class?: string;
		[k: `data-${string}`]: string | boolean | undefined;
	} = $props();

	const inline = $derived(docked && viewport.wide);
	const closable = $derived(dismissible && !!onclose);
	function change(v: boolean) {
		if (!v && closable) onclose?.();
	}

	// A docked request is not a modal, so nothing takes the focus for it: do it, or a keyboard and a
	// screen reader user would not know it is there.
	let dockedHead = $state<HTMLElement | null>(null);
	$effect(() => {
		if (inline && open) dockedHead?.focus({ preventScroll: true });
	});
	const uid = $props.id();
</script>

<!-- One row: the title's line is as tall as the buttons beside it, so their centres are the same. -->
{#snippet head(kind: 'card' | 'dialog' | 'drawer')}
	<div class="flex shrink-0 items-start gap-2 px-4 pt-4 pb-3" data-slot="sheet-head">
		{#if onback}
			<Button variant="ghost" size="icon-sm" class="-ml-1.5" aria-label="Back" onclick={onback}>
				<ChevronLeft />
			</Button>
		{:else if attention}
			<span class="grid h-7 shrink-0 place-items-center"><StateDot state="waiting for you" /></span>
		{/if}
		<div class="min-w-0 flex-1">
			{#if kind === 'dialog'}
				<Dialog.Title class="truncate leading-7">{title}</Dialog.Title>
				{#if subtitle}<Dialog.Description class="truncate text-xs">{subtitle}</Dialog.Description
					>{/if}
			{:else if kind === 'drawer'}
				<Drawer.Title class="truncate leading-7">{title}</Drawer.Title>
				{#if subtitle}<Drawer.Description class="truncate text-xs">{subtitle}</Drawer.Description
					>{/if}
			{:else}
				<Card.Title
					bind:ref={dockedHead}
					id="{uid}-title"
					role="heading"
					aria-level={2}
					tabindex={-1}
					class="truncate leading-7 outline-none"
				>
					{title}
				</Card.Title>
				{#if subtitle}<Card.Description class="truncate text-xs">{subtitle}</Card.Description>{/if}
			{/if}
		</div>
		{#if actions}{@render actions()}{/if}
		{#if closable}
			<Button variant="ghost" size="icon-sm" class="-mr-1.5" aria-label="Close" onclick={onclose}>
				<X />
			</Button>
		{/if}
	</div>
{/snippet}

{#snippet body()}
	<div class="scroll-thin min-h-0 flex-1 overflow-y-auto px-4 pb-4" data-slot="sheet-body">
		{@render children()}
	</div>
{/snippet}

{#if inline}
	{#if open}
		<Card.Root
			role="group"
			aria-labelledby="{uid}-title"
			class={cn(
				'mx-auto mb-3 max-h-[56dvh] w-full gap-0 py-0 duration-200 animate-in fade-in-0 slide-in-from-bottom-2',
				attention && 'ring-ring/60',
				className
			)}
			data-attention={attention ? '' : undefined}
			{...rest}
		>
			{@render head('card')}
			{@render body()}
			{#if footer}
				<Card.Footer class="shrink-0 flex-col-reverse gap-2 sm:flex-row sm:justify-end"
					>{@render footer()}</Card.Footer
				>
			{/if}
			<!-- The focus move names the request; this line says it waits for an answer. -->
			<p class="sr-only" role="status">
				{attention ? `${title}. Waiting for your answer.` : title}
			</p>
		</Card.Root>
	{/if}
{:else if viewport.wide}
	<Dialog.Root {open} onOpenChange={change}>
		<Dialog.Content
			showCloseButton={false}
			interactOutsideBehavior={closable ? 'close' : 'ignore'}
			escapeKeydownBehavior={closable ? 'close' : 'ignore'}
			class={cn(
				'flex max-h-[min(82dvh,48rem)] flex-col gap-0 overflow-hidden p-0 sm:max-w-lg',
				className
			)}
			{...rest}
		>
			{@render head('dialog')}
			{@render body()}
			{#if footer}
				<Dialog.Footer class="mx-0 mb-0 shrink-0">{@render footer()}</Dialog.Footer>
			{/if}
		</Dialog.Content>
	</Dialog.Root>
{:else}
	<Drawer.Root {open} onOpenChange={change} dismissible={closable} shouldScaleBackground={false}>
		<Drawer.Content
			class={cn('data-[vaul-drawer-direction=bottom]:max-h-[90dvh]', className)}
			{...rest}
		>
			{@render head('drawer')}
			{@render body()}
			{#if footer}
				<Drawer.Footer
					class="shrink-0 flex-col-reverse border-t bg-muted/50 pb-[max(1rem,env(safe-area-inset-bottom))]"
					>{@render footer()}</Drawer.Footer
				>
			{/if}
		</Drawer.Content>
	</Drawer.Root>
{/if}
