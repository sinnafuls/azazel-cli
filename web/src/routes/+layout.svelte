<script lang="ts">
	import '../app.css';
	import { onMount, type Snippet } from 'svelte';
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { viewport } from '$lib/media.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import Brand from '$lib/components/Brand.svelte';
	import SessionList from '$lib/components/SessionList.svelte';
	import { Button } from '$lib/components/ui/button';
	import X from '@lucide/svelte/icons/x';

	let { children }: { children: Snippet } = $props();

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
</script>

<svelte:head><title>azazel</title></svelte:head>

{#if app.auth === 'ok'}
	<div class="grid h-dvh grid-cols-1 lg:grid-cols-[18.5rem_minmax(0,1fr)]">
		<aside class="bg-card hidden min-h-0 flex-col border-r lg:flex" aria-label="Sessions">
			<div class="flex h-12 shrink-0 items-center justify-between border-b px-4"><Brand /></div>
			<div class="scroll-thin min-h-0 flex-1 overflow-y-auto p-2"><SessionList {current} /></div>
			<div class="flex items-center justify-between border-t px-3 py-2 text-xs">
				<span class="text-muted-foreground"
					>{app.me?.access === 'control' ? 'Full control' : 'Watch only'}</span
				>
				<span class="flex gap-1">
					<button
						class="hover:bg-raised h-8 rounded-md px-2"
						onclick={() =>
							app.setTheme(
								app.theme === 'dark' ? 'light' : app.theme === 'light' ? 'system' : 'dark'
							)}>Theme: {app.theme}</button
					>
					<button class="hover:bg-raised h-8 rounded-md px-2" onclick={() => app.logout()}
						>Sign out</button
					>
				</span>
			</div>
		</aside>
		<main class="flex min-h-0 min-w-0 flex-col">{@render children()}</main>
	</div>
{:else if app.auth === 'checking' || app.auth === 'pairing'}
	<div class="grid h-dvh place-items-center">
		<div class="flex flex-col items-center gap-4" role="status">
			<Brand />
			<p class="text-muted-foreground text-sm">
				{app.auth === 'pairing' ? 'Pairing this device…' : 'Connecting…'}
			</p>
		</div>
	</div>
{:else}
	<div class="grid min-h-dvh place-items-center p-6">
		<div class="w-full max-w-sm space-y-5">
			<Brand />
			{#if app.auth === 'needs-link'}
				<div class="space-y-2">
					<h1 class="text-lg font-semibold">
						{app.error ? 'That link did not work' : 'This device is not paired'}
					</h1>
					<p class="text-muted-foreground text-sm leading-relaxed">
						{app.error ?? 'Open the pairing link or scan the QR code that azazel printed.'}
					</p>
				</div>
				<div class="bg-card rounded-lg border p-3.5 text-[13px] leading-relaxed">
					<p class="text-muted-foreground mb-2">On the machine that runs azazel:</p>
					<code class="bg-background block rounded border px-2.5 py-1.5 font-mono text-xs"
						>azazel remote</code
					>
					<p class="text-muted-foreground mt-2">
						Press <kbd class="bg-raised rounded border px-1 font-mono text-xs">l</kbd> there for a new
						control link. A link works once and expires after a few minutes.
					</p>
				</div>
			{:else}
				<div class="space-y-2">
					<h1 class="text-lg font-semibold">Cannot reach azazel</h1>
					<p class="text-muted-foreground text-sm" role="alert">{app.error}</p>
				</div>
				<Button class="h-11 w-full md:h-9" onclick={() => app.init()}>Try again</Button>
			{/if}
		</div>
	</div>
{/if}

<div
	class="pointer-events-none fixed inset-x-0 top-0 z-50 flex flex-col items-center gap-2 p-3 pt-[max(0.75rem,env(safe-area-inset-top))]"
	aria-live="polite"
>
	{#each toasts.items as t (t.id)}
		<div
			class="bg-card pointer-events-auto flex w-full max-w-sm items-start gap-3 rounded-lg border px-3.5 py-2.5 text-[13px] shadow-lg {t.level ===
			'error'
				? 'border-destructive/60'
				: ''}"
			role={t.level === 'error' ? 'alert' : 'status'}
		>
			<span
				class="min-w-0 flex-1 {t.level === 'error'
					? 'text-destructive'
					: t.level === 'warn'
						? 'text-accent-text'
						: t.level === 'success'
							? 'text-success'
							: ''}">{t.text}</span
			>
			<button
				class="text-muted-foreground hover:text-foreground -m-1 grid size-8 place-items-center"
				aria-label="Dismiss"
				onclick={() => toasts.dismiss(t.id)}><X class="size-3.5" /></button
			>
		</div>
	{/each}
</div>
