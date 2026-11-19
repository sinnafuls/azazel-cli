<script lang="ts">
	// One setting: its name and what it does at the left, its control at the right. It is a Field;
	// `responsive` stacks the control under the text when the pane is narrow, `vertical` always.
	import type { Snippet } from 'svelte';
	import * as Field from '$lib/components/ui/field';
	import { cn } from '$lib/utils';

	let {
		label,
		hint,
		id,
		for: htmlFor,
		orientation = 'horizontal',
		mono = false,
		badges,
		extra,
		children,
		class: className,
		...rest
	}: {
		label: string;
		hint?: string;
		/** The id of the name, for a control that is labelled by it. */
		id?: string;
		/** The id of the control the name is the label of. */
		for?: string;
		orientation?: 'horizontal' | 'vertical' | 'responsive';
		mono?: boolean;
		/** Badges after the name. */
		badges?: Snippet;
		/** More lines under the hint. */
		extra?: Snippet;
		children?: Snippet;
		class?: string;
		[k: `data-${string}`]: string | boolean | undefined;
	} = $props();
</script>

<Field.Field {orientation} class={cn('border-b py-3 last:border-b-0', className)} {...rest}>
	<Field.Content class="min-w-0">
		{#if htmlFor}
			<Field.Label for={htmlFor} {id} class={cn('flex-wrap leading-5', mono && 'font-mono')}>
				{label}{@render badges?.()}
			</Field.Label>
		{:else}
			<Field.Title {id} class={cn('flex-wrap leading-5', mono && 'font-mono')}>
				{label}{@render badges?.()}
			</Field.Title>
		{/if}
		{#if hint}<Field.Description class="text-xs">{hint}</Field.Description>{/if}
		{@render extra?.()}
	</Field.Content>
	{@render children?.()}
</Field.Field>
