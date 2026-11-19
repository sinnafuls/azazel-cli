<script lang="ts">
	// Every setting the computer reports (`get_settings`), by page and group, with search. The
	// control follows the setting's kind; a change is saved to the chosen scope.
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import { ui } from '$lib/ui.svelte';
	import type { SaveTo, SettingChanged, SettingList, SettingRow } from '$lib/protocol';
	import { Switch } from '$lib/components/ui/switch';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import * as NativeSelect from '$lib/components/ui/native-select';
	import * as InputGroup from '$lib/components/ui/input-group';
	import * as CodeBlock from '$lib/components/ui/code-block';
	import * as Field from '$lib/components/ui/field';
	import Tip from '$lib/components/Tip.svelte';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import ListEditor from './ListEditor.svelte';
	import ScopeSelect from './ScopeSelect.svelte';
	import Row from './Row.svelte';
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
	<!-- Stays in view while the list scrolls under it; the pane's own top padding is taken back. -->
	<div class="sticky -top-4 z-10 -mx-1 -mt-4 grid gap-2 bg-popover px-1 pt-4 pb-2">
		<div class="flex flex-wrap items-center gap-2">
			<InputGroup.Root class="min-w-40 flex-1">
				<InputGroup.Addon><Search /></InputGroup.Addon>
				<InputGroup.Input
					type="search"
					bind:value={query}
					placeholder="Search {all.length} settings…"
					aria-label="Search settings"
				/>
			</InputGroup.Root>
			{#if app.canControl}
				<ScopeSelect bind:value={scope} project={rows.data?.project ?? false} />
			{/if}
		</div>
		<!-- Wrapped, not scrolled sideways: every page is in view without a gesture. -->
		<div class="flex flex-wrap gap-1" role="group" aria-label="Pages">
			{#each ['', ...pages] as p (p)}
				{@const on = page === p && !query.trim()}
				<Button
					variant={on ? 'secondary' : 'ghost'}
					size="sm"
					class="shrink-0 font-normal {on ? '' : 'text-muted-foreground'}"
					aria-pressed={on}
					onclick={() => {
						page = p;
						query = '';
					}}>{p || 'All'}</Button
				>
			{/each}
		</div>
	</div>

	{#each tree as p (p.page)}
		<section aria-label={p.page} data-settings-page={p.page}>
			{#if !page || query.trim()}
				<h3 class="mt-4 mb-1 text-base font-semibold tracking-tight">{p.page}</h3>
			{/if}
			{#each p.groups as g (g.group)}
				{#if g.group !== p.page || p.groups.length > 1}
					<h4 class="mt-4 flex min-h-8 items-center text-xs font-medium text-muted-foreground">
						{g.group}
					</h4>
				{/if}
				<Field.Group class="gap-0">
					{#each g.rows as r (r.path)}
						{@const id = idOf(r)}
						{@const wide = r.kind === 'list' || r.kind === 'role' || r.kind === 'table'}
						<Row
							label={r.label}
							hint={r.description}
							id="{id}-l"
							orientation={wide ? 'vertical' : 'responsive'}
							data-setting={r.path}
							data-kind={r.kind}
						>
							{#snippet badges()}
								{#if r.source}<Badge variant="secondary">{SOURCE[r.source] ?? r.source}</Badge>{/if}
								{#if r.gated}
									<Badge
										variant="outline"
										title="A project's config may set this only once the project is trusted."
										>needs trust</Badge
									>
								{/if}
							{/snippet}
							{#snippet extra()}
								<p class="font-mono text-xs break-all text-muted-foreground">
									{r.path}{r.live ? '' : ' · applies on restart'}
								</p>
							{/snippet}
							<div class="flex min-w-0 items-center gap-1.5 {wide ? 'w-full' : 'shrink-0'}">
								{#if r.kind === 'switch'}
									<Switch
										aria-labelledby="{id}-l"
										checked={r.value === true}
										disabled={locked(r)}
										onCheckedChange={(v) => set(r, v)}
									/>
								{:else if (r.kind === 'choice' || r.kind === 'theme') && r.choices?.length}
									<NativeSelect.Root
										aria-labelledby="{id}-l"
										value={String(r.value ?? '')}
										disabled={locked(r)}
										onchange={(e) => set(r, e.currentTarget.value)}
									>
										{#each r.choices as c (c)}
											<NativeSelect.Option value={c}>{c}</NativeSelect.Option>
										{/each}
									</NativeSelect.Root>
								{:else if r.kind === 'number'}
									<Input
										class="num w-24 text-right"
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
									<Button variant="outline" onclick={() => (ui.settings = 'models')}
										>Edit in Models</Button
									>
								{:else if r.kind === 'list'}
									<div class="min-w-0 flex-1">
										<ListEditor
											items={list(r.value)}
											label={r.label}
											disabled={locked(r)}
											onchange={(v) => set(r, v)}
										/>
									</div>
								{:else if r.kind === 'table'}
									<CodeBlock.Root size="sm" class="min-w-0 flex-1">
										<CodeBlock.Content
											code={JSON.stringify(r.value, null, 2)}
											class="scroll-thin max-h-40"
										/>
									</CodeBlock.Root>
								{:else}
									<Input
										class="w-48 max-w-full font-mono text-xs md:text-xs"
										aria-labelledby="{id}-l"
										value={String(r.value ?? '')}
										disabled={locked(r)}
										spellcheck="false"
										onchange={(e) => set(r, e.currentTarget.value)}
									/>
								{/if}
								{#if app.canControl && r.kind !== 'role' && r.source === scope}
									<Tip
										text="Remove it from {SOURCE[scope]}"
										class="shrink-0"
										aria-label="Reset {r.label}: remove it from {SOURCE[scope]}"
										onclick={() => set(r, null)}><RotateCcw /></Tip
									>
								{/if}
							</div>
						</Row>
					{/each}
				</Field.Group>
			{/each}
		</section>
	{:else}
		<p class="py-8 text-center text-sm text-muted-foreground">
			No setting matches “{query.trim()}”.
		</p>
	{/each}
{/if}
