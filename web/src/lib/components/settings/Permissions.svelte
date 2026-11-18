<script lang="ts">
	// The mode of the open session, and the allow, ask and deny rules (editable with full access).
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import { MODES } from '$lib/protocol';
	import type { SaveTo, SettingChanged, SettingList } from '$lib/protocol';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import ListEditor from './ListEditor.svelte';

	const ABOUT: Record<string, string> = {
		plan: 'Reads and writes a plan. Nothing else is changed.',
		manual: 'You approve each change and each command.',
		'accept-edits': 'Edits inside the project go through; commands still ask.',
		auto: 'No approvals. Every guard stays on.',
		bypass: 'No approvals and no guards, except deny rules and the push guard.'
	};
	const RULES: [string, string, string][] = [
		['permissions.allow', 'Allow', 'Never asks.'],
		['permissions.ask', 'Ask', 'Always asks, whatever the mode.'],
		['permissions.deny', 'Deny', 'Always refused.']
	];

	const settings = new Loaded<SettingList>(() => hostRpc<SettingList>({ type: 'get_settings' }));
	onMount(() => void settings.load());

	const mode = $derived(ui.session?.state?.mode ?? null);
	const modes = $derived(MODES.filter((m) => m !== 'bypass' || app.me?.allow_bypass));
	let confirmBypass = $state(false);
	let scope = $state<SaveTo>('home');
	const fail = (e: unknown) => toasts.push(e instanceof Error ? e.message : String(e), 'error');
	const rule = (path: string) => settings.data?.settings.find((r) => r.path === path);
	const list = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : []);

	async function setMode(m: string) {
		if (m === 'bypass' && !confirmBypass) {
			confirmBypass = true;
			return;
		}
		confirmBypass = false;
		await ui.session?.setMode(m).catch(fail);
	}
	async function setRule(path: string, value: string[]) {
		try {
			const done = await hostRpc<SettingChanged>({ type: 'set_setting', path, value, scope });
			const list = settings.data;
			if (list)
				settings.set({
					...list,
					settings: list.settings.map((r) => (r.path === path ? done.setting : r))
				});
		} catch (e) {
			fail(e);
			void settings.load(true);
		}
	}
</script>

<h3 class="set-h" id="perm-mode">Mode of this session</h3>
{#if !ui.session}
	<p class="text-muted-foreground py-2 text-[13px]">Open a session to see and change its mode.</p>
{:else}
	<div role="radiogroup" aria-labelledby="perm-mode" class="space-y-1">
		{#each modes as m (m)}
			<button
				type="button"
				role="radio"
				aria-checked={mode === m}
				class="choice flex w-full items-baseline gap-3 px-3 py-2 text-left"
				disabled={!app.canControl}
				onclick={() => setMode(m)}
			>
				<span class="w-24 shrink-0 font-mono text-[13px] {m === 'bypass' ? 'text-destructive' : ''}"
					>{m}</span
				>
				<span class="text-muted-foreground min-w-0 flex-1 text-xs leading-snug">{ABOUT[m]}</span>
			</button>
		{/each}
	</div>
	{#if confirmBypass}
		<div class="bg-destructive/10 mt-2 rounded-xl p-3" role="alert">
			<p class="text-[13px] leading-relaxed">
				Bypass lifts every guard except deny rules and the push guard. Agents will edit files and
				run commands without asking.
			</p>
			<div class="mt-2.5 flex gap-2">
				<button type="button" class="btn btn-secondary" onclick={() => (confirmBypass = false)}
					>Cancel</button
				>
				<button type="button" class="btn btn-destroy" onclick={() => setMode('bypass')}
					>Use bypass</button
				>
			</div>
		</div>
	{/if}
{/if}

<div class="mt-6 mb-1.5 flex items-center justify-between gap-3">
	<h3 class="text-muted-foreground text-xs font-medium">Rules</h3>
	{#if settings.state === 'ready' && app.fullAccess}
		<label class="text-muted-foreground flex items-center gap-2 text-xs">
			Save for
			<select class="select h-8" bind:value={scope} aria-label="Where rules are saved">
				<option value="home">Every session</option>
				<option value="project" disabled={!settings.data?.project}>This project</option>
				<option value="session">This session</option>
			</select>
		</label>
	{/if}
</div>
{#if settings.state !== 'ready'}
	<Unavailable of={settings} what="rules" />
{:else}
	{#each RULES as [path, label, hint] (path)}
		{@const r = rule(path)}
		<div class="set-row stacks items-start!" data-rules={label.toLowerCase()}>
			<div class="w-36 shrink-0 pt-0.5">
				<p class="set-label">{label}</p>
				<p class="set-hint">{hint}</p>
			</div>
			<div class="min-w-0 flex-1">
				<ListEditor
					items={list(r?.value)}
					label="{label} rules"
					placeholder="tool(pattern), then Enter"
					disabled={!app.fullAccess || !r}
					onchange={(v) => setRule(path, v)}
				/>
			</div>
		</div>
	{/each}
	{#if !app.fullAccess}
		<p class="set-hint mt-2">
			Rules can be read here. Changing them changes what agents may do on the computer, so it needs
			full access.
		</p>
	{/if}
{/if}
