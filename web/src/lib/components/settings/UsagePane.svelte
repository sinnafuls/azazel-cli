<script lang="ts">
	// What the accounts have left (each window as a bar, with when it resets) and what this session cost.
	import { onMount } from 'svelte';
	import { hostRpc } from '$lib/host';
	import { ui } from '$lib/ui.svelte';
	import type { AccountUsage, CostReport } from '$lib/protocol';
	import { cost as usd, resetsIn, tokens } from '$lib/format';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import Meter from './Meter.svelte';

	const usage = new Loaded<AccountUsage[]>(() => hostRpc<AccountUsage[]>({ type: 'get_usage' }));
	const cost = new Loaded<CostReport>(() => hostRpc<CostReport>({ type: 'get_cost' }));
	onMount(() => {
		void usage.load();
		void cost.load();
	});
	const c = $derived(cost.data);
	const u = $derived(c?.usage);
</script>

<h3 class="set-h">{ui.session ? 'This session' : 'Session cost'}</h3>
{#if cost.state !== 'ready' || !c}
	<Unavailable of={cost} what="the session cost" />
{:else}
	<dl class="group-card grid grid-cols-2 sm:grid-cols-4 sm:divide-x" data-cost>
		{#each [['Cost', usd(u?.cost_usd)], ['Input', tokens(u?.input ?? 0)], ['Output', tokens(u?.output ?? 0)], ['Cache hits', c.cache_hit_rate == null ? '—' : `${Math.round(c.cache_hit_rate)}%`]] as [k, v] (k)}
			<div class="flex min-w-0 flex-col-reverse px-3.5 py-3">
				<dt class="text-muted-foreground mt-0.5 text-xs">{k}</dt>
				<dd class="num truncate font-mono text-lg font-medium tracking-[-0.03em]">{v}</dd>
			</div>
		{/each}
	</dl>
	<p class="text-muted-foreground num mt-2 text-xs">
		{tokens(u?.cache_read ?? 0)} read from cache, {tokens(u?.cache_write ?? 0)} written to it · {c.requests}
		{c.requests === 1 ? 'request' : 'requests'}.{c.note ? ` ${c.note}` : ''}
	</p>
	{#if c.models?.length}
		<div class="scroll-thin mt-3 overflow-x-auto">
			<table class="w-full min-w-[26rem] text-[13px]">
				<thead>
					<tr class="text-muted-foreground text-xs">
						<th class="py-1.5 pr-3 text-left font-medium">Model</th>
						<th class="px-2 text-right font-medium">Input</th>
						<th class="px-2 text-right font-medium">Requests</th>
						<th class="px-2 text-right font-medium">Cached</th>
						<th class="pl-2 text-right font-medium">Hit rate</th>
					</tr>
				</thead>
				<tbody>
					{#each c.models as m (m.model)}
						<tr class="border-t">
							<td class="max-w-[14rem] truncate py-2 pr-3 font-mono text-xs">{m.model}</td>
							<td class="num px-2 text-right">{tokens(m.input)}</td>
							<td class="num px-2 text-right">{m.requests}</td>
							<td class="num px-2 text-right">{tokens(m.cache_read)}</td>
							<td class="num pl-2 text-right font-medium"
								>{m.hit_rate == null ? '—' : `${Math.round(m.hit_rate)}%`}</td
							>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
{/if}

<h3 class="set-h">Accounts</h3>
{#if usage.state !== 'ready'}
	<Unavailable of={usage} what="account usage" />
{:else if !usage.data?.length}
	<p class="text-muted-foreground py-2 text-[13px]">
		No account reports its limits. Accounts with a subscription show their windows here.
	</p>
{:else}
	{#each usage.data as a (a.provider + a.account)}
		<section class="mb-3" aria-label="{a.provider} {a.account}" data-account={a.provider}>
			<p class="text-[13px] font-medium">
				{a.provider} <span class="text-muted-foreground font-normal">· {a.account}</span>
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
				<p class="text-destructive text-xs" role="status">
					Blocked for another {resetsIn(a.blocked_until)}.
				</p>
			{/if}
		</section>
	{/each}
{/if}
