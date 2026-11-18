<script lang="ts">
	// This device, the others that are paired, a new link, and whether full access is on.
	import { onMount } from 'svelte';
	import { api, type DeviceRow } from '$lib/api';
	import { app } from '$lib/app.svelte';
	import { Refused } from '$lib/host';
	import { ui } from '$lib/ui.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { ago } from '$lib/format';
	import { Loaded } from './load.svelte';
	import Unavailable from './Unavailable.svelte';
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

<h3 class="set-h">Full access</h3>
{#if app.fullAccess}
	<div class="alarm flex items-start gap-2.5 rounded-xl p-3" data-full-access-status="on">
		<ShieldAlert class="mt-0.5 size-4 shrink-0" />
		<div class="text-[13px] leading-relaxed">
			<p class="font-semibold">On: this browser can do anything on this computer</p>
			<p>
				It may change the configuration and the permission rules, save for every session and run
				every command. Anyone holding this device holds the computer. Turn it off in the terminal (<span
					class="font-mono">[remote] full_access</span
				>).
			</p>
		</div>
	</div>
{:else}
	<div class="set-row" data-full-access-status="off">
		<div>
			<p class="set-label">Off</p>
			<p class="set-hint">
				This browser drives sessions but cannot change the configuration, edit permission rules or
				save for every session. It is turned on at the computer (<span class="font-mono"
					>[remote] full_access</span
				>), never from here.
			</p>
		</div>
	</div>
{/if}

<h3 class="set-h">This device</h3>
<div class="set-row">
	<div>
		<p class="set-label">{access}</p>
		{#if until}<p class="set-hint">Signed in until {until}.</p>{/if}
	</div>
	<button type="button" class="btn btn-secondary shrink-0" onclick={() => app.logout()}
		>Sign out</button
	>
</div>
<div class="set-row">
	<div>
		<p class="set-label">Open on another device</p>
		<p class="set-hint">A link or a QR code that works once, for a few minutes.</p>
	</div>
	<button type="button" class="btn btn-secondary shrink-0" onclick={() => (ui.pairing = true)}
		><QrCode class="size-4" /> New link</button
	>
</div>

<h3 class="set-h">Paired devices</h3>
{#if devices.state !== 'ready'}
	<Unavailable of={devices} what="the list of paired devices" />
{:else}
	<ul aria-label="Paired devices">
		{#each devices.data ?? [] as d (d.id)}
			<li class="set-row">
				<div class="min-w-0">
					<p class="set-label truncate">
						Device {d.id}{#if d.current}<span class="tag ml-1.5 h-[1.125rem] text-[11px]"
								>this one</span
							>{/if}
					</p>
					<p class="set-hint">
						{d.access === 'control' ? 'Full control' : 'Watch only'} · paired {ago(d.paired)} · signed
						in until {new Date(d.expires).toLocaleString([], {
							dateStyle: 'medium',
							timeStyle: 'short'
						})}
					</p>
				</div>
			</li>
		{/each}
	</ul>
	{#if app.canControl}
		{#if confirmAll}
			<div class="bg-destructive/10 mt-2 rounded-xl p-3" role="alert">
				<p class="text-[13px] leading-relaxed">
					Every device is signed out, this one too. You will need a new link from the terminal to
					get back in.
				</p>
				<div class="mt-2.5 flex gap-2">
					<button type="button" class="btn btn-secondary" onclick={() => (confirmAll = false)}
						>Cancel</button
					>
					<button type="button" class="btn btn-destroy" onclick={signOutAll}
						>Sign everyone out</button
					>
				</div>
			</div>
		{:else}
			<button type="button" class="btn btn-secondary mt-3" onclick={() => (confirmAll = true)}
				>Sign everyone out…</button
			>
		{/if}
	{/if}
{/if}
