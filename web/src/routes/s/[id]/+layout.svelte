<script lang="ts">
	// One WebSocket per open session, shared by the chat and the agent views, with the right-hand pane
	// (agents and usage) on desktop and a bottom sheet for them on a phone.
	import { onMount, type Snippet } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { Session } from '$lib/session/session.svelte';
	import { setSession } from '$lib/context';
	import { toasts } from '$lib/toasts.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { basename } from '$lib/format';
	import { homePath } from '$lib/sidebar';
	import { viewport } from '$lib/media.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import StatusBar from '$lib/components/StatusBar.svelte';
	import AgentHub from '$lib/components/AgentHub.svelte';
	import UsagePanel from '$lib/components/UsagePanel.svelte';
	import StateDot from '$lib/components/StateDot.svelte';
	import Sheet from '$lib/components/Sheet.svelte';
	import Menu, { type MenuItem } from '$lib/components/Menu.svelte';
	import SquarePen from '@lucide/svelte/icons/square-pen';
	import PanelLeft from '@lucide/svelte/icons/panel-left';
	import Ellipsis from '@lucide/svelte/icons/ellipsis';
	import Pencil from '@lucide/svelte/icons/pencil';
	import GitFork from '@lucide/svelte/icons/git-fork';
	import Download from '@lucide/svelte/icons/download';
	import Shrink from '@lucide/svelte/icons/shrink';
	import History from '@lucide/svelte/icons/history';
	import CircleStop from '@lucide/svelte/icons/circle-stop';
	import Users from '@lucide/svelte/icons/users';
	import WifiOff from '@lucide/svelte/icons/wifi-off';

	let { children }: { children: Snippet } = $props();

	const id = $derived(page.params.id as string);
	const session = new Session({
		id: page.params.id as string,
		onToast: (t) => toasts.push(t.text, t.level),
		onTheme: (t) => app.setTerminalTheme(t)
	});
	setSession(session);

	onMount(() => {
		session.start();
		ui.session = session;
		const stopWatching = sessions.watch();
		const visible = () => document.visibilityState === 'visible' && void session.resume();
		document.addEventListener('visibilitychange', visible);
		addEventListener('online', visible);
		return () => {
			document.removeEventListener('visibilitychange', visible);
			removeEventListener('online', visible);
			stopWatching();
			if (ui.session === session) ui.session = null;
			session.stop();
		};
	});

	const info = $derived(sessions.list.find((s) => s.id === id));
	const title = $derived(
		session.state?.name ?? info?.name ?? basename(session.state?.cwd ?? info?.cwd ?? id)
	);
	const waiting = $derived(session.model.requests.length > 0);
	const stateName = $derived(waiting ? 'waiting for you' : session.busy ? 'working' : 'idle');
	const agentId = $derived(page.params.agent ?? null);
	let panel = $state<'agents' | 'usage' | null>(null);
	let rightTab = $state<'agents' | 'usage'>('agents');
	const live = $derived(
		session.model.roster.filter(
			(m) => m.id !== 'lead' && (m.state === 'running' || m.state === 'queued')
		).length
	);
	const where = $derived(session.state?.cwd ?? info?.cwd ?? '');

	// The session's own menu: what the `/` commands do, for people who never type one.
	let ask = $state<'rename' | 'end' | null>(null);
	let name = $state('');
	let working = $state(false);
	async function run(fn: () => Promise<unknown>, done?: string) {
		working = true;
		try {
			await fn();
			if (done) toasts.push(done, 'success');
			ask = null;
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		} finally {
			working = false;
		}
	}
	const actions = $derived<MenuItem[]>([
		{
			label: 'Rename',
			icon: Pencil,
			onselect: () => {
				name = title;
				ask = 'rename';
			}
		},
		{
			label: 'Fork',
			icon: GitFork,
			onselect: () => run(() => session.fork(), 'Forked: this session continues in a copy.')
		},
		{ label: 'Rewind…', icon: History, onselect: () => (ui.rewind = {}) },
		{
			label: 'Compact',
			icon: Shrink,
			disabled: session.busy,
			onselect: () => run(() => session.compact(), 'Compacting the conversation.')
		},
		{
			label: 'Export',
			icon: Download,
			onselect: () =>
				run(async () => {
					const r = await session.exportTo();
					toasts.push(`Exported to ${r.path}`, 'success', 9000);
				})
		},
		'sep',
		{ label: 'End session', icon: CircleStop, danger: true, onselect: () => (ask = 'end') }
	]);
</script>

<svelte:head><title>{waiting ? '(!) ' : ''}{title} · azazel</title></svelte:head>

<div class="grid min-h-0 flex-1 grid-cols-1 xl:grid-cols-[minmax(0,1fr)_20rem]">
	<div class="flex min-h-0 min-w-0 flex-col">
		<header class="shrink-0 pt-[env(safe-area-inset-top)]">
			<div class="flex h-12 items-center gap-1 px-2 md:px-3">
				{#if !viewport.wide}
					<button
						type="button"
						class="icon-btn"
						aria-label="Open the sidebar"
						onclick={() => (ui.drawer = true)}><PanelLeft class="size-[18px]" /></button
					>
				{/if}
				<div class="flex min-w-0 flex-1 items-baseline gap-2.5 px-1.5">
					<h1 class="flex min-w-0 items-center gap-2 text-sm leading-5 font-medium">
						<StateDot state={stateName} />
						<span class="truncate">{title}</span>
						{#if agentId}<span class="text-muted-foreground shrink-0 font-normal">/ {agentId}</span
							>{/if}
					</h1>
					{#if where}
						<p
							class="text-muted-foreground hidden min-w-0 truncate font-mono text-xs leading-4 md:block"
							title={where}
						>
							{homePath(where)}{#if session.state?.branch}<span class="text-faint"
									>{` · ${session.state.branch}`}</span
								>{/if}
						</p>
					{/if}
				</div>
				{#if newChat.available && where}
					<button
						type="button"
						class="icon-btn"
						aria-label="New chat in this folder"
						title="New chat in this folder"
						onclick={() => newChat.show(where)}><SquarePen class="size-4" /></button
					>
				{/if}
				<button
					type="button"
					class="icon-btn xl:hidden {live ? 'w-auto gap-1 px-1.5' : ''}"
					aria-label="Agents and usage"
					title="Agents and usage"
					onclick={() => (panel = 'agents')}
				>
					<Users class="size-4" />
					{#if live}<span class="text-info num text-xs font-medium">{live}</span>{/if}
				</button>
				{#if app.canControl}
					<Menu label="Session actions" items={actions}><Ellipsis class="size-4" /></Menu>
				{/if}
			</div>
			<StatusBar {session} canControl={app.canControl} />
		</header>

		{#if session.status === 'reconnecting' || session.status === 'connecting'}
			<div
				class="bg-inset text-muted-foreground flex items-center gap-2 border-y px-4 py-2 text-xs"
				role="status"
			>
				<WifiOff class="size-3.5 shrink-0" />
				{session.status === 'connecting' ? 'Connecting…' : 'Connection lost. Reconnecting…'}
			</div>
		{:else if session.status === 'ended'}
			<div class="bg-inset border-y px-4 py-2 text-xs" role="status">
				This session has ended. <a class="text-accent-text underline underline-offset-2" href="/"
					>Back to the list</a
				>
			</div>
		{:else if session.status === 'missing'}
			<div class="bg-inset border-y px-4 py-2 text-xs" role="alert">
				This session is not running any more. <a
					class="text-accent-text underline underline-offset-2"
					href="/">Back to the list</a
				>
			</div>
		{:else if session.status === 'unauthorized'}
			<div class="bg-inset text-destructive border-y px-4 py-2 text-xs" role="alert">
				This device was signed out. <button
					class="underline underline-offset-2"
					onclick={() => app.init()}>Check again</button
				>
			</div>
		{/if}

		{@render children()}
	</div>

	<aside class="hidden min-h-0 flex-col border-l xl:flex" aria-label="Agents and usage">
		<div class="flex h-12 shrink-0 items-center px-3">
			<div class="seg w-full" role="tablist">
				{#each ['agents', 'usage'] as t (t)}
					<button
						role="tab"
						aria-selected={rightTab === t}
						class="capitalize"
						onclick={() => (rightTab = t as 'agents' | 'usage')}>{t}</button
					>
				{/each}
			</div>
		</div>
		<div class="scroll-thin min-h-0 flex-1 overflow-y-auto p-4">
			{#if rightTab === 'agents'}<AgentHub
					{session}
					canControl={app.canControl}
					active={agentId ?? 'lead'}
				/>{:else}<UsagePanel {session} />{/if}
		</div>
	</aside>
</div>

<Sheet
	open={panel !== null}
	title={panel === 'usage' ? 'Usage' : 'Agents'}
	onclose={() => (panel = null)}
>
	<div class="seg mb-4" role="tablist">
		{#each ['agents', 'usage'] as t (t)}
			<button
				role="tab"
				aria-selected={panel === t}
				class="capitalize"
				onclick={() => (panel = t as 'agents' | 'usage')}>{t}</button
			>
		{/each}
	</div>
	{#if panel === 'usage'}<UsagePanel {session} />{:else}<AgentHub
			{session}
			canControl={app.canControl}
			active={agentId ?? 'lead'}
		/>{/if}
</Sheet>

<Sheet
	open={ask === 'rename'}
	title="Rename session"
	onclose={() => (ask = null)}
	class="md:max-w-md"
>
	<form
		class="space-y-4"
		onsubmit={(e) => {
			e.preventDefault();
			if (name.trim()) void run(() => session.setName(name.trim()).then(() => sessions.refresh()));
		}}
	>
		<label class="block">
			<span class="label mb-1.5 block">Name</span>
			<!-- svelte-ignore a11y_autofocus -->
			<input class="field" bind:value={name} maxlength="120" autocomplete="off" autofocus />
		</label>
		<div class="flex justify-end gap-2">
			<button type="button" class="btn btn-secondary" onclick={() => (ask = null)}>Cancel</button>
			<button type="submit" class="btn btn-primary" disabled={working || !name.trim()}
				>Rename</button
			>
		</div>
	</form>
</Sheet>

<Sheet
	open={ask === 'end'}
	title="End this session?"
	onclose={() => (ask = null)}
	class="md:max-w-md"
>
	<div class="space-y-4">
		<p class="text-muted-foreground text-sm leading-relaxed">
			<span class="text-foreground font-medium">{title}</span> stops now and leaves the list. Its conversation
			stays on the computer and can be resumed from the terminal.
		</p>
		<div class="flex justify-end gap-2">
			<button type="button" class="btn btn-secondary" onclick={() => (ask = null)}>Cancel</button>
			<button
				type="button"
				class="btn btn-destroy"
				disabled={working}
				onclick={() =>
					run(async () => {
						await session.end(true).catch(() => {});
						await sessions.refresh();
						await goto('/');
					})}>End session</button
			>
		</div>
	</div>
</Sheet>
