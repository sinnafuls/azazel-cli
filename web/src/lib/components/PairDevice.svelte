<script lang="ts">
	import { api } from '$lib/api';
	import { renderSVG } from 'uqr';
	import { app } from '$lib/app.svelte';
	import { Button } from '$lib/components/ui/button';
	import Sheet from './Sheet.svelte';

	let { open, onclose }: { open: boolean; onclose: () => void } = $props();

	let access = $state<'view' | 'control'>('view');
	let link = $state<{ url: string; expires: string } | null>(null);
	let error = $state<string | null>(null);
	let busy = $state(false);
	let copied = $state(false);

	// `renderSVG` output is generated here from a URL; it holds no model or user text.
	const svg = $derived(link ? renderSVG(link.url, { border: 1, ecc: 'M' }) : '');

	async function make() {
		busy = true;
		error = null;
		copied = false;
		try {
			link = await api.link(access);
		} catch (e) {
			link = null;
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
	async function copy() {
		if (!link) return;
		await navigator.clipboard?.writeText(link.url).catch(() => {});
		copied = true;
	}
	$effect(() => {
		if (!open) link = null;
	});
</script>

<Sheet
	{open}
	title="Open on another device"
	subtitle="A link works once, for a few minutes"
	{onclose}
>
	<div class="space-y-4">
		<div class="grid grid-cols-2 gap-1 rounded-lg border p-1" role="radiogroup" aria-label="Access">
			{#each [['view', 'Watch only'], ['control', 'Full control']] as [v, label] (v)}
				<button
					type="button"
					role="radio"
					aria-checked={access === v}
					disabled={v === 'control' && !app.canControl}
					class="min-h-11 rounded-md text-sm disabled:opacity-40 md:min-h-9 {access === v
						? 'bg-raised font-medium'
						: 'text-muted-foreground'}"
					onclick={() => ((access = v as 'view' | 'control'), (link = null))}>{label}</button
				>
			{/each}
		</div>
		{#if !link}
			<Button class="h-11 w-full md:h-9" disabled={busy} onclick={make}>Make a link</Button>
		{:else}
			<div
				class="mx-auto w-48 rounded-lg bg-white p-2 text-black [&>svg]:h-auto [&>svg]:w-full"
				role="img"
				aria-label="QR code of the link"
			>
				<!-- eslint-disable-next-line svelte/no-at-html-tags -->
				{@html svg}
			</div>
			<code class="bg-inset block rounded-md border p-2.5 text-xs break-all">{link.url}</code>
			<Button variant="outline" class="h-11 w-full md:h-9" onclick={copy}
				>{copied ? 'Copied' : 'Copy link'}</Button
			>
			<p class="text-muted-foreground text-xs">
				Expires {new Date(link.expires).toLocaleTimeString([], {
					hour: '2-digit',
					minute: '2-digit'
				})}. Anyone who opens it first gets in, so share it only with yourself.
			</p>
		{/if}
		{#if error}<p class="text-destructive text-[13px]" role="alert">{error}</p>{/if}
	</div>
</Sheet>
