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
</script>

<svelte:head><title>{waiting ? '(!) ' : ''}{title} · azazel</title></svelte:head>

<div class="grid min-h-0 flex-1 grid-cols-1 xl:grid-cols-[minmax(0,1fr)_20rem]">
	<div class="flex min-h-0 min-w-0 flex-col">
		<header class="bg-background shrink-0 border-b pt-[env(safe-area-inset-top)]">
			<div class="flex h-12 items-center gap-1 px-2 md:px-4">
				<a
					href="/"
					class="text-muted-foreground hover:text-foreground grid size-11 place-items-center lg:hidden"
					aria-label="All sessions"
					onclick={(e) => {
						if (viewport.wide) {
							e.preventDefault();
							menu = true;
						}
					}}><ArrowLeft class="size-[18px]" /></a
				>
				<div class="min-w-0 flex-1">
					<h1 class="flex items-center gap-2 truncate text-sm font-semibold">
						<StateDot state={stateName} class={waiting ? '' : ''} />
						<span class="truncate">{title}</span>
						{#if agentId}<span class="text-muted-foreground font-mono text-xs font-normal"
								>/ {agentId}</span
							>{/if}
					</h1>
					<p class="text-muted-foreground truncate font-mono text-[11px]">
						{session.state?.cwd ?? info?.cwd ?? ''}{session.state?.branch
							? ` · ${session.state.branch}`
							: ''}
					</p>
				</div>
				<button
					class="text-muted-foreground hover:text-foreground relative grid size-11 place-items-center xl:hidden"
					aria-label="Agents and usage"
					onclick={() => (panel = 'agents')}
				>
					<Users class="size-[18px]" />
					{#if live}<span
							class="bg-info text-background num absolute top-2 right-1.5 grid size-4 place-items-center rounded-full text-[10px] font-bold"
							>{live}</span
						>{/if}
				</button>
			</div>
			<div class="border-t px-2 md:px-3"><StatusBar {session} canControl={app.canControl} /></div>
		</header>

		{#if session.status === 'reconnecting' || session.status === 'connecting'}
			<div
				class="bg-raised text-muted-foreground flex items-center gap-2 border-b px-4 py-1.5 text-xs"
				role="status"
			>
				<WifiOff class="size-3.5" />
				{session.status === 'connecting' ? 'Connecting…' : 'Connection lost. Reconnecting…'}
			</div>
		{:else if session.status === 'ended'}
			<div class="bg-raised border-b px-4 py-2 text-xs" role="status">
				This session has ended. <a class="text-accent-text underline" href="/">Back to the list</a>
			</div>
		{:else if session.status === 'missing'}
			<div class="bg-raised border-b px-4 py-2 text-xs" role="alert">
				This session is not running any more. <a class="text-accent-text underline" href="/"
					>Back to the list</a
				>
			</div>
		{:else if session.status === 'unauthorized'}
			<div class="bg-destructive/10 text-destructive border-b px-4 py-2 text-xs" role="alert">
				This device was signed out. <button class="underline" onclick={() => app.init()}
					>Check again</button
				>
			</div>
		{/if}

		{@render children()}
	</div>

	<aside
		class="bg-card scroll-thin hidden min-h-0 flex-col overflow-y-auto border-l xl:flex"
		aria-label="Agents and usage"
	>
		<div
			class="sticky top-0 z-10 grid h-12 shrink-0 grid-cols-2 border-b bg-card px-2"
			role="tablist"
		>
			{#each ['agents', 'usage'] as t (t)}
				<button
					role="tab"
					aria-selected={rightTab === t}
					class="text-sm capitalize {rightTab === t
						? 'border-primary border-b-2 font-medium'
						: 'text-muted-foreground'}"
					onclick={() => (rightTab = t as 'agents' | 'usage')}>{t}</button
				>
			{/each}
		</div>
		<div class="p-4">
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
	<div class="mb-3 grid grid-cols-2 gap-1 rounded-lg border p-1" role="tablist">
		{#each ['agents', 'usage'] as t (t)}
			<button
				role="tab"
				aria-selected={panel === t}
				class="min-h-11 rounded-md text-sm capitalize {panel === t
					? 'bg-raised font-medium'
					: 'text-muted-foreground'}"
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
	<SessionList current={id} onnavigate={() => (menu = false)} />
</Sheet>
