<script lang="ts">
	import { COMPOSER_KEYS, isMac, keysOf, SHORTCUTS } from '$lib/shortcuts';

	const mac = isMac(typeof navigator === 'undefined' ? '' : navigator.platform);
</script>

{#snippet keys(combo: string)}
	<span class="flex shrink-0 gap-1">
		{#each keysOf(combo, mac) as k (k)}<kbd class="kbd">{k}</kbd>{/each}
	</span>
{/snippet}

<h3 class="set-h">Anywhere</h3>
<dl>
	{#each SHORTCUTS as s (s.id)}
		<div class="set-row">
			<dt class="set-label">{s.label}</dt>
			<dd>{@render keys(s.combo)}</dd>
		</div>
	{/each}
</dl>

<h3 class="set-h">In the message box</h3>
<dl>
	{#each COMPOSER_KEYS as [combo, label] (combo)}
		<div class="set-row">
			<dt class="set-label">{label}</dt>
			<dd>{@render keys(combo)}</dd>
		</div>
	{/each}
</dl>
