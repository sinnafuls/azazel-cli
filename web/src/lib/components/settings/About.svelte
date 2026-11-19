<script lang="ts">
	import { app } from '$lib/app.svelte';
	import Brand from '../Brand.svelte';
	import * as Item from '$lib/components/ui/item';
	import * as Field from '$lib/components/ui/field';
	import Row from './Row.svelte';

	const rows = $derived<[string, string][]>([
		['Version', app.me?.version ?? 'unknown'],
		['Protocol', String(app.me?.protocol ?? '')],
		[
			'This device',
			app.fullAccess ? 'Full access' : app.canControl ? 'Full control' : 'Watch only'
		],
		[
			'Signed in until',
			app.me?.expires
				? new Date(app.me.expires).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' })
				: ''
		]
	]);
</script>

<Item.Root class="px-0">
	<!-- The stock media stands on the title's line; a mark as tall as both lines is centred on them. -->
	<Item.Media
		class="group-has-data-[slot=item-description]/item:translate-y-0 group-has-data-[slot=item-description]/item:self-center"
	>
		<Brand size={32} word={false} />
	</Item.Media>
	<Item.Content>
		<Item.Title>azazel</Item.Title>
		<Item.Description>A coding agent on your computer, driven from this browser.</Item.Description>
	</Item.Content>
</Item.Root>
<Field.Group class="gap-0">
	{#each rows as [k, v] (k)}
		{#if v}
			<Row label={k}>
				<span class="num shrink-0 text-xs text-muted-foreground" data-about={k}>{v}</span>
			</Row>
		{/if}
	{/each}
</Field.Group>
