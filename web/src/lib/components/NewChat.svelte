<script lang="ts">
	// Starts a background session in a folder: one the server offers, one inside it, or (when bypass
	// is allowed on the computer) any folder typed in. A folder nobody trusted waits for the person
	// at the computer that runs `azazel remote` to allow it.
	import Segmented from '$lib/components/Segmented.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Textarea } from '$lib/components/ui/textarea';
	import * as Field from '$lib/components/ui/field';
	import * as Alert from '$lib/components/ui/alert';
	import { goto } from '$app/navigation';
	import { api } from '$lib/api';
	import { app } from '$lib/app.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import { resume } from '$lib/resume.svelte';
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
		id="new-chat"
		onsubmit={(e) => {
			e.preventDefault();
			void start();
		}}
	>
		<Field.Group class="gap-4">
			<Field.Field>
				<Field.Label for="new-chat-folder">Folder</Field.Label>
				<Input
					id="new-chat-folder"
					class="font-mono text-xs md:text-xs"
					placeholder={listing?.anywhere ? '~/projects/app' : 'Pick a folder below'}
					autocomplete="off"
					spellcheck="false"
					bind:value={cwd}
				/>
				<!-- The folders to pick from: one bordered list, with the way up and back at its edges. -->
				<div class="overflow-hidden rounded-lg border" data-folders>
					{#if listing?.at}
						<div class="flex items-center gap-1 border-b p-1">
							<Button
								variant="ghost"
								size="icon-sm"
								aria-label="Up"
								onclick={() => load(listing?.parent ?? undefined)}><ArrowUp /></Button
							>
							<span class="min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground"
								>{listing.at}</span
							>
							<Button variant="ghost" size="sm" onclick={() => (cwd = listing?.at ?? '')}
								>Use this folder</Button
							>
						</div>
					{/if}
					<ul
						class="scroll-thin grid max-h-64 grid-cols-1 gap-0.5 overflow-y-auto p-1"
						aria-label="Folders"
					>
						{#each listing?.folders ?? [] as f (f.path)}
							<li class="flex items-center gap-0.5">
								<Button
									variant="ghost"
									class="min-w-0 flex-1 justify-start font-normal aria-[current=true]:bg-muted"
									aria-current={f.path === cwd.trim() ? 'true' : undefined}
									disabled={!f.startable}
									onclick={() => (cwd = f.path)}
								>
									{#if f.kind === 'project'}
										<FolderGit class="text-muted-foreground" />
									{:else}
										<Folder_ class="text-muted-foreground" />
									{/if}
									<span class="min-w-0 flex-1 truncate text-left font-mono text-xs" data-row-text
										>{listing?.at ? basename(f.path) : f.name}</span
									>
									{#if !f.trusted}<Badge variant="outline">untrusted</Badge>{/if}
									{#if f.path === cwd.trim()}<Check class="text-success" />{/if}
								</Button>
								{#if canOpen(f)}
									<Button
										variant="ghost"
										size="icon-sm"
										class="shrink-0"
										aria-label="Open {f.name}"
										onclick={() => load(f.path)}><ChevronRight /></Button
									>
								{:else if (listing?.folders ?? []).some(canOpen)}
									<!-- The place of the button, kept so that the badges of every row end on one line. -->
									<span class="size-7 shrink-0" aria-hidden="true"></span>
								{/if}
							</li>
						{:else}
							<li class="px-2 py-4 text-sm text-muted-foreground">
								{loading
									? 'Looking for folders…'
									: listing?.at
										? 'No folders in here.'
										: 'No folders yet. Add some to `[remote] folders` on the computer, or start a session there once.'}
							</li>
						{/each}
					</ul>
					{#if listing?.at}
						<div class="border-t p-1">
							<Button
								variant="ghost"
								size="sm"
								class="w-full justify-start font-normal text-muted-foreground"
								onclick={() => load()}>Back to the start</Button
							>
						</div>
					{/if}
				</div>
			</Field.Field>

			<Field.Field>
				<Field.Label for="new-chat-prompt">First message</Field.Label>
				<Textarea
					id="new-chat-prompt"
					placeholder="Optional: the session waits for you without one"
					bind:value={prompt}
				/>
			</Field.Field>

			<Field.Field>
				<Field.Label id="new-chat-mode">Permission mode</Field.Label>
				<Segmented
					labelledby="new-chat-mode"
					bind:value={mode}
					options={[
						{ value: '', label: 'default' },
						...modes.map((m) => ({
							value: m,
							label: m,
							class: m === 'bypass' ? 'text-destructive data-[state=on]:text-destructive' : ''
						}))
					]}
				/>
			</Field.Field>

			{#if asks}
				<Alert.Root>
					<ShieldAlert class="text-warning" />
					<Alert.Description>
						This folder is not trusted. The session starts once someone at the computer allows it in
						the azazel terminal there.
					</Alert.Description>
				</Alert.Root>
			{/if}
			{#if error}
				<Alert.Root variant="destructive" role="alert">
					<Alert.Description>{error}</Alert.Description>
				</Alert.Root>
			{/if}
		</Field.Group>
	</form>
	{#snippet footer()}
		{#if resume.available}
			<Button
				variant="ghost"
				class="font-normal text-muted-foreground sm:mr-auto"
				onclick={() => {
					newChat.close();
					resume.show();
				}}>Continue an earlier session…</Button
			>
		{/if}
		<Button type="submit" form="new-chat" class="sm:min-w-36" disabled={!cwd.trim() || busy}>
			{busy ? (asks ? 'Waiting for the computer to allow it…' : 'Starting…') : 'Start'}
		</Button>
	{/snippet}
</Sheet>
