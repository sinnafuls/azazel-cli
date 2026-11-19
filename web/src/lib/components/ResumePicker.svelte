<script lang="ts">
	// Earlier sessions of this computer, newest first, those of the project on screen on top. Picking
	// one goes on with it in a background session (or opens the one that has it already).
	import { api } from '$lib/api';
	import { ui } from '$lib/ui.svelte';
	import { resume } from '$lib/resume.svelte';
	import { ago } from '$lib/format';
	import { homePath } from '$lib/sidebar';
	import { messageCount, projectFirst, savedTitle } from '$lib/history';
	import type { SavedSession } from '$lib/protocol';
	import Sheet from './Sheet.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import * as InputGroup from '$lib/components/ui/input-group';
	import * as Item from '$lib/components/ui/item';
	import * as Alert from '$lib/components/ui/alert';
	import Search from '@lucide/svelte/icons/search';

	let query = $state('');
	let list = $state.raw<SavedSession[]>([]);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let asked = 0;

	const here = $derived(ui.session?.state?.cwd ?? null);
	const shown = $derived(projectFirst(list, here));

	async function load(q: string) {
		const mine = ++asked;
		loading = true;
		error = null;
		try {
			const found = await api.history(q);
			if (mine === asked) list = found;
		} catch (e) {
			if (mine === asked) error = e instanceof Error ? e.message : String(e);
		} finally {
			if (mine === asked) loading = false;
		}
	}

	$effect(() => {
		if (!resume.open) return;
		query = '';
		busy = null;
		void load('');
	});

	// Typing searches the conversations too, so it waits for a pause.
	let timer: ReturnType<typeof setTimeout> | undefined;
	function typed() {
		clearTimeout(timer);
		timer = setTimeout(() => void load(query), 250);
	}

	async function pick(s: SavedSession) {
		if (busy) return;
		busy = s.id;
		error = null;
		try {
			await resume.continueWith(s.id);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = null;
		}
	}
</script>

<Sheet
	open={resume.open}
	title="Resume a session"
	subtitle="It continues on the computer, in the background"
	onclose={() => resume.close()}
>
	<!-- One column that cannot grow past the sheet: a long path ends in an ellipsis instead. -->
	<div class="grid grid-cols-1 gap-3">
		<InputGroup.Root>
			<InputGroup.Addon><Search /></InputGroup.Addon>
			<InputGroup.Input
				placeholder="Search earlier sessions"
				aria-label="Search earlier sessions"
				autocomplete="off"
				spellcheck="false"
				bind:value={query}
				oninput={typed}
			/>
		</InputGroup.Root>
		{#if error}
			<Alert.Root variant="destructive" role="alert">
				<Alert.Description>{error}</Alert.Description>
			</Alert.Root>
		{/if}
		<ul class="-mx-2.5 grid grid-cols-1 gap-0.5" aria-label="Earlier sessions">
			{#each shown as s (s.id)}
				<li>
					<Item.Root size="xs" class="w-full flex-nowrap text-left hover:bg-muted">
						{#snippet child({ props })}
							<button type="button" {...props} disabled={!!busy} onclick={() => void pick(s)}>
								<Item.Content class="min-w-0">
									<Item.Title class="w-full">
										<span class="min-w-0 flex-1 truncate" data-row-text>{savedTitle(s)}</span>
										{#if s.running}<Badge variant="secondary">running</Badge>{/if}
										<span class="shrink-0 text-xs font-normal text-muted-foreground"
											>{ago(s.updated)}</span
										>
									</Item.Title>
									<span class="flex w-full min-w-0 gap-2 text-xs text-muted-foreground">
										<span class="min-w-0 flex-1 truncate font-mono">{homePath(s.cwd)}</span>
										<span class="shrink-0"
											>{busy === s.id ? 'Starting…' : messageCount(s.messages)}</span
										>
									</span>
									{#if s.name && s.first_prompt}
										<span class="w-full truncate text-xs text-muted-foreground"
											>{s.first_prompt}</span
										>
									{/if}
								</Item.Content>
							</button>
						{/snippet}
					</Item.Root>
				</li>
			{:else}
				<li class="px-2.5 py-4 text-sm text-muted-foreground">
					{loading
						? 'Looking for sessions…'
						: query.trim()
							? 'No earlier session mentions that.'
							: 'No earlier sessions in the folders open to browsers.'}
				</li>
			{/each}
		</ul>
	</div>
</Sheet>
