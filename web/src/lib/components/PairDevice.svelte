<script lang="ts">
	// A link (and its QR code) that pairs another device, to watch or to control.
	import Segmented from '$lib/components/Segmented.svelte';
	import { api } from '$lib/api';
	import { renderSVG } from 'uqr';
	import { app } from '$lib/app.svelte';
	import { Button } from '$lib/components/ui/button';
	import * as Alert from '$lib/components/ui/alert';
	import * as CodeBlock from '$lib/components/ui/code-block';
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
	<div class="grid gap-4">
		<Segmented
			label="Access"
			bind:value={access}
			onchange={() => (link = null)}
			options={[
				{ value: 'view', label: 'Watch only' },
				{ value: 'control', label: 'Full control', disabled: !app.canControl }
			]}
		/>
		{#if !link}
			<Button class="w-full" disabled={busy} onclick={make}>Make a link</Button>
		{:else}
			<!-- The one place with colours that are not tokens: a QR code is black on white in every
			     theme, or a camera cannot read it. -->
			<div
				class="mx-auto w-48 rounded-lg bg-white p-2 text-black ring-1 ring-foreground/10 [&>svg]:h-auto [&>svg]:w-full"
				role="img"
				aria-label="QR code of the link"
				data-qr
			>
				<!-- eslint-disable-next-line svelte/no-at-html-tags -->
				{@html svg}
			</div>
			<CodeBlock.Root size="sm">
				<CodeBlock.Content code={link.url} wrap class="break-all" />
			</CodeBlock.Root>
			<Button variant="outline" class="w-full" onclick={copy}
				>{copied ? 'Copied' : 'Copy link'}</Button
			>
			<p class="text-xs leading-relaxed text-muted-foreground">
				Expires {new Date(link.expires).toLocaleTimeString([], {
					hour: '2-digit',
					minute: '2-digit'
				})}. Anyone who opens it first gets in, so share it only with yourself.
			</p>
		{/if}
		{#if error}
			<Alert.Root variant="destructive" role="alert">
				<Alert.Description>{error}</Alert.Description>
			</Alert.Root>
		{/if}
	</div>
</Sheet>
