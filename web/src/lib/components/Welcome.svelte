<script lang="ts">
	// The first screen of a device that is not paired yet (or cannot reach the server): what the page
	// is and the one command that makes a link. One Card in the middle of the page, as sin-api's
	// sign-in.
	import Brand from './Brand.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Kbd } from '$lib/components/ui/kbd';
	import * as Card from '$lib/components/ui/card';
	import * as InputGroup from '$lib/components/ui/input-group';
	import RotateCw from '@lucide/svelte/icons/rotate-cw';
	import Copy from '@lucide/svelte/icons/copy';
	import Check from '@lucide/svelte/icons/check';

	let {
		mode,
		error,
		onretry
	}: { mode: 'needs-link' | 'error'; error: string | null; onretry: () => void } = $props();

	let copied = $state(false);
	async function copy() {
		try {
			await navigator.clipboard.writeText('azazel remote');
			copied = true;
			setTimeout(() => (copied = false), 1600);
		} catch {
			// The command stays selectable.
		}
	}
</script>

<div
	class="grid min-h-dvh place-items-center bg-background p-4 pt-[max(1rem,env(safe-area-inset-top))]"
>
	<Card.Root class="w-full max-w-md" data-welcome={mode}>
		<Card.Header>
			<div class="mb-4"><Brand /></div>
			{#if mode === 'needs-link'}
				<Card.Title role="heading" aria-level={1} class="text-lg"
					>{error ? 'That link did not work.' : 'Your agents, on any screen.'}</Card.Title
				>
				<Card.Description>
					{error ??
						'Follow a session as it works, answer what it asks and steer it, from this browser or your phone. Pair this device with a link from the computer that runs azazel.'}
				</Card.Description>
			{:else}
				<Card.Title role="heading" aria-level={1} class="text-lg">Cannot reach azazel.</Card.Title>
				<Card.Description role="alert">{error}</Card.Description>
			{/if}
		</Card.Header>
		{#if mode === 'needs-link'}
			<Card.Content class="grid gap-3">
				<InputGroup.Root>
					<InputGroup.Addon>
						<InputGroup.Text class="font-mono" aria-hidden="true">$</InputGroup.Text>
					</InputGroup.Addon>
					<InputGroup.Input
						readonly
						value="azazel remote"
						aria-label="The command to run on the computer"
						class="font-mono"
					/>
					<InputGroup.Addon align="inline-end">
						<InputGroup.Button size="icon-xs" aria-label="Copy the command" onclick={copy}>
							{#if copied}<Check class="text-success" />{:else}<Copy />{/if}
						</InputGroup.Button>
					</InputGroup.Addon>
				</InputGroup.Root>
				<p class="text-sm leading-relaxed text-muted-foreground">
					Open the link it prints, or scan its QR code. Press <Kbd>l</Kbd> there for another. A link works
					once, for fifteen minutes.
				</p>
			</Card.Content>
		{:else}
			<Card.Footer>
				<Button onclick={onretry}><RotateCw /> Try again</Button>
			</Card.Footer>
		{/if}
	</Card.Root>
</div>
