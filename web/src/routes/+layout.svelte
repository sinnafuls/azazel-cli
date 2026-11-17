<script lang="ts">
	import '../app.css';
	import { onMount, type Snippet } from 'svelte';
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { viewport } from '$lib/media.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import Brand from '$lib/components/Brand.svelte';
	import SessionList from '$lib/components/SessionList.svelte';
	import PairDevice from '$lib/components/PairDevice.svelte';
	import NewChat from '$lib/components/NewChat.svelte';
	import Welcome from '$lib/components/Welcome.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import Plus from '@lucide/svelte/icons/plus';
	import ThemeToggle from '$lib/components/ThemeToggle.svelte';
	import X from '@lucide/svelte/icons/x';
	import LogOut from '@lucide/svelte/icons/log-out';
	import QrCode from '@lucide/svelte/icons/qr-code';

	const TOAST_DOT = {
		error: 'bg-destructive',
		warn: 'bg-ember',
		success: 'bg-success',
		info: 'bg-info'
	} as const;

	let { children }: { children: Snippet } = $props();
	let pairing = $state(false);

	onMount(() => {
		app.applyTheme();
		viewport.init();
		void app.init();
		const onHash = () => {
			if (location.hash.startsWith('#pair=')) void app.init();
		};
		addEventListener('hashchange', onHash);
		return () => removeEventListener('hashchange', onHash);
	});

	const current = $derived(page.params.id ?? null);
	const waiting = $derived(sessions.list.filter((s) => s.state === 'waiting for you').length);
</script>

<svelte:head><title>azazel</title></svelte:head>

{#if app.auth === 'ok'}
	<!-- Wide: the rail is the page and the work sits on it as a raised panel. -->
	<div class="grid h-dvh grid-cols-1 lg:grid-cols-[17.5rem_minmax(0,1fr)]">
		<aside class="hidden min-h-0 flex-col lg:flex" aria-label="Sessions">
			<div class="flex h-14 shrink-0 items-center px-4">
				<a href="/" class="rounded-md" aria-label="azazel, all sessions"><Brand /></a>
			</div>
			<div class="flex shrink-0 items-center justify-between px-4 pt-2 pb-1.5">
				<h2 class="label">Sessions</h2>
				<span class="flex items-center gap-1.5">
					{#if sessions.loaded && sessions.list.length}
						{#if waiting}<span class="tag tag-ember num">{waiting} waiting</span>{/if}
						<span class="text-faint meta">{sessions.list.length}</span>
					{/if}
					{#if newChat.available}
						<button
							type="button"
							class="btn btn-quiet btn-icon -mr-2"
							aria-label="New chat"
							title="New chat"
							onclick={() => newChat.show()}><Plus class="size-4" /></button
						>
					{/if}
				</span>
			</div>
			<div class="scroll-thin min-h-0 flex-1 overflow-y-auto px-2 pb-2">
				<SessionList {current} />
			</div>
			<div class="flex h-13 shrink-0 items-center gap-0.5 pr-2 pl-4">
				<span class="text-muted-foreground flex min-w-0 flex-1 items-center gap-2 truncate text-xs">
					<span
						class="size-1.5 shrink-0 rounded-full {app.me?.access === 'control'
							? 'bg-success'
							: 'bg-faint'}"
						aria-hidden="true"
					></span>
					{app.me?.access === 'control' ? 'Full control' : 'Watch only'}
				</span>
				<button
					type="button"
					class="btn btn-quiet btn-icon"
					aria-label="Open on another device"
					title="Open on another device"
					onclick={() => (pairing = true)}><QrCode class="size-4" /></button
				>
				<ThemeToggle />
				<button
					type="button"
					class="btn btn-quiet btn-icon"
					aria-label="Sign out"
					title="Sign out"
					onclick={() => app.logout()}><LogOut class="size-4" /></button
				>
			</div>
		</aside>
		<main class="panel flex min-h-0 min-w-0 flex-col">{@render children()}</main>
	</div>
	<PairDevice open={pairing} onclose={() => (pairing = false)} />
	{#if newChat.available}<NewChat />{/if}
{:else if app.auth === 'checking' || app.auth === 'pairing'}
	<div class="bg-background relative grid h-dvh place-items-center p-6">
		<div class="dotgrid" aria-hidden="true"></div>
		<div class="relative flex flex-col items-center gap-5" role="status">
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
	class="pointer-events-none fixed inset-x-0 top-0 z-50 flex flex-col items-center gap-2 p-3 pt-[max(0.75rem,env(safe-area-inset-top))] md:items-end md:p-4"
	aria-live="polite"
>
	{#each toasts.items as t (t.id)}
		<div
			class="float toast pointer-events-auto flex w-full max-w-sm items-start gap-3 rounded-xl py-1 pr-1 pl-3.5"
			role={t.level === 'error' ? 'alert' : 'status'}
		>
			<span class="relative mt-[1.125rem] grid size-2 shrink-0 place-items-center fine:mt-3">
				<span class="absolute -inset-1 rounded-full opacity-25 {TOAST_DOT[t.level]}"></span>
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
</style>
