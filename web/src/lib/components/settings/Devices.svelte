<script lang="ts">
	// This device, the others that are paired, a new link, and whether full access is on.
	import { onMount } from 'svelte';
	import { api, type DeviceRow } from '$lib/api';
	import { app } from '$lib/app.svelte';
	import { Refused } from '$lib/host';
	import { ui } from '$lib/ui.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { ago } from '$lib/format';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import * as Alert from '$lib/components/ui/alert';
	import * as AlertDialog from '$lib/components/ui/alert-dialog';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
	import Section from './Section.svelte';
	import Row from './Row.svelte';
	import QrCode from '@lucide/svelte/icons/qr-code';
	import ShieldAlert from '@lucide/svelte/icons/shield-alert';

	const devices = new Loaded<DeviceRow[]>(() =>
		// Only a control device may list the others: a view device does not ask.
		app.canControl ? api.devices() : Promise.reject(new Refused())
	);
	onMount(() => void devices.load());

	let confirmAll = $state(false);
	const access = $derived(app.canControl ? 'Full control' : 'Watch only');
	const until = $derived(
		app.me?.expires
			? new Date(app.me.expires).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' })
			: ''
	);

	async function signOutAll() {
		try {
			await api.signOutAll();
			ui.settings = null;
			await app.init();
		} catch (e) {
			confirmAll = false;
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		}
	}
</script>

<Section title="Full access">
	{#if app.fullAccess}
		<Alert.Root variant="destructive" data-full-access-status="on">
			<ShieldAlert />
			<Alert.Title>On: this browser can do anything on this computer</Alert.Title>
			<Alert.Description>
				It may change the configuration and the permission rules, save for every session and run
				every command. Anyone holding this device holds the computer. Turn it off in the terminal
				([remote] full_access).
			</Alert.Description>
		</Alert.Root>
	{:else}
		<Row
			label="Off"
			hint="This browser drives sessions but cannot change the configuration, edit permission rules or save for every session. It is turned on at the computer ([remote] full_access), never from here."
			data-full-access-status="off"
		/>
	{/if}
</Section>

<Section title="This device">
	<Row label={access} hint={until ? `Signed in until ${until}.` : undefined}>
		<Button variant="outline" class="shrink-0" onclick={() => app.logout()}>Sign out</Button>
	</Row>
	<Row
		label="Open on another device"
		hint="A link or a QR code that works once, for a few minutes."
	>
		<Button variant="outline" class="shrink-0" onclick={() => (ui.pairing = true)}
			><QrCode /> New link</Button
		>
	</Row>
</Section>

<Section title="Paired devices">
	{#if devices.state !== 'ready'}
		<Unavailable of={devices} what="the list of paired devices" />
	{:else}
		<ul aria-label="Paired devices">
			{#each devices.data ?? [] as d (d.id)}
				<li class="border-b last:border-b-0">
					<Row
						label="Device {d.id}"
						hint="{d.access === 'control' ? 'Full control' : 'Watch only'} · paired {ago(
							d.paired
						)} · signed in until {new Date(d.expires).toLocaleString([], {
							dateStyle: 'medium',
							timeStyle: 'short'
						})}"
						class="border-b-0"
					>
						{#snippet badges()}
							{#if d.current}<Badge variant="secondary">this one</Badge>{/if}
						{/snippet}
					</Row>
				</li>
			{/each}
		</ul>
		{#if app.canControl}
			<Button variant="outline" class="mt-3" onclick={() => (confirmAll = true)}
				>Sign everyone out…</Button
			>
		{/if}
	{/if}
</Section>

<AlertDialog.Root open={confirmAll} onOpenChange={(v) => !v && (confirmAll = false)}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>Sign everyone out?</AlertDialog.Title>
			<AlertDialog.Description>
				Every device is signed out, this one too. You will need a new link from the terminal to get
				back in.
			</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
			<Button variant="destructive" onclick={signOutAll}>Sign everyone out</Button>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
