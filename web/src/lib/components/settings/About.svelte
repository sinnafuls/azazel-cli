<script lang="ts">
	import { app } from '$lib/app.svelte';
	import Brand from '../Brand.svelte';

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

<div class="flex items-center gap-3 py-3">
	<Brand size={36} word={false} />
	<div>
		<p class="text-[15px] font-semibold tracking-[-0.02em]">azazel</p>
		<p class="text-muted-foreground text-[13px]">
			A coding agent on your computer, driven from this browser.
		</p>
	</div>
</div>
<dl>
	{#each rows as [k, v] (k)}
		{#if v}
			<div class="set-row">
				<dt class="set-label">{k}</dt>
				<dd class="text-muted-foreground num font-mono text-[13px]" data-about={k}>{v}</dd>
			</div>
		{/if}
	{/each}
</dl>
