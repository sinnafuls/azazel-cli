<script lang="ts">
	// The sidebar: the brand, New chat and Search, the sessions, and at the bottom what this device
	// may do and the way into Settings. As a rail it is the same actions as glyphs.
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { isMac, keysOf } from '$lib/shortcuts';
	import Brand from './Brand.svelte';
	import SessionList from './SessionList.svelte';
	import PanelLeft from '@lucide/svelte/icons/panel-left';
	import SquarePen from '@lucide/svelte/icons/square-pen';
	import Search from '@lucide/svelte/icons/search';
	import Settings from '@lucide/svelte/icons/settings';
	import ShieldAlert from '@lucide/svelte/icons/shield-alert';

	let { mode }: { mode: 'full' | 'rail' | 'drawer' } = $props();

	const mac = isMac(typeof navigator === 'undefined' ? '' : navigator.platform);
	const keys = (combo: string) => keysOf(combo, mac).join(mac ? '' : '+');
	const current = $derived(page.params.id ?? null);
	const waiting = $derived(sessions.list.filter((s) => s.state === 'waiting for you').length);
	const access = $derived(
		app.fullAccess ? 'Full access' : app.canControl ? 'Full control' : 'Watch only'
	);
	const close = () => (ui.drawer = false);
	const toggle = () => ui.toggleSidebar(!viewport.wide);
</script>

{#if mode === 'rail'}
	<div class="flex h-full flex-col items-center gap-1 py-2">
		<button
			type="button"
			class="icon-btn rail-btn"
			aria-label="Show the sidebar"
			title="Show the sidebar ({keys('mod+shift+s')})"
			aria-expanded="false"
			onclick={toggle}><PanelLeft class="size-4" /></button
		>
		{#if newChat.available}
			<button
				type="button"
				class="icon-btn rail-btn"
				aria-label="New chat"
				title="New chat ({keys('mod+shift+o')})"
				onclick={() => newChat.show()}><SquarePen class="size-4" /></button
			>
		{/if}
		<button
			type="button"
			class="icon-btn rail-btn relative"
			aria-label={waiting ? `Search sessions, ${waiting} waiting for you` : 'Search sessions'}
			title="Search sessions ({keys('mod+k')})"
			onclick={() => (ui.search = true)}
		>
			<Search class="size-4" />
			{#if waiting}<span
					class="bg-ember ring-rail absolute top-1 right-1 size-2 rounded-full ring-2"
					aria-hidden="true"
				></span>{/if}
		</button>
		<span class="flex-1"></span>
		{#if app.fullAccess}
			<span class="text-destructive grid size-8 place-items-center" title="Full access is on">
				<ShieldAlert class="size-4" /><span class="sr-only">Full access is on</span>
			</span>
		{/if}
		<button
			type="button"
			class="icon-btn rail-btn"
			aria-label="Settings"
			title="Settings ({keys('mod+.')})"
			onclick={() => ui.openSettings()}><Settings class="size-4" /></button
		>
	</div>
{:else}
	<div class="flex h-full min-w-0 flex-col">
		<div class="flex h-12 shrink-0 items-center gap-1 pr-2 pl-3">
			<a href="/" class="mr-auto rounded-md" aria-label="azazel, all sessions" onclick={close}
				><Brand size={22} /></a
			>
			<button
				type="button"
				class="icon-btn"
				aria-label={mode === 'drawer' ? 'Close the sidebar' : 'Hide the sidebar'}
				title="{mode === 'drawer' ? 'Close' : 'Hide'} the sidebar ({keys('mod+shift+s')})"
				aria-expanded="true"
				onclick={toggle}><PanelLeft class="size-4" /></button
			>
		</div>

		<div class="shrink-0 px-2 pb-2">
			{#if newChat.available}
				<button
					type="button"
					class="side-row"
					aria-label="New chat"
					onclick={() => {
						close();
						newChat.show();
					}}
				>
					<SquarePen class="size-4 shrink-0" />
					<span class="min-w-0 flex-1 truncate">New chat</span>
					<span class="text-faint hidden text-xs fine:inline" aria-hidden="true"
						>{keys('mod+shift+o')}</span
					>
				</button>
			{/if}
			<button
				type="button"
				class="side-row"
				aria-label="Search sessions"
				onclick={() => {
					close();
					ui.search = true;
				}}
			>
				<Search class="size-4 shrink-0" />
				<span class="min-w-0 flex-1 truncate">Search</span>
				<span class="text-faint hidden text-xs fine:inline" aria-hidden="true">{keys('mod+k')}</span
				>
			</button>
		</div>

		<div class="scroll-thin min-h-0 flex-1 overflow-y-auto px-2 pb-2">
			<SessionList {current} onnavigate={close} />
		</div>

		<div class="shrink-0 border-t p-2 pb-[max(0.5rem,env(safe-area-inset-bottom))]">
			<button
				type="button"
				class="side-row"
				aria-label="Settings"
				onclick={() => ui.openSettings(viewport.wide ? 'general' : '')}
			>
				<Settings class="size-4 shrink-0" />
				<span class="min-w-0 flex-1 truncate">Settings</span>
				<span
					class="flex shrink-0 items-center gap-1.5 text-xs {app.fullAccess
						? 'text-destructive font-medium'
						: 'text-muted-foreground'}"
				>
					{#if app.fullAccess}<ShieldAlert class="size-3.5" />{:else}<span
							class="size-1.5 rounded-full {app.canControl ? 'bg-success' : 'bg-faint'}"
							aria-hidden="true"
						></span>{/if}
					{access}
				</span>
			</button>
		</div>
	</div>
{/if}

<style>
	.rail-btn {
		width: 2rem;
		height: 2rem;
		border-radius: var(--radius-md);
	}
</style>
