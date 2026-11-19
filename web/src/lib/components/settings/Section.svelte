<script lang="ts">
	// A titled group of settings rows: a quiet heading, an optional control beside it, and the rows
	// in a Field.Group (whose width decides whether a responsive row stacks).
	import type { Snippet } from 'svelte';
	import * as Field from '$lib/components/ui/field';
	import { cn } from '$lib/utils';

	let {
		title,
		id,
		action,
		children,
		class: className
	}: {
		title: string;
		id?: string;
		action?: Snippet;
		children: Snippet;
		class?: string;
	} = $props();
</script>

<section class={cn('mt-6 first:mt-0', className)}>
	<div class="mb-1 flex min-h-8 items-center justify-between gap-3">
		<h3 {id} class="text-xs font-medium text-muted-foreground">{title}</h3>
		{@render action?.()}
	</div>
	<Field.Group class="gap-0">{@render children()}</Field.Group>
</section>
