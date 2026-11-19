<script lang="ts">
	// Which models answer for each role, the saved presets, and how hard the open session thinks.
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import { THINKING_LEVELS, type ModelRow, type Roles, type SaveTo } from '$lib/protocol';
	import * as NativeSelect from '$lib/components/ui/native-select';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import ListEditor from './ListEditor.svelte';
	import ScopeSelect from './ScopeSelect.svelte';
	import Section from './Section.svelte';
	import Row from './Row.svelte';

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
	<Section title="This session">
		<Row label="Model">
			{#snippet extra()}
				<p class="truncate font-mono text-xs text-muted-foreground">{st.model ?? 'none'}</p>
			{/snippet}
			<Button
				variant="outline"
				class="shrink-0"
				disabled={!app.canControl}
				onclick={() => {
					ui.settings = null;
					ui.picker = 'model';
				}}>Change</Button
			>
		</Row>
		<Row label="Thinking" for="set-session-thinking">
			<NativeSelect.Root
				id="set-session-thinking"
				value={st.thinking}
				disabled={!app.canControl}
				onchange={(e) => ui.session?.setThinking(e.currentTarget.value).catch(fail)}
			>
				{#each THINKING_LEVELS as l (l)}
					<NativeSelect.Option value={l}>{l}</NativeSelect.Option>
				{/each}
			</NativeSelect.Root>
		</Row>
	</Section>
{/if}

<Section title="Roles">
	{#snippet action()}
		{#if roles.state === 'ready' && app.canControl}
			<ScopeSelect bind:value={scope} project={roles.data?.project ?? false} />
		{/if}
	{/snippet}
	{#if roles.state !== 'ready'}
		<Unavailable of={roles} what="roles" />
	{:else if roles.data}
		<p class="mb-1 text-xs leading-relaxed text-muted-foreground">
			A role is a chain of models: the first that answers is used. Agents and tasks name a role (<span
				class="font-mono">@default</span
			>, <span class="font-mono">@smol</span>) instead of a model.
		</p>
		<ul aria-label="Role chains">
			{#each roles.data.roles as r (r.role)}
				<li class="border-b last:border-b-0" data-role={r.role}>
					<Row
						label="@{r.role}"
						mono
						hint="{SOURCE[r.source] ?? r.source}{r.thinking ? ` · thinks ${r.thinking}` : ''}"
						orientation="vertical"
						class="border-b-0"
					>
						<ListEditor
							items={r.chain}
							label="Models of @{r.role}"
							placeholder="provider/model, then Enter"
							suggestions={usable}
							disabled={!app.canControl}
							onchange={(chain) => setRole(r.role, chain.length ? chain : null)}
						/>
						{#if !r.chain.length && r.resolved.length}
							<p class="font-mono text-xs text-muted-foreground">uses {r.resolved[0]}</p>
						{/if}
					</Row>
				</li>
			{/each}
		</ul>
	{/if}
</Section>

{#if roles.state === 'ready' && roles.data}
	<Section title="Presets">
		{#if roles.data.presets.length}
			<ul class="flex flex-wrap gap-2" aria-label="Presets">
				{#each roles.data.presets as p (p)}
					<li>
						<Button variant="outline" disabled={!app.canControl} onclick={() => applyPreset(p)}
							>{p}</Button
						>
					</li>
				{/each}
			</ul>
		{:else}
			<p class="text-xs text-muted-foreground">No presets saved yet.</p>
		{/if}
		{#if app.fullAccess}
			<form
				class="mt-3 flex gap-2"
				onsubmit={(e) => {
					e.preventDefault();
					void savePreset();
				}}
			>
				<Input
					class="max-w-56"
					bind:value={preset}
					placeholder="Name of a new preset"
					aria-label="Name of a new preset"
				/>
				<Button type="submit" variant="outline" disabled={!preset.trim()}>Save current roles</Button
				>
			</form>
		{:else}
			<p class="mt-2 text-xs leading-relaxed text-muted-foreground">
				Saving a preset changes the configuration: it needs full access.
			</p>
		{/if}
	</Section>
{/if}
