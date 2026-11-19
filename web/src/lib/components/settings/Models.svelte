<script lang="ts">
	// Which models answer for each role, the profiles that set every role at once, and how hard the
	// open session thinks.
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import {
		THINKING_LEVELS,
		type ModelRow,
		type Roles,
		type SaveTo,
		type SettingList,
		type State
	} from '$lib/protocol';
	import { profileLogins, profileModels, profileSource } from '$lib/profiles';
	import * as NativeSelect from '$lib/components/ui/native-select';
	import * as Card from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import Check from '@lucide/svelte/icons/check';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import ListEditor from './ListEditor.svelte';
	import ScopeSelect from './ScopeSelect.svelte';
	import Section from './Section.svelte';
	import Row from './Row.svelte';

	const roles = new Loaded<Roles>(() => hostRpc<Roles>({ type: 'list_roles' }));
	const models = new Loaded<ModelRow[]>(() => hostRpc<ModelRow[]>({ type: 'list_models' }));
	// An open session shows its own profiles, live; with none open they are read from a running one.
	const host = new Loaded<State>(() => hostRpc<State>({ type: 'get_state' }));
	// `[profiles] on_limit` and `on_reset`, shown as they are set; All settings changes them.
	const settings = new Loaded<SettingList>(() => hostRpc<SettingList>({ type: 'get_settings' }));
	onMount(() => {
		void roles.load();
		void models.load();
		// Reading the settings is a control call; a view device would only be refused.
		if (app.canControl) void settings.load();
		if (!ui.session) void host.load();
	});

	let scope = $state<SaveTo>('session');
	let profileName = $state('');
	let switching = $state<string | null>(null);
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
	const profileState = $derived(st ?? host.data);
	const profiles = $derived(profileState?.profiles ?? []);
	const current = $derived(profileState?.profile ?? null);
	const setting = (path: string) => settings.data?.settings.find((r) => r.path === path);
	const onLimit = $derived(setting('profiles.on_limit'));
	const onReset = $derived(setting('profiles.on_reset'));

	/** A profile belongs to a session: the one that is open moves to it. */
	async function setProfile(name: string) {
		const s = ui.session;
		if (!s || switching) return;
		switching = name;
		try {
			await s.setProfile(name);
			void roles.load(true);
		} catch (e) {
			fail(e);
		} finally {
			switching = null;
		}
	}
	async function saveProfile() {
		const name = profileName.trim();
		if (!name) return;
		try {
			if (ui.session) await ui.session.saveProfile(name);
			else host.set(await hostRpc<State>({ type: 'save_profile', name }));
			profileName = '';
			toasts.push(`Saved the roles as the profile ${name}.`, 'success', 2500);
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

<Section title="Profiles" id="set-profiles">
	{#if !profileState}
		<Unavailable of={host} what="profiles" />
	{:else if !profiles.length}
		<p class="py-2 text-xs leading-relaxed text-muted-foreground">
			This computer reports no profiles: its azazel is older than 0.16.
		</p>
	{:else}
		<p class="mb-2 text-xs leading-relaxed text-muted-foreground">
			A profile sets the models of every role at once. {ui.session
				? 'Switching moves the open session; its next reply starts a new prompt cache.'
				: 'Open a session to switch its profile.'}
		</p>
		<Card.Root size="sm" data-profiles>
			<Card.Content>
				<ul aria-label="Profiles">
					{#each profiles as p (p.name)}
						<!-- The button sits beside the name, so the models below have the whole width. -->
						<li
							class="grid gap-1 border-b py-3 first:pt-0 last:border-b-0 last:pb-0"
							data-profile={p.name}
							aria-current={p.name === current ? 'true' : undefined}
						>
							<div class="flex min-h-8 min-w-0 items-center gap-3">
								<div class="flex min-w-0 flex-1 items-center gap-1.5">
									<span class="truncate font-medium">{p.name}</span>
									<Badge variant="outline" class="font-normal text-muted-foreground"
										>{profileSource(p)}</Badge
									>
									{#if p.name === current}
										<Badge variant="secondary"><Check />in use</Badge>
									{/if}
								</div>
								{#if app.canControl && p.name !== current}
									<Button
										variant="outline"
										class="shrink-0"
										disabled={!p.usable || !ui.session || switching !== null}
										aria-label="Switch to the profile {p.name}"
										onclick={() => setProfile(p.name)}>Switch</Button
									>
								{/if}
							</div>
							<dl
								class="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 font-mono text-xs text-muted-foreground"
							>
								{#each profileModels(p) as m (m.role)}
									<dt>@{m.role}</dt>
									<dd class="break-all">{m.model}</dd>
								{/each}
							</dl>
							{#if !p.usable}
								<p class="text-xs text-muted-foreground">
									needs <code class="font-mono text-foreground">{profileLogins(p).join(', ')}</code>
								</p>
							{/if}
						</li>
					{/each}
				</ul>
			</Card.Content>
		</Card.Root>
		{#if app.fullAccess}
			<Row
				label="Save current as…"
				for="set-profile-name"
				hint="Writes the roles as they stand to your config, as a profile of that name."
				orientation="responsive"
			>
				<form
					class="flex shrink-0 gap-2"
					onsubmit={(e) => {
						e.preventDefault();
						void saveProfile();
					}}
				>
					<Input
						id="set-profile-name"
						class="w-44"
						bind:value={profileName}
						placeholder="Profile name"
						autocomplete="off"
						spellcheck={false}
					/>
					<Button type="submit" variant="outline" disabled={!profileName.trim()}>Save</Button>
				</form>
			</Row>
		{:else if app.canControl}
			<p class="py-2 text-xs leading-relaxed text-muted-foreground">
				Saving the current roles as a profile needs full access.
			</p>
		{/if}
	{/if}
	{#if !app.canControl}
		<!-- A view device may not read the settings, so the rows are left out. -->
	{:else if settings.state !== 'ready'}
		<Unavailable of={settings} what="what a usage limit does" />
	{:else if onLimit && onReset}
		<Row label={onLimit.label} hint={onLimit.description} data-profile-setting="on_limit">
			{#if onLimit.value}
				<span class="shrink-0 font-mono text-xs">{onLimit.value}</span>
			{:else}
				<span class="shrink-0 text-xs text-muted-foreground">never switches</span>
			{/if}
		</Row>
		<Row label={onReset.label} hint={onReset.description} data-profile-setting="on_reset">
			<span class="shrink-0 font-mono text-xs">{onReset.value}</span>
		</Row>
		<p class="mt-2 text-xs leading-relaxed text-muted-foreground">
			These two are shown as they are set. Change them under All settings, or as
			<code class="font-mono">on_limit</code> and <code class="font-mono">on_reset</code> of
			<code class="font-mono">[profiles]</code> in the config file.
		</p>
	{/if}
</Section>

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
