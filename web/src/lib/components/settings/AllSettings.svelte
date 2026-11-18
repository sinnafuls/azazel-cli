<script lang="ts">
	// Every setting the computer reports (`get_settings`), by page and group, with search. The
	// control follows the setting's kind; a change is saved to the chosen scope.
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import { ui } from '$lib/ui.svelte';
	import type { SaveTo, SettingChanged, SettingList, SettingRow } from '$lib/protocol';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import ListEditor from './ListEditor.svelte';
	import ScopeSelect from './ScopeSelect.svelte';
	import Search from '@lucide/svelte/icons/search';
	import RotateCcw from '@lucide/svelte/icons/rotate-ccw';

	const rows = new Loaded<SettingList>(() => hostRpc<SettingList>({ type: 'get_settings' }));
	onMount(() => void rows.load());

	let query = $state('');
	let page = $state('');
	let scope = $state<SaveTo>('session');

	/** Who set the value in effect, as the tag says it. */
	const SOURCE: Record<string, string> = {
		session: 'this session',
		flag: 'command line',
		env: 'environment',
		local: 'local config',
		project: 'this project',
		home: 'every session'
	};
	const all = $derived(rows.data?.settings ?? []);
	const pages = $derived(rows.data?.pages ?? []);
	/** A change for this session only takes the settings a running session applies at once. */
	const locked = (r: SettingRow) =>
		!app.canControl || (scope === 'session' && !r.live) || (r.gated && !app.fullAccess);
	const shown = $derived.by(() => {
		const words = query.toLowerCase().split(/\s+/).filter(Boolean);
		return all.filter((r) => {
			if (page && !words.length && r.page !== page) return false;
			const hay = `${r.label} ${r.path} ${r.description} ${r.group} ${r.page}`.toLowerCase();
			return words.every((w) => hay.includes(w));
		});
	});
	/** page -> group -> rows, in the order the computer sent them. */
	const tree = $derived.by(() => {
		const out: { page: string; groups: { group: string; rows: SettingRow[] }[] }[] = [];
		for (const r of shown) {
			let p = out.find((x) => x.page === r.page);
			if (!p) out.push((p = { page: r.page, groups: [] }));
			let g = p.groups.find((x) => x.group === r.group);
			if (!g) p.groups.push((g = { group: r.group, rows: [] }));
			g.rows.push(r);
		}
		return out;
	});

	const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);
	const list = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : []);
	const idOf = (r: SettingRow) => `set-${r.path.replace(/[^\w-]/g, '-')}`;

	/** `value: null` takes the setting out of the chosen scope. */
	async function set(r: SettingRow, value: unknown) {
		if (value !== null && same(r.value, value)) return;
		try {
			const done = await hostRpc<SettingChanged>({
				type: 'set_setting',
				path: r.path,
				value,
				scope
			});
			const list = rows.data;
			if (list)
				rows.set({
					...list,
					settings: list.settings.map((x) => (x.path === r.path ? done.setting : x))
				});
			if (!r.live) toasts.push(`${r.label}: applies when azazel starts again.`, 'info');
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
			void rows.load(true);
		}
	}
</script>

{#if rows.state !== 'ready'}
	<Unavailable of={rows} what="settings" />
{:else}
	<div class="bg-background sticky top-0 z-10 -mx-1 space-y-2 px-1 pt-1 pb-2">
		<div class="flex flex-wrap items-center gap-2">
			<label class="relative min-w-40 flex-1">
				<Search
					class="text-muted-foreground pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2"
				/>
				<input
					class="field h-8 pl-8 text-[13px]"
					type="search"
					bind:value={query}
					placeholder="Search {all.length} settings"
					aria-label="Search settings"
				/>
			</label>
			{#if app.canControl}<ScopeSelect
					bind:value={scope}
					project={rows.data?.project ?? false}
				/>{/if}
		</div>
		<div class="no-scrollbar -mx-1 flex gap-1 overflow-x-auto px-1" role="group" aria-label="Pages">
			{#each ['', ...pages] as p (p)}
				<button
					type="button"
					class="page-chip"
					aria-pressed={page === p && !query.trim()}
					onclick={() => {
						page = p;
						query = '';
					}}>{p || 'All'}</button
				>
			{/each}
		</div>
	</div>

	{#each tree as p (p.page)}
		<section aria-label={p.page} data-settings-page={p.page}>
			{#if !page || query.trim()}
				<h3 class="mt-4 mb-1 text-[15px] font-semibold tracking-[-0.01em]">{p.page}</h3>
			{/if}
			{#each p.groups as g (g.group)}
				{#if g.group !== p.page || p.groups.length > 1}<h4 class="set-h">{g.group}</h4>{/if}
				{#each g.rows as r (r.path)}
					{@const id = idOf(r)}
					{@const wide = r.kind === 'list' || r.kind === 'role' || r.kind === 'table'}
					<div
						class="set-row {wide ? 'flex-col items-stretch! gap-y-2!' : 'stacks'}"
						data-setting={r.path}
						data-kind={r.kind}
					>
						<div class="min-w-0 flex-1">
							<p class="set-label" id="{id}-l">
								{r.label}
								{#if r.source}<span class="tag ml-1 h-[1.125rem] align-[1px] text-[11px]"
										>{SOURCE[r.source] ?? r.source}</span
									>{/if}
								{#if r.gated}<span
										class="tag ml-1 h-[1.125rem] align-[1px] text-[11px]"
										title="A project's config may set this only once the project is trusted."
										>needs trust</span
									>{/if}
							</p>
							<p class="set-hint">{r.description}</p>
							<p class="text-faint mt-0.5 font-mono text-[11px]">
								{r.path}{r.live ? '' : ' · applies on restart'}
							</p>
						</div>
						<div class="flex min-w-0 shrink-0 items-center gap-1.5 {wide ? '' : 'max-w-[55%]'}">
							{#if r.kind === 'switch'}
								<button
									type="button"
									class="switch"
									role="switch"
									aria-checked={r.value === true}
									aria-labelledby="{id}-l"
									disabled={locked(r)}
									onclick={() => set(r, r.value !== true)}
								></button>
							{:else if (r.kind === 'choice' || r.kind === 'theme') && r.choices?.length}
								<select
									class="select"
									aria-labelledby="{id}-l"
									value={String(r.value ?? '')}
									disabled={locked(r)}
									onchange={(e) => set(r, e.currentTarget.value)}
								>
									{#each r.choices as c (c)}<option value={c}>{c}</option>{/each}
								</select>
							{:else if r.kind === 'number'}
								<input
									class="field num h-8 w-24 text-right font-mono text-[13px]"
									type="number"
									step="any"
									aria-labelledby="{id}-l"
									value={Number(r.value ?? 0)}
									disabled={locked(r)}
									onchange={(e) => {
										const n = e.currentTarget.valueAsNumber;
										if (Number.isFinite(n)) void set(r, n);
										else e.currentTarget.value = String(r.value ?? 0);
									}}
								/>
							{:else if r.kind === 'role'}
								<button
									type="button"
									class="btn btn-secondary"
									onclick={() => (ui.settings = 'models')}>Edit in Models</button
								>
							{:else if r.kind === 'list'}
								<ListEditor
									items={list(r.value)}
									label={r.label}
									disabled={locked(r)}
									onchange={(v) => set(r, v)}
								/>
							{:else if r.kind === 'table'}
								<pre
									class="well text-ink-2 scroll-thin max-h-40 w-full overflow-auto p-2.5 text-xs">{JSON.stringify(
										r.value,
										null,
										2
									)}</pre>
							{:else}
								<input
									class="field h-8 w-48 max-w-full font-mono text-[13px]"
									aria-labelledby="{id}-l"
									value={String(r.value ?? '')}
									disabled={locked(r)}
									spellcheck="false"
									onchange={(e) => set(r, e.currentTarget.value)}
								/>
							{/if}
							{#if app.canControl && r.kind !== 'role' && r.source === scope}
								<button
									type="button"
									class="icon-btn"
									aria-label="Reset {r.label}: remove it from {SOURCE[scope]}"
									title="Remove it from {SOURCE[scope]}"
									onclick={() => set(r, null)}><RotateCcw class="size-3.5" /></button
								>
							{/if}
						</div>
					</div>
				{/each}
			{/each}
		</section>
	{:else}
		<p class="text-muted-foreground py-8 text-center text-[13px]">
			No setting matches “{query.trim()}”.
		</p>
	{/each}
{/if}

<style>
	.page-chip {
		flex-shrink: 0;
		height: 1.625rem;
		padding: 0 0.625rem;
		border-radius: 999px;
		font-size: 0.75rem;
		font-weight: 500;
		color: var(--muted-foreground);
		transition:
			background-color var(--dur-fast) var(--ease-out),
			color var(--dur-fast) var(--ease-out);
	}
	.page-chip[aria-pressed='true'] {
		background: var(--active);
		color: var(--foreground);
	}
	@media (hover: hover) {
		.page-chip:hover {
			background: var(--hover);
			color: var(--foreground);
		}
	}
	@media (pointer: coarse) {
		.page-chip {
			height: 2.25rem;
			padding: 0 0.875rem;
			font-size: 0.8125rem;
		}
	}
</style>
