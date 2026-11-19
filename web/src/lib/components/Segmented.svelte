<script lang="ts" generics="T extends string">
	// One choice among a few, as a row of segments: the ToggleGroup, outlined and joined. With one
	// value it is a radio group (arrows move, one tab stop); a pressed segment cannot be unpressed,
	// because there is always a choice.
	import * as ToggleGroup from '$lib/components/ui/toggle-group';
	import { cn } from '$lib/utils';

	let {
		value = $bindable(),
		options,
		label,
		labelledby,
		onchange,
		class: className
	}: {
		value: T;
		options: { value: T; label: string; disabled?: boolean; class?: string }[];
		label?: string;
		labelledby?: string;
		onchange?: (v: T) => void;
		class?: string;
	} = $props();
</script>

<ToggleGroup.Root
	type="single"
	variant="outline"
	bind:value={
		() => value,
		(v) => {
			if (!v || v === value) return;
			value = v as T;
			onchange?.(v as T);
		}
	}
	aria-label={label}
	aria-labelledby={labelledby}
	class={cn('w-full', className)}
>
	{#each options as o (o.value)}
		<ToggleGroup.Item
			value={o.value}
			disabled={o.disabled}
			class={cn(
				// Each segment starts from the width of its label and they share what is left, so a long
				// label ("Follow terminal") is not squeezed into a third of the row.
				'min-w-0 flex-auto text-muted-foreground data-[state=on]:text-foreground',
				o.class
			)}
		>
			{o.label}
		</ToggleGroup.Item>
	{/each}
</ToggleGroup.Root>
