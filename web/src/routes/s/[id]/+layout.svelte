<script lang="ts">
	// One WebSocket per open session, shared by the chat and the agent views, with the right-hand pane
	// (agents and usage) on desktop and a bottom sheet for them on a phone.
	import { onMount, type Snippet } from 'svelte';
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { Session } from '$lib/session/session.svelte';
	import { setSession } from '$lib/context';
	import { toasts } from '$lib/toasts.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { basename } from '$lib/format';
	import { viewport } from '$lib/media.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import SquarePen from '@lucide/svelte/icons/square-pen';
	import StatusBar from '$lib/components/StatusBar.svelte';
	import AgentHub from '$lib/components/AgentHub.svelte';
	import UsagePanel from '$lib/components/UsagePanel.svelte';
	import StateDot from '$lib/components/StateDot.svelte';
	import Sheet from '$lib/components/Sheet.svelte';
	import SessionList from '$lib/components/SessionList.svelte';
	import ArrowLeft from '@lucide/svelte/icons/arrow-left';
	import Users from '@lucide/svelte/icons/users';
	import WifiOff from '@lucide/svelte/icons/wifi-off';

	let { children }: { children: Snippet } = $props();

	const id = $derived(page.params.id as string);
	const session = new Session({
		id: page.params.id as string,
		onToast: (t) => toasts.push(t.text, t.level)
	});
	setSession(session);

	onMount(() => {
		session.start();
		const stopWatching = sessions.watch();
		const visible = () => document.visibilityState === 'visible' && void session.resume();
		document.addEventListener('visibilitychange', visible);
		addEventListener('online', visible);
		return () => {
			document.removeEventListener('visibilitychange', visible);
			removeEventListener('online', visible);
			stopWatching();
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
	let menu = $state(false);
	let rightTab = $state<'agents' | 'usage'>('agents');
	const live = $derived(
		session.model.roster.filter(
			(m) => m.id !== 'lead' && (m.state === 'running' || m.state === 'queued')
		).length
	);
	const where = $derived(session.state?.cwd ?? info?.cwd ?? '');
</script>

<svelte:head><title>{waiting ? '(!) ' : ''}{title} · azazel</title></svelte:head>

<div class="grid min-h-0 flex-1 grid-cols-1 xl:grid-cols-[minmax(0,1fr)_20rem]">
	<div class="flex min-h-0 min-w-0 flex-col">
		<header class="shrink-0 border-b pt-[env(safe-area-inset-top)]">
			<div class="flex h-14 items-center gap-1 px-1.5 lg:h-12 lg:pr-3 lg:pl-5">
				<a
					href="/"
					class="btn btn-quiet btn-icon lg:hidden"
					aria-label="All sessions"
					onclick={(e) => {
						if (viewport.wide) {
							e.preventDefault();
							menu = true;
						}
					}}><ArrowLeft class="size-[18px]" /></a
				>
				<div class="min-w-0 flex-1 pl-1 lg:flex lg:items-baseline lg:gap-3 lg:pl-0">
					<h1
						class="flex min-w-0 items-center gap-2.5 text-[15px] leading-5 font-semibold tracking-[-0.02em]"
					>
						<StateDot state={stateName} />
						<span class="truncate">{title}</span>
						{#if agentId}<span class="text-muted-foreground meta shrink-0 font-normal"
								>/ {agentId}</span
							>{/if}
					</h1>
					{#if where}
						<p class="text-muted-foreground meta mt-0.5 min-w-0 truncate pl-5 lg:mt-0 lg:pl-0">
							{where}{#if session.state?.branch}<span class="text-faint"
									>{` · ${session.state.branch}`}</span
								>{/if}
						</p>
					{/if}
				</div>
				{#if newChat.available && where}
					<button
						type="button"
						class="btn btn-quiet btn-icon"
						aria-label="New chat in this folder"
						title="New chat in this folder"
						onclick={() => newChat.show(where)}><SquarePen class="size-[18px]" /></button
					>
				{/if}
				<button
					type="button"
					class="btn btn-quiet xl:hidden {live ? 'gap-1.5 px-3' : 'btn-icon'}"
					aria-label="Agents and usage"
					onclick={() => (panel = 'agents')}
				>
					<Users class="size-[18px]" />
					{#if live}<span class="text-info meta font-medium">{live}</span>{/if}
				</button>
			</div>
			<StatusBar {session} canControl={app.canControl} />
		</header>

		{#if session.status === 'reconnecting' || session.status === 'connecting'}
			<div
				class="bg-inset text-muted-foreground flex items-center gap-2 border-b px-4 py-2 text-xs lg:px-5"
				role="status"
			>
				<WifiOff class="size-3.5 shrink-0" />
				{session.status === 'connecting' ? 'Connecting…' : 'Connection lost. Reconnecting…'}
			</div>
		{:else if session.status === 'ended'}
			<div class="bg-inset border-b px-4 py-2 text-xs lg:px-5" role="status">
				This session has ended. <a class="text-accent-text underline underline-offset-2" href="/"
					>Back to the list</a
				>
			</div>
		{:else if session.status === 'missing'}
			<div class="bg-inset border-b px-4 py-2 text-xs lg:px-5" role="alert">
				This session is not running any more. <a
					class="text-accent-text underline underline-offset-2"
					href="/">Back to the list</a
				>
			</div>
		{:else if session.status === 'unauthorized'}
			<div class="bg-inset text-destructive border-b px-4 py-2 text-xs lg:px-5" role="alert">
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

<Sheet open={menu} title="Sessions" onclose={() => (menu = false)}>
	<div class="-mx-2"><SessionList current={id} onnavigate={() => (menu = false)} /></div>
</Sheet>
