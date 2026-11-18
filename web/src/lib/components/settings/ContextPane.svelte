<script lang="ts">
	// What fills the context window: one stacked bar, then the same as a table, and Compact.
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import type { ContextReport } from '$lib/protocol';
	import { tokens } from '$lib/format';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import Shrink from '@lucide/svelte/icons/shrink';

	const report = new Loaded<ContextReport | null>(() =>
		hostRpc<ContextReport | null>({ type: 'get_context' })
	);
	onMount(() => void report.load());

	const r = $derived(report.data);
	const used = $derived(r ? r.tokens : 0);
	const window = $derived(r?.window || 0);
	const pct = (n: number) => (window ? (n / window) * 100 : 0);
	const cats = $derived(
		(r?.categories ?? []).map((c, i) => ({
			...c,
			size: c.tokens,
			tone: i % 8
		}))
	);
	let compacting = $state(false);

	async function compact() {
		const s = ui.session;
		if (!s) return;
		compacting = true;
		try {
			await s.compact();
			toasts.push('Compacting the conversation.', 'info', 2500);
			setTimeout(() => void report.load(true), 1200);
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		} finally {
			compacting = false;
		}
	}
</script>

{#if report.state === 'ready' && !r}
	<p class="text-muted-foreground py-2 text-[13px] leading-relaxed" data-context-empty>
		Nothing has been sent to the model yet. The breakdown appears after the first request.
	</p>
{:else if report.state !== 'ready' || !r}
	<Unavailable of={report} what="the context report" />
{:else}
	<div class="flex items-end justify-between gap-4 pt-1">
		<div class="min-w-0">
			<p class="num font-mono text-2xl font-medium tracking-[-0.03em]" data-context-used>
				{tokens(used)}
				<span class="text-muted-foreground text-sm font-normal">of {tokens(window)} tokens</span>
			</p>
			<p class="text-muted-foreground mt-0.5 truncate text-xs">
				{Math.round(pct(used))}% used ·
				<span class="font-mono">{r.model ?? 'no model yet'}</span>{r.reported ? '' : ' · estimated'}
			</p>
		</div>
		{#if ui.session && app.canControl}
			<button
				type="button"
				class="btn btn-secondary shrink-0"
				disabled={compacting || ui.session.busy}
				title={ui.session.busy ? 'Wait until the agent is done' : 'Summarise the conversation'}
				onclick={compact}><Shrink class="size-4" /> Compact</button
			>
		{/if}
	</div>

	<div
		class="bar mt-4"
		role="img"
		aria-label="Context: {Math.round(pct(used))}% used. {cats
			.map((c) => `${c.label} ${tokens(c.size)}`)
			.join(', ')}"
		data-context-bar
	>
		{#each cats as c (c.kind + c.name)}
			<span class="tone-{c.tone}" style="width: {pct(c.size)}%" title="{c.label}: {tokens(c.size)}"
			></span>
		{/each}
	</div>

	<table class="mt-4 w-full text-[13px]">
		<thead>
			<tr class="text-muted-foreground text-xs">
				<th class="py-1.5 text-left font-medium">Part</th>
				<th class="px-2 text-right font-medium">Items</th>
				<th class="px-2 text-right font-medium">Tokens</th>
				<th class="w-14 pl-2 text-right font-medium">Share</th>
			</tr>
		</thead>
		<tbody>
			{#each cats as c (c.kind + c.name)}
				<tr class="border-t">
					<td class="py-2">
						<span class="flex items-center gap-2"
							><span class="dot tone-{c.tone}" aria-hidden="true"></span>{c.label}</span
						>
					</td>
					<td class="num text-muted-foreground px-2 text-right">{c.items}</td>
					<td class="num px-2 text-right">{r?.reported ? '' : '~'}{tokens(c.size)}</td>
					<td class="num text-muted-foreground pl-2 text-right">{pct(c.size).toFixed(1)}%</td>
				</tr>
			{/each}
			<tr class="border-t">
				<td class="text-muted-foreground py-2">
					<span class="flex items-center gap-2"
						><span class="dot free" aria-hidden="true"></span>Free</span
					>
				</td>
				<td></td>
				<td class="num text-muted-foreground px-2 text-right"
					>{tokens(Math.max(window - used, 0))}</td
				>
				<td class="num text-muted-foreground pl-2 text-right"
					>{Math.max(100 - pct(used), 0).toFixed(1)}%</td
				>
			</tr>
		</tbody>
	</table>
{/if}

<style>
	/* A chart's own palette: eight hues at one lightness, each 3:1 against the track in both themes. */
	.bar,
	table {
		--t0: light-dark(oklch(0.52 0.14 255), oklch(0.74 0.11 250));
		--t1: light-dark(oklch(0.52 0.12 155), oklch(0.76 0.12 155));
		--t2: light-dark(oklch(0.55 0.12 70), oklch(0.78 0.12 80));
		--t3: light-dark(oklch(0.5 0.14 305), oklch(0.74 0.11 305));
		--t4: light-dark(oklch(0.52 0.1 200), oklch(0.76 0.09 200));
		--t5: light-dark(oklch(0.52 0.14 20), oklch(0.74 0.1 20));
		--t6: light-dark(oklch(0.5 0.1 120), oklch(0.76 0.1 120));
		--t7: light-dark(oklch(0.5 0.13 340), oklch(0.74 0.1 340));
	}
	.bar {
		display: flex;
		gap: 2px;
		height: 0.75rem;
		overflow: hidden;
		border-radius: 999px;
		background: var(--hover);
		box-shadow: inset 0 0 0 1px var(--border);
	}
	.bar span {
		min-width: 2px;
		height: 100%;
	}
	.dot {
		width: 0.5rem;
		height: 0.5rem;
		flex-shrink: 0;
		border-radius: 2px;
	}
	.free {
		background: var(--hover);
		box-shadow: inset 0 0 0 1px var(--input);
	}
	.tone-0 {
		background: var(--t0);
	}
	.tone-1 {
		background: var(--t1);
	}
	.tone-2 {
		background: var(--t2);
	}
	.tone-3 {
		background: var(--t3);
	}
	.tone-4 {
		background: var(--t4);
	}
	.tone-5 {
		background: var(--t5);
	}
	.tone-6 {
		background: var(--t6);
	}
	.tone-7 {
		background: var(--t7);
	}
</style>
