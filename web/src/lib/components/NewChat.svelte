<script lang="ts">
	// Starts a background session in a folder: one the server offers, one inside it, or (when bypass
	// is allowed on the computer) any folder typed in. A folder nobody trusted waits for the person
	// at the computer that runs `azazel remote` to allow it.
	import { goto } from '$app/navigation';
	import { api } from '$lib/api';
	import { app } from '$lib/app.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { basename } from '$lib/format';
	import { MODES, type Folder, type Folders } from '$lib/protocol';
	import Sheet from './Sheet.svelte';
	import Folder_ from '@lucide/svelte/icons/folder';
	import FolderGit from '@lucide/svelte/icons/folder-git-2';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import ArrowUp from '@lucide/svelte/icons/arrow-up';
	import ShieldAlert from '@lucide/svelte/icons/shield-alert';
	import Check from '@lucide/svelte/icons/check';

	let listing = $state<Folders | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let cwd = $state('');
	let prompt = $state('');
	let mode = $state<string>('');
	let busy = $state(false);
	// The same request sent again (a retry, a double tap) starts nothing new.
	let requestId = $state('');

	const chosen = $derived(listing?.folders.find((f) => f.path === cwd.trim()) ?? null);
	const asks = $derived(
		!listing?.anywhere && (chosen ? !chosen.trusted : cwd.trim() !== '' && !known(cwd.trim()))
	);
	const modes = $derived(MODES.filter((m) => m !== 'bypass' || app.me?.allow_bypass));

	// Folders seen in any listing, so that one picked from inside a folder keeps its trust.
	let seen = $state<Folder[]>([]);
	function known(path: string) {
		return seen.find((f) => f.path === path)?.trusted ?? false;
	}

	async function load(under?: string) {
		loading = true;
		error = null;
		try {
			listing = await api.folders(under);
			seen = [
				...seen.filter((f) => !listing!.folders.some((g) => g.path === f.path)),
				...listing.folders
			];
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		if (!newChat.open) return;
		cwd = newChat.cwd ?? '';
		prompt = '';
		mode = '';
		busy = false;
		error = null;
		seen = [];
		requestId = crypto.randomUUID();
		void load();
	});

	async function start() {
		const folder = cwd.trim();
		if (!folder || busy) return;
		busy = true;
		error = null;
		try {
			const id = await api.start({
				cwd: folder,
				prompt: prompt.trim() || undefined,
				mode: mode || undefined,
				request_id: requestId
			});
			newChat.close();
			void sessions.refresh();
			await goto(`/s/${encodeURIComponent(id)}`);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			// A refusal is final for this request; asking again is a new one.
			requestId = crypto.randomUUID();
		} finally {
			busy = false;
		}
	}

	function canOpen(f: Folder) {
		return f.kind !== 'project' || !!listing?.anywhere;
	}
</script>

<Sheet
	open={newChat.open}
	title="New chat"
	subtitle="It runs on the computer, in the background"
	onclose={() => newChat.close()}
>
	<form
		class="space-y-4"
		onsubmit={(e) => {
			e.preventDefault();
			void start();
		}}
	>
		<div>
			<label class="label" for="new-chat-folder">Folder</label>
			<input
				id="new-chat-folder"
				class="field mt-1.5 font-mono text-xs"
				placeholder={listing?.anywhere ? '~/projects/app' : 'Pick a folder below'}
				autocomplete="off"
				spellcheck="false"
				bind:value={cwd}
			/>
		</div>

		<div class="well border">
			{#if listing?.at}
				<div class="flex items-center gap-1 border-b px-1.5 py-1">
					<button
						type="button"
						class="btn btn-quiet btn-icon"
						aria-label="Up"
						onclick={() => load(listing?.parent ?? undefined)}><ArrowUp class="size-4" /></button
					>
					<span class="meta min-w-0 flex-1 truncate font-mono">{listing.at}</span>
					<button
						type="button"
						class="btn btn-quiet px-2 text-xs"
						onclick={() => (cwd = listing?.at ?? '')}>Use this folder</button
					>
				</div>
			{/if}
			<ul class="scroll-thin max-h-64 overflow-y-auto p-1" aria-label="Folders">
				{#each listing?.folders ?? [] as f (f.path)}
					<li class="flex items-center">
						<button
							type="button"
							class="opt min-w-0 flex-1"
							aria-current={f.path === cwd.trim() ? 'true' : undefined}
							disabled={!f.startable}
							onclick={() => (cwd = f.path)}
						>
							{#if f.kind === 'project'}<FolderGit
									class="text-faint size-4 shrink-0"
								/>{:else}<Folder_ class="text-faint size-4 shrink-0" />{/if}
							<span class="min-w-0 flex-1 truncate font-mono text-xs"
								>{listing?.at ? basename(f.path) : f.name}</span
							>
							{#if !f.trusted}<span class="tag shrink-0" title="Not trusted">untrusted</span>{/if}
							{#if f.path === cwd.trim()}<Check class="text-accent-text size-4 shrink-0" />{/if}
						</button>
						{#if canOpen(f)}
							<button
								type="button"
								class="btn btn-quiet btn-icon shrink-0"
								aria-label="Open {f.name}"
								onclick={() => load(f.path)}><ChevronRight class="size-4" /></button
							>
						{/if}
					</li>
				{:else}
					<li class="text-muted-foreground px-2 py-4 text-sm">
						{loading
							? 'Looking for folders…'
							: listing?.at
								? 'No folders in here.'
								: 'No folders yet. Add some to `[remote] folders` on the computer, or start a session there once.'}
					</li>
				{/each}
			</ul>
			{#if listing?.at}
				<button
					type="button"
					class="text-muted-foreground meta w-full border-t px-3 py-2 text-left"
					onclick={() => load()}>Back to the start</button
				>
			{/if}
		</div>

		<div>
			<label class="label" for="new-chat-prompt">First message</label>
			<textarea
				id="new-chat-prompt"
				class="field mt-1.5 min-h-20"
				placeholder="Optional: the session waits for you without one"
				bind:value={prompt}></textarea>
		</div>

		<div>
			<p class="label" id="new-chat-mode">Permission mode</p>
			<div class="seg mt-1.5 flex-wrap" role="radiogroup" aria-labelledby="new-chat-mode">
				<button type="button" role="radio" aria-checked={mode === ''} onclick={() => (mode = '')}
					>default</button
				>
				{#each modes as m (m)}
					<button
						type="button"
						role="radio"
						aria-checked={mode === m}
						class={m === 'bypass' ? 'text-destructive' : ''}
						onclick={() => (mode = m)}>{m}</button
					>
				{/each}
			</div>
		</div>

		{#if asks}
			<p class="text-muted-foreground flex gap-2 text-xs leading-relaxed">
				<ShieldAlert class="text-ember mt-px size-4 shrink-0" />
				This folder is not trusted. The session starts once someone at the computer allows it in the terminal
				of azazel remote.
			</p>
		{/if}
		{#if error}<p class="text-destructive text-sm" role="alert">{error}</p>{/if}
		<button type="submit" class="btn btn-primary w-full" disabled={!cwd.trim() || busy}>
			{busy ? (asks ? 'Waiting for the computer to allow it…' : 'Starting…') : 'Start'}
		</button>
	</form>
</Sheet>
