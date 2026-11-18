<script lang="ts">
	// Which models answer for each role, the saved presets, and how hard the open session thinks.
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import { THINKING_LEVELS, type ModelRow, type Roles, type SaveTo } from '$lib/protocol';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import ListEditor from './ListEditor.svelte';
	import ScopeSelect from './ScopeSelect.svelte';

	const roles = new Loaded<Roles>(() => hostRpc<Roles>({ type: 'list_roles' }));
	const models = new Loaded<ModelRow[]>(() => hostRpc<ModelRow[]>({ type: 'list_models' }));
	onMount(() => {
		void roles.load();
		void models.load();
	});

	let scope = $state<SaveTo>('session');
	let preset = $state('');
	const usable = $derived((models.data ?? []).filter((m) => m.usable).map((m) => m.model));
	const st = $derived(ui.session?.state ?? null);
	const fail = (e: unknown) => toasts.push(e instanceof Error ? e.message : String(e), 'error');

	async function setRole(role: string, chain: string[] | null) {
		try {
			roles.set(await hostRpc<Roles>({ type: 'set_role', role, chain, save: scope }));
			void ui.session?.refreshState().catch(() => {});
		} catch (e) {
			fail(e);
		}
	}
	async function applyPreset(name: string) {
		try {
			roles.set(await hostRpc<Roles>({ type: 'apply_preset', name }));
			void ui.session?.refreshState().catch(() => {});
			toasts.push(`Applied the preset ${name}.`, 'success', 2500);
		} catch (e) {
			fail(e);
		}
	}
	async function savePreset() {
		const name = preset.trim();
		if (!name) return;
		try {
			roles.set(await hostRpc<Roles>({ type: 'save_preset', name }));
			preset = '';
			toasts.push(`Saved the roles as the preset ${name}.`, 'success', 2500);
		} catch (e) {
			fail(e);
		}
	}
	const SOURCE: Record<string, string> = {
		builtin: 'default',
		home: 'every session',
		project: 'this project',
		session: 'this session',
		unset: 'not set'
	};
</script>

{#if st && ui.session}
	<h3 class="set-h">This session</h3>
	<div class="set-row">
		<div class="min-w-0">
			<p class="set-label">Model</p>
			<p class="set-hint truncate font-mono">{st.model ?? 'none'}</p>
		</div>
		<button
			type="button"
			class="btn btn-secondary shrink-0"
			disabled={!app.canControl}
			onclick={() => {
				ui.settings = null;
				ui.picker = 'model';
			}}>Change</button
		>
	</div>
	<div class="set-row">
		<label class="set-label" for="set-thinking">Thinking</label>
		<select
			id="set-thinking"
			class="select"
			value={st.thinking}
			disabled={!app.canControl}
			onchange={(e) => ui.session?.setThinking(e.currentTarget.value).catch(fail)}
		>
			{#each THINKING_LEVELS as l (l)}<option value={l}>{l}</option>{/each}
		</select>
	</div>
{/if}

<div class="mt-6 mb-1.5 flex items-center justify-between gap-3">
	<h3 class="text-muted-foreground text-xs font-medium">Roles</h3>
	{#if roles.state === 'ready' && app.canControl}
		<ScopeSelect bind:value={scope} project={roles.data?.project ?? false} />
	{/if}
</div>
{#if roles.state !== 'ready'}
	<Unavailable of={roles} what="roles" />
{:else if roles.data}
	<p class="set-hint mb-2">
		A role is a chain of models: the first that answers is used. Agents and tasks name a role (<span
			class="font-mono">@default</span
		>, <span class="font-mono">@smol</span>) instead of a model.
	</p>
	<ul aria-label="Role chains">
		{#each roles.data.roles as r (r.role)}
			<li class="set-row stacks items-start!" data-role={r.role}>
				<div class="w-36 shrink-0 pt-0.5">
					<p class="set-label font-mono">@{r.role}</p>
					<p class="set-hint">
						{SOURCE[r.source] ?? r.source}{r.thinking ? ` · thinks ${r.thinking}` : ''}
					</p>
				</div>
				<div class="min-w-0 flex-1">
					<ListEditor
						items={r.chain}
						label="Models of @{r.role}"
						placeholder="provider/model, then Enter"
						suggestions={usable}
						disabled={!app.canControl}
						onchange={(chain) => setRole(r.role, chain.length ? chain : null)}
					/>
					{#if !r.chain.length && r.resolved.length}
						<p class="set-hint font-mono">uses {r.resolved[0]}</p>
					{/if}
				</div>
			</li>
		{/each}
	</ul>

	<h3 class="set-h">Presets</h3>
	{#if roles.data.presets.length}
		<ul class="flex flex-wrap gap-2" aria-label="Presets">
			{#each roles.data.presets as p (p)}
				<li>
					<button
						type="button"
						class="btn btn-secondary"
						disabled={!app.canControl}
						onclick={() => applyPreset(p)}>{p}</button
					>
				</li>
			{/each}
		</ul>
	{:else}
		<p class="set-hint">No presets saved yet.</p>
	{/if}
	{#if app.fullAccess}
		<form
			class="mt-3 flex gap-2"
			onsubmit={(e) => {
				e.preventDefault();
				void savePreset();
			}}
		>
			<input
				class="field h-9 max-w-56 text-[13px]"
				bind:value={preset}
				placeholder="Name of a new preset"
				aria-label="Name of a new preset"
			/>
			<button type="submit" class="btn btn-secondary" disabled={!preset.trim()}
				>Save current roles</button
			>
		</form>
	{:else}
		<p class="set-hint mt-2">Saving a preset changes the configuration: it needs full access.</p>
	{/if}
{/if}
