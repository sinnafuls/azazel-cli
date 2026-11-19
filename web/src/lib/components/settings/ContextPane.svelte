<script lang="ts">
	// What fills the context window: one stacked bar, then the same as a Table, and Compact.
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import type { ContextReport } from '$lib/protocol';
	import { tokens } from '$lib/format';
	import * as Table from '$lib/components/ui/table';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import Tip from '$lib/components/Tip.svelte';
	import Shrink from '@lucide/svelte/icons/shrink';

	const report = new Loaded<ContextReport | null>(() =>
		hostRpc<ContextReport | null>({ type: 'get_context' })
	);
	onMount(() => void report.load());

	// The chart colours of the design system: the accent, then its greys.
	const TONES = ['bg-chart-1', 'bg-chart-2', 'bg-chart-3', 'bg-chart-4', 'bg-chart-5'];

	const r = $derived(report.data);
	const used = $derived(r ? r.tokens : 0);
	const window = $derived(r?.window || 0);
	const pct = (n: number) => (window ? (n / window) * 100 : 0);
	const cats = $derived(
		(r?.categories ?? []).map((c, i) => ({
			...c,
			size: c.tokens,
			tone: TONES[i % TONES.length]
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
	<p class="py-2 text-xs leading-relaxed text-muted-foreground" data-context-empty>
		Nothing has been sent to the model yet. The breakdown appears after the first request.
	</p>
{:else if report.state !== 'ready' || !r}
	<Unavailable of={report} what="the context report" />
{:else}
	<div class="flex items-end justify-between gap-4">
		<div class="min-w-0">
			<p class="num text-2xl font-medium tracking-tight" data-context-used>
				{tokens(used)}
				<span class="text-sm font-normal text-muted-foreground">of {tokens(window)} tokens</span>
			</p>
			<p class="mt-0.5 truncate text-xs text-muted-foreground">
				{Math.round(pct(used))}% used ·
				<span class="font-mono">{r.model ?? 'no model yet'}</span>{r.reported ? '' : ' · estimated'}
			</p>
		</div>
		{#if ui.session && app.canControl}
			<div class="flex shrink-0 flex-col items-end gap-1">
				<Tip
					text="Summarise the conversation"
					variant="outline"
					size="default"
					disabled={compacting || ui.session.busy}
					onclick={compact}><Shrink /> Compact</Tip
				>
				{#if ui.session.busy}
					<span class="text-xs text-muted-foreground">Wait until the agent is done</span>
				{/if}
			</div>
		{/if}
	</div>

	<!-- No stock component is a stacked bar (Progress has one value): the track of a Progress, with
	     one segment per part in the chart colours. -->
	<div
		class="mt-4 flex h-1.5 gap-0.5 overflow-hidden rounded-full bg-muted"
		role="img"
		aria-label="Context: {Math.round(pct(used))}% used. {cats
			.map((c) => `${c.label} ${tokens(c.size)}`)
			.join(', ')}"
		data-context-bar
	>
		{#each cats as c (c.kind + c.name)}
			<span
				class="h-full min-w-0.5 {c.tone}"
				style="width: {pct(c.size)}%"
				title="{c.label}: {tokens(c.size)}"
			></span>
		{/each}
	</div>

	<Table.Root class="mt-4 text-xs">
		<Table.Header>
			<Table.Row class="hover:bg-transparent">
				<Table.Head class="h-8 pl-0 text-muted-foreground">Part</Table.Head>
				<Table.Head class="h-8 text-right text-muted-foreground">Items</Table.Head>
				<Table.Head class="h-8 text-right text-muted-foreground">Tokens</Table.Head>
				<Table.Head class="h-8 w-14 pr-0 text-right text-muted-foreground">Share</Table.Head>
			</Table.Row>
		</Table.Header>
		<Table.Body>
			{#each cats as c (c.kind + c.name)}
				<Table.Row>
					<Table.Cell class="pl-0">
						<span class="flex items-center gap-2"
							><span class="size-2 shrink-0 rounded-full {c.tone}" aria-hidden="true"
							></span>{c.label}</span
						>
					</Table.Cell>
					<Table.Cell class="num text-right text-muted-foreground">{c.items}</Table.Cell>
					<Table.Cell class="num text-right">{r?.reported ? '' : '~'}{tokens(c.size)}</Table.Cell>
					<Table.Cell class="num pr-0 text-right text-muted-foreground"
						>{pct(c.size).toFixed(1)}%</Table.Cell
					>
				</Table.Row>
			{/each}
			<Table.Row>
				<Table.Cell class="pl-0 text-muted-foreground">
					<span class="flex items-center gap-2"
						><span
							class="size-2 shrink-0 rounded-full bg-muted ring-1 ring-input"
							aria-hidden="true"
						></span>Free</span
					>
				</Table.Cell>
				<Table.Cell></Table.Cell>
				<Table.Cell class="num text-right text-muted-foreground"
					>{tokens(Math.max(window - used, 0))}</Table.Cell
				>
				<Table.Cell class="num pr-0 text-right text-muted-foreground"
					>{Math.max(100 - pct(used), 0).toFixed(1)}%</Table.Cell
				>
			</Table.Row>
		</Table.Body>
	</Table.Root>
{/if}
