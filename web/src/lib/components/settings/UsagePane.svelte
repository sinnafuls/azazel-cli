<script lang="ts">
	// What the accounts have left (each window as a bar, with when it resets) and what this session cost.
	import { onMount } from 'svelte';
	import { hostRpc } from '$lib/host';
	import { ui } from '$lib/ui.svelte';
	import type { AccountUsage, CostReport } from '$lib/protocol';
	import { cost as usd, resetsIn, tokens } from '$lib/format';
	import * as Card from '$lib/components/ui/card';
	import * as Table from '$lib/components/ui/table';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import Meter from './Meter.svelte';
	import Section from './Section.svelte';

	const usage = new Loaded<AccountUsage[]>(() => hostRpc<AccountUsage[]>({ type: 'get_usage' }));
	const cost = new Loaded<CostReport>(() => hostRpc<CostReport>({ type: 'get_cost' }));
	onMount(() => {
		void usage.load();
		void cost.load();
	});
	const c = $derived(cost.data);
	const u = $derived(c?.usage);
</script>

<Section title={ui.session ? 'This session' : 'Session cost'}>
	{#if cost.state !== 'ready' || !c}
		<Unavailable of={cost} what="the session cost" />
	{:else}
		<Card.Root size="sm" data-cost>
			<Card.Content>
				<dl class="grid grid-cols-2 gap-3 sm:grid-cols-4">
					{#each [['Cost', usd(u?.cost_usd)], ['Input', tokens(u?.input ?? 0)], ['Output', tokens(u?.output ?? 0)], ['Cache hits', c.cache_hit_rate == null ? '—' : `${Math.round(c.cache_hit_rate)}%`]] as [k, v] (k)}
						<div class="flex min-w-0 flex-col-reverse">
							<dt class="text-xs text-muted-foreground">{k}</dt>
							<dd class="num truncate text-base font-medium">{v}</dd>
						</div>
					{/each}
				</dl>
			</Card.Content>
		</Card.Root>
		<p class="num mt-2 text-xs text-muted-foreground">
			{tokens(u?.cache_read ?? 0)} read from cache, {tokens(u?.cache_write ?? 0)} written to it · {c.requests}
			{c.requests === 1 ? 'request' : 'requests'}.{c.note ? ` ${c.note}` : ''}
		</p>
		{#if c.models?.length}
			<Table.Root class="mt-3 text-xs">
				<Table.Header>
					<Table.Row class="hover:bg-transparent">
						<Table.Head class="h-8 pl-0 text-muted-foreground">Model</Table.Head>
						<Table.Head class="h-8 text-right text-muted-foreground">Input</Table.Head>
						<Table.Head class="h-8 text-right text-muted-foreground">Requests</Table.Head>
						<Table.Head class="h-8 text-right text-muted-foreground">Cached</Table.Head>
						<Table.Head class="h-8 pr-0 text-right text-muted-foreground">Hit rate</Table.Head>
					</Table.Row>
				</Table.Header>
				<Table.Body>
					{#each c.models as m (m.model)}
						<Table.Row>
							<Table.Cell class="max-w-56 truncate pl-0 font-mono">{m.model}</Table.Cell>
							<Table.Cell class="num text-right">{tokens(m.input)}</Table.Cell>
							<Table.Cell class="num text-right">{m.requests}</Table.Cell>
							<Table.Cell class="num text-right">{tokens(m.cache_read)}</Table.Cell>
							<Table.Cell class="num pr-0 text-right font-medium"
								>{m.hit_rate == null ? '—' : `${Math.round(m.hit_rate)}%`}</Table.Cell
							>
						</Table.Row>
					{/each}
				</Table.Body>
			</Table.Root>
		{/if}
	{/if}
</Section>

<Section title="Accounts">
	{#if usage.state !== 'ready'}
		<Unavailable of={usage} what="account usage" />
	{:else if !usage.data?.length}
		<p class="py-2 text-xs text-muted-foreground">
			No account reports its limits. Accounts with a subscription show their windows here.
		</p>
	{:else}
		{#each usage.data as a (a.provider + a.account)}
			<section class="mb-3" aria-label="{a.provider} {a.account}" data-account={a.provider}>
				<p class="text-sm font-medium">
					{a.provider} <span class="font-normal text-muted-foreground">· {a.account}</span>
				</p>
				{#each a.windows as w (w.name + (w.scope ?? ''))}
					<Meter
						label="{w.name}{w.scope ? ` · ${w.scope}` : ''}"
						value={w.used * 100}
						note={w.resets_at ? `Resets in ${resetsIn(w.resets_at)}` : ''}
						bad={w.status === 'rejected'}
					/>
				{/each}
				{#if a.blocked_until}
					<p class="text-xs text-destructive" role="status">
						Blocked for another {resetsIn(a.blocked_until)}.
					</p>
				{/if}
			</section>
		{/each}
	{/if}
</Section>
