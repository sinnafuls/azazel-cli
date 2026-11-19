<script lang="ts">
	// The frame of the app, as sin-api's admin shell: Sidebar.Provider, the sidebar (a rail of icons
	// when folded, a sheet on a phone) and the inset with the page; and what floats over it
	// (settings, search, pairing, toasts).
	import '@fontsource-variable/geist';
	import '@fontsource-variable/geist-mono';
	import '../app.css';
	import { onMount, type Snippet } from 'svelte';
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import { isMac, shortcutFor } from '$lib/shortcuts';
	import Brand from '$lib/components/Brand.svelte';
	import AppSidebar from '$lib/components/Sidebar.svelte';
	import * as Sidebar from '$lib/components/ui/sidebar';
	import { Spinner } from '$lib/components/ui/spinner';
	import SearchPalette from '$lib/components/SearchPalette.svelte';
	import SettingsWindow from '$lib/components/settings/SettingsWindow.svelte';
	import PairDevice from '$lib/components/PairDevice.svelte';
	import NewChat from '$lib/components/NewChat.svelte';
	import ResumePicker from '$lib/components/ResumePicker.svelte';
	import { resume } from '$lib/resume.svelte';
	import Welcome from '$lib/components/Welcome.svelte';
	import { Toaster } from '$lib/components/ui/sonner';
	import ShieldAlert from '@lucide/svelte/icons/shield-alert';

	let { children }: { children: Snippet } = $props();
	let mac = false;

	onMount(() => {
		mac = isMac(navigator.platform);
		app.applyTheme();
		ui.init();
		viewport.init();
		void app.init();
		const onHash = () => {
			if (location.hash.startsWith('#pair=')) void app.init();
		};
		addEventListener('hashchange', onHash);
		return () => removeEventListener('hashchange', onHash);
	});

	function onkeydown(e: KeyboardEvent) {
		if (app.auth !== 'ok') return;
		const sc = shortcutFor(e, mac);
		if (!sc) return;
		if (sc.id === 'stop') {
			// Escape belongs first to whatever is open: a menu, a dialog, the `/` list.
			if (
				e.defaultPrevented ||
				document.querySelector('[role=dialog], [role=alertdialog], [role=menu], dialog:modal')
			)
				return;
			const s = ui.session;
			if (!s?.busy || !app.canControl) return;
			e.preventDefault();
			s.abort().catch((err) => toasts.push(String(err?.message ?? err), 'error'));
			return;
		}
		e.preventDefault();
		if (sc.id === 'new-chat') {
			if (newChat.available) newChat.show();
		} else if (sc.id === 'search') {
			ui.settings = null;
			ui.search = !ui.search;
		} else if (sc.id === 'settings') {
			if (ui.settings !== null) ui.settings = null;
			else ui.openSettings(viewport.wide ? 'general' : '');
		} else if (sc.id === 'sidebar') ui.toggleSidebar();
		else if (sc.id === 'shortcuts') ui.openSettings('shortcuts');
	}
</script>

<svelte:head><title>azazel</title></svelte:head>
<svelte:window {onkeydown} />

{#if app.auth === 'ok'}
	<Sidebar.Provider
		open={!ui.collapsed}
		onOpenChange={(open) => ui.setCollapsed(!open)}
		class="h-dvh min-h-0"
	>
		<AppSidebar />
		<Sidebar.Inset class="h-dvh min-w-0 overflow-hidden">
			{#if app.fullAccess}
				<div
					class="flex shrink-0 items-center justify-center gap-2 border-b bg-destructive/10 px-4 py-1.5 pt-[max(0.375rem,env(safe-area-inset-top))] text-center text-xs font-medium"
					role="note"
					aria-label="Full access warning"
					data-full-access
				>
					<ShieldAlert class="size-4 shrink-0 text-destructive" />
					<span>Full access: this browser can do anything on this computer</span>
				</div>
			{/if}
			<div class="flex min-h-0 min-w-0 flex-1 flex-col">
				{#key page.params.id}{@render children()}{/key}
			</div>
		</Sidebar.Inset>
	</Sidebar.Provider>

	<SearchPalette />
	<SettingsWindow />
	<PairDevice open={ui.pairing} onclose={() => (ui.pairing = false)} />
	{#if newChat.available}<NewChat />{/if}
	{#if resume.available}<ResumePicker />{/if}
{:else if app.auth === 'checking' || app.auth === 'pairing'}
	<div class="grid h-dvh place-items-center bg-background p-6">
		<div class="flex flex-col items-center gap-4 text-sm text-muted-foreground" role="status">
			<Brand size={32} word={false} />
			<span class="flex items-center gap-2"
				><Spinner />{app.auth === 'pairing' ? 'Pairing this device…' : 'Connecting…'}</span
			>
		</div>
	</div>
{:else}
	<Welcome
		mode={app.auth === 'needs-link' ? 'needs-link' : 'error'}
		error={app.error}
		onretry={() => app.init()}
	/>
{/if}

<Toaster position={viewport.wide ? 'bottom-right' : 'top-center'} closeButton />
