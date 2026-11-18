<script lang="ts">
	// The frame of the app: the full-access strip, the sidebar (resizable, a rail when collapsed, a
	// drawer on a phone), the page, and what floats over it (settings, search, pairing, toasts).
	import '../app.css';
	import { onMount, type Snippet } from 'svelte';
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import { isMac, shortcutFor } from '$lib/shortcuts';
	import { SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, SIDEBAR_RAIL } from '$lib/sidebar';
	import Brand from '$lib/components/Brand.svelte';
	import Sidebar from '$lib/components/Sidebar.svelte';
	import SearchPalette from '$lib/components/SearchPalette.svelte';
	import SettingsWindow from '$lib/components/settings/SettingsWindow.svelte';
	import PairDevice from '$lib/components/PairDevice.svelte';
	import NewChat from '$lib/components/NewChat.svelte';
	import Welcome from '$lib/components/Welcome.svelte';
	import X from '@lucide/svelte/icons/x';
	import ShieldAlert from '@lucide/svelte/icons/shield-alert';

	const TOAST_DOT = {
		error: 'bg-destructive',
		warn: 'bg-ember',
		success: 'bg-success',
		info: 'bg-info'
	} as const;

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
			if (e.defaultPrevented || document.querySelector('dialog:modal, [role=menu]')) return;
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
		} else if (sc.id === 'sidebar') ui.toggleSidebar(!viewport.wide);
		else if (sc.id === 'shortcuts') ui.openSettings('shortcuts');
	}

	// Dragging the sidebar's edge. The width follows the pointer and is remembered on release.
	let dragging = $state(false);
	function dragStart(e: PointerEvent) {
		dragging = true;
		(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
	}
	function dragMove(e: PointerEvent) {
		if (dragging) ui.setWidth(e.clientX, false);
	}
	function dragEnd() {
		if (!dragging) return;
		dragging = false;
		ui.setWidth(ui.width);
	}
	function dragKey(e: KeyboardEvent) {
		const step = e.shiftKey ? 48 : 16;
		if (e.key === 'ArrowLeft') ui.setWidth(ui.width - step);
		else if (e.key === 'ArrowRight') ui.setWidth(ui.width + step);
		else if (e.key === 'Home') ui.setWidth(SIDEBAR_MIN);
		else if (e.key === 'End') ui.setWidth(SIDEBAR_MAX);
		else return;
		e.preventDefault();
	}

	// The drawer is a modal dialog: the focus stays in it and Escape closes it.
	let drawer = $state<HTMLDialogElement>();
	$effect(() => {
		const d = drawer;
		if (!d) return;
		const want = ui.drawer && !viewport.wide;
		if (want && !d.open) d.showModal();
		else if (!want && d.open) d.close();
	});
	$effect(() => {
		if (viewport.wide) ui.drawer = false;
	});
</script>

<svelte:head><title>azazel</title></svelte:head>
<svelte:window {onkeydown} />

{#if app.auth === 'ok'}
	<div class="flex h-dvh flex-col">
		{#if app.fullAccess}
			<div
				class="alarm flex shrink-0 items-center justify-center gap-2 px-3 py-1.5 pt-[max(0.375rem,env(safe-area-inset-top))] text-center text-[13px] leading-snug font-semibold"
				role="note"
				aria-label="Full access warning"
				data-full-access
			>
				<ShieldAlert class="size-4 shrink-0" />
				<span>Full access: this browser can do anything on this computer</span>
			</div>
		{/if}
		<div class="flex min-h-0 flex-1">
			{#if viewport.wide}
				<aside
					class="side bg-rail relative shrink-0 border-r"
					class:dragging
					style="width: {ui.collapsed ? SIDEBAR_RAIL : ui.width}px"
					aria-label="Sidebar"
					data-sidebar={ui.collapsed ? 'rail' : 'full'}
				>
					<div class="h-full overflow-hidden">
						<Sidebar mode={ui.collapsed ? 'rail' : 'full'} />
					</div>
					{#if !ui.collapsed}
						<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
						<div
							class="resizer"
							role="separator"
							aria-orientation="vertical"
							aria-label="Sidebar width"
							aria-valuemin={SIDEBAR_MIN}
							aria-valuemax={SIDEBAR_MAX}
							aria-valuenow={ui.width}
							tabindex="0"
							onpointerdown={dragStart}
							onpointermove={dragMove}
							onpointerup={dragEnd}
							onpointercancel={dragEnd}
							ondblclick={() => ui.setWidth(SIDEBAR_DEFAULT)}
							onkeydown={dragKey}
						></div>
					{/if}
				</aside>
			{/if}
			<main class="bg-background flex min-h-0 min-w-0 flex-1 flex-col">
				{#key page.params.id}{@render children()}{/key}
			</main>
		</div>
	</div>

	<dialog
		bind:this={drawer}
		class="drawer bg-rail text-foreground"
		aria-label="Sidebar"
		onclose={() => (ui.drawer = false)}
		onclick={(e) => {
			if (e.target === drawer) ui.drawer = false;
		}}
	>
		{#if ui.drawer && !viewport.wide}<Sidebar mode="drawer" />{/if}
	</dialog>

	<SearchPalette />
	<SettingsWindow />
	<PairDevice open={ui.pairing} onclose={() => (ui.pairing = false)} />
	{#if newChat.available}<NewChat />{/if}
{:else if app.auth === 'checking' || app.auth === 'pairing'}
	<div class="bg-background grid h-dvh place-items-center p-6">
		<div class="flex flex-col items-center gap-5" role="status">
			<Brand size={40} word={false} />
			<p class="shimmer text-sm">
				{app.auth === 'pairing' ? 'Pairing this device…' : 'Connecting…'}
			</p>
		</div>
	</div>
{:else}
	<Welcome
		mode={app.auth === 'needs-link' ? 'needs-link' : 'error'}
		error={app.error}
		onretry={() => app.init()}
	/>
{/if}

<div
	class="pointer-events-none fixed inset-x-0 top-0 z-[60] flex flex-col items-center gap-2 p-3 pt-[max(0.75rem,env(safe-area-inset-top))] md:items-end md:p-4"
	aria-live="polite"
>
	{#each toasts.items as t (t.id)}
		<div
			class="float toast pointer-events-auto flex w-full max-w-sm items-start gap-3 rounded-2xl py-1 pr-1 pl-3.5"
			role={t.level === 'error' ? 'alert' : 'status'}
		>
			<span class="relative mt-[1.125rem] grid size-2 shrink-0 place-items-center fine:mt-3">
				<span class="relative size-2 rounded-full {TOAST_DOT[t.level]}" aria-hidden="true"></span>
			</span>
			<span class="min-w-0 flex-1 py-3 text-sm leading-snug break-words fine:py-1.5">{t.text}</span>
			<button
				type="button"
				class="btn btn-quiet btn-icon"
				aria-label="Dismiss"
				onclick={() => toasts.dismiss(t.id)}><X class="size-4" /></button
			>
		</div>
	{/each}
</div>

<style>
	.toast {
		animation: drop-in var(--dur-slow) var(--ease-out) both;
	}
	.side {
		transition: width var(--dur-slow) var(--ease-pane);
	}
	.side.dragging {
		transition: none;
		user-select: none;
	}
	/* The edge that is dragged: 9px to grab, a 2px line when pointed at, focused or dragged. */
	.resizer {
		position: absolute;
		top: 0;
		right: -5px;
		bottom: 0;
		z-index: 5;
		width: 9px;
		cursor: col-resize;
		touch-action: none;
		outline: none;
	}
	.resizer::after {
		content: '';
		position: absolute;
		inset: 0 3px 0 4px;
		background: transparent;
		transition: background-color var(--dur) var(--ease-out);
	}
	.resizer:hover::after,
	.dragging .resizer::after {
		background: var(--input);
	}
	.resizer:focus-visible::after {
		background: var(--ring);
	}
	.drawer {
		position: fixed;
		inset: 0 auto 0 0;
		width: min(20rem, 86vw);
		height: 100dvh;
		max-height: none;
		margin: 0;
		padding: 0;
		border: 0;
		border-right: 1px solid var(--border-strong);
		box-shadow: var(--elev-3);
	}
	.drawer[open] {
		animation: slide-in-left var(--dur-slow) var(--ease-pane) both;
	}
	.drawer::backdrop {
		background: var(--scrim);
		animation: fade-in var(--dur) var(--ease-out) both;
	}
</style>
