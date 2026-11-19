<script lang="ts">
	// A Button (or, with `child`, any element) with a Tooltip: not the native `title`, so that it
	// looks like the rest, shows on keyboard focus and is described to a screen reader.
	// A tooltip is a help, never the only place a fact is shown: touch has no hover.
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
		child,
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
		{#if child}
			<Tooltip.Trigger {child} />
		{:else}
			<Tooltip.Trigger>
				{#snippet child({ props })}
					<!-- Merged, not spread one over the other: the caller's handlers (a held Stop listens to
					     the pointer) and the tooltip's both run. Button's props are a union with a link's, so
					     the result is typed loosely. -->
					<!-- `type` last: the trigger's props carry `type="button"`, which would turn a submit
					     button (the composer's Send) into one that does nothing. -->
					<Button {variant} {size} class={className} {...mergeProps(rest, props) as object} {type}>
						{@render children?.()}
					</Button>
				{/snippet}
			</Tooltip.Trigger>
		{/if}
		<Tooltip.Content {side}>{text}</Tooltip.Content>
	</Tooltip.Root>
</Tooltip.Provider>
