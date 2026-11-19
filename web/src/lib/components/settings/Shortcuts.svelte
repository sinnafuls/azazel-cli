<script lang="ts">
	import { COMPOSER_KEYS, isMac, keysOf, SHORTCUTS } from '$lib/shortcuts';
	import * as Kbd from '$lib/components/ui/kbd';
	import Section from './Section.svelte';
	import Row from './Row.svelte';

	const mac = isMac(typeof navigator === 'undefined' ? '' : navigator.platform);
</script>

{#snippet keys(combo: string)}
	<Kbd.Group class="shrink-0">
		{#each keysOf(combo, mac) as k (k)}<Kbd.Root>{k}</Kbd.Root>{/each}
	</Kbd.Group>
{/snippet}

<Section title="Anywhere">
	{#each SHORTCUTS as s (s.id)}
		<Row label={s.label}>{@render keys(s.combo)}</Row>
	{/each}
</Section>

<Section title="In the message box">
	{#each COMPOSER_KEYS as [combo, label] (combo)}
		<Row {label}>{@render keys(combo)}</Row>
	{/each}
</Section>
