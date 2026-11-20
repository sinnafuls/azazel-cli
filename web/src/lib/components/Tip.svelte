<script lang="ts" module>
	// Whether the focus that moves now is moved by Tab. Tab moves it while its keydown is handled,
	// so the mark is taken back in the next task: focus that a script gives later (a sheet that
	// closes and hands it back to the button that opened it) is not the reader's doing.
	// `:focus-visible` cannot tell the two apart: it matches the button after Escape closed the sheet.
	let tabbing = false;
	if (typeof document !== 'undefined')
		document.addEventListener(
			'keydown',
			(e) => {
				tabbing = e.key === 'Tab';
				if (tabbing) setTimeout(() => (tabbing = false));
			},
			true
		);

	/** The trigger's props, with a focus handler that opens the tooltip only for focus Tab brought. */
	function quiet(props: Record<string, unknown>): Record<string, unknown> {
		const onfocus = props.onfocus as ((e: FocusEvent) => void) | undefined;
		return {
			...props,
			onfocus(this: unknown, e: FocusEvent) {
				if (tabbing) onfocus?.call(this, e);
			}
		};
	}
</script>

<script lang="ts">
	// A Button (or, with `child`, any element) with a Tooltip: not the native `title`, so that it
	// looks like the rest, shows when Tab reaches it and is described to a screen reader.
	// A tooltip is a help, never the only place a fact is shown: touch has no hover.
	// It opens for the pointer and for Tab, and not for focus a script moved here: a sheet that
	// closes gives the focus back to its button, and the tooltip then covered what was under it.
	import type { Snippet } from 'svelte';
	import type { HTMLButtonAttributes } from 'svelte/elements';
	import { mergeProps } from 'bits-ui';
	import * as Tooltip from '$lib/components/ui/tooltip';
	import { Button, type ButtonSize, type ButtonVariant } from '$lib/components/ui/button';

	let {
		text,
		side = 'top',
		variant = 'ghost',
		size = 'icon-sm',
		class: className,
		type = 'button',
		children,
		// Under another name: the snippet handed to the trigger below is called `child` too.
		child: own,
		...rest
	}: Omit<HTMLButtonAttributes, 'children' | 'title' | 'id' | 'class'> & {
		id?: string;
		text: string;
		side?: 'top' | 'right' | 'bottom' | 'left';
		variant?: ButtonVariant;
		size?: ButtonSize;
		class?: string;
		children?: Snippet;
		/** Renders the trigger itself, with the props to spread on it (a link, a menu button). */
		child?: Snippet<[{ props: Record<string, unknown> }]>;
	} = $props();
</script>

<Tooltip.Provider delayDuration={200}>
	<Tooltip.Root>
		{#if own}
			<Tooltip.Trigger>
				{#snippet child({ props })}
					{@render own({ props: quiet(props) })}
				{/snippet}
			</Tooltip.Trigger>
		{:else}
			<Tooltip.Trigger>
				{#snippet child({ props })}
					<!-- Merged, not spread one over the other: the caller's handlers (a held Stop listens to
					     the pointer) and the tooltip's both run. Button's props are a union with a link's, so
					     the result is typed loosely. -->
					<!-- `type` last: the trigger's props carry `type="button"`, which would turn a submit
					     button (the composer's Send) into one that does nothing. -->
					<Button
						{variant}
						{size}
						class={className}
						{...mergeProps(rest, quiet(props)) as object}
						{type}
					>
						{@render children?.()}
					</Button>
				{/snippet}
			</Tooltip.Trigger>
		{/if}
		<Tooltip.Content {side}>{text}</Tooltip.Content>
	</Tooltip.Root>
</Tooltip.Provider>
