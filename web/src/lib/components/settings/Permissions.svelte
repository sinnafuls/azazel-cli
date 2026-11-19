<script lang="ts">
	// The mode of the open session, and the allow, ask and deny rules (editable with full access).
	import { onMount } from 'svelte';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { hostRpc } from '$lib/host';
	import { toasts } from '$lib/toasts.svelte';
	import { MODES } from '$lib/protocol';
	import type { SaveTo, SettingChanged, SettingList } from '$lib/protocol';
	import * as NativeSelect from '$lib/components/ui/native-select';
	import * as RadioGroup from '$lib/components/ui/radio-group';
	import * as AlertDialog from '$lib/components/ui/alert-dialog';
	import * as Field from '$lib/components/ui/field';
	import { Button } from '$lib/components/ui/button';
	import { Label } from '$lib/components/ui/label';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import ListEditor from './ListEditor.svelte';
	import Section from './Section.svelte';
	import Row from './Row.svelte';

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

	async function setMode(m: string, confirmed = false) {
		if (m === 'bypass' && !confirmed) {
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

<Section title="Mode of this session" id="perm-mode">
	{#if !ui.session}
		<p class="py-2 text-xs text-muted-foreground">Open a session to see and change its mode.</p>
	{:else}
		<!-- Bound through functions: the group shows the session's mode, never a choice that was not
		     confirmed. -->
		<RadioGroup.Root
			aria-labelledby="perm-mode"
			disabled={!app.canControl}
			bind:value={() => mode ?? '', (m) => void setMode(m)}
		>
			{#each modes as m (m)}
				<Field.Label for="perm-mode-{m}">
					<Field.Field orientation="horizontal">
						<RadioGroup.Item value={m} id="perm-mode-{m}" />
						<Field.Content>
							<Field.Title class="font-mono text-xs {m === 'bypass' ? 'text-destructive' : ''}"
								>{m}</Field.Title
							>
							<Field.Description class="text-xs">{ABOUT[m]}</Field.Description>
						</Field.Content>
					</Field.Field>
				</Field.Label>
			{/each}
		</RadioGroup.Root>
	{/if}
</Section>

<Section title="Rules">
	{#snippet action()}
		{#if settings.state === 'ready' && app.fullAccess}
			<div class="flex shrink-0 items-center gap-2">
				<Label for="perm-scope" class="text-xs font-normal text-muted-foreground">Save for</Label>
				<NativeSelect.Root id="perm-scope" bind:value={scope} aria-label="Where rules are saved">
					<NativeSelect.Option value="home">Every session</NativeSelect.Option>
					<NativeSelect.Option value="project" disabled={!settings.data?.project}
						>This project</NativeSelect.Option
					>
					<NativeSelect.Option value="session">This session</NativeSelect.Option>
				</NativeSelect.Root>
			</div>
		{/if}
	{/snippet}
	{#if settings.state !== 'ready'}
		<Unavailable of={settings} what="rules" />
	{:else}
		{#each RULES as [path, label, hint] (path)}
			{@const r = rule(path)}
			<Row {label} {hint} orientation="vertical" data-rules={label.toLowerCase()}>
				<ListEditor
					items={list(r?.value)}
					label="{label} rules"
					placeholder="tool(pattern), then Enter"
					disabled={!app.fullAccess || !r}
					onchange={(v) => setRule(path, v)}
				/>
			</Row>
		{/each}
		{#if !app.fullAccess}
			<p class="mt-2 text-xs leading-relaxed text-muted-foreground">
				Rules can be read here. Changing them changes what agents may do on the computer, so it
				needs full access.
			</p>
		{/if}
	{/if}
</Section>

<AlertDialog.Root open={confirmBypass} onOpenChange={(v) => !v && (confirmBypass = false)}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>Use bypass?</AlertDialog.Title>
			<AlertDialog.Description>
				Bypass lifts every guard except deny rules and the push guard. Agents will edit files and
				run commands without asking.
			</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
			<Button variant="destructive" onclick={() => setMode('bypass', true)}>Use bypass</Button>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
