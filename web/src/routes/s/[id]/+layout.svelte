<script lang="ts">
	// One WebSocket per open session, shared by the chat and the agent views, with the right-hand pane
	// (agents and usage) on desktop, which slides open and shut, and a bottom sheet for them on a phone.
	import { onMount, type Snippet, untrack } from 'svelte';
	import type { Pane } from 'paneforge';
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
	import { resume } from '$lib/resume.svelte';
	import Segmented from '$lib/components/Segmented.svelte';
	import { Button } from '$lib/components/ui/button';
	import * as Tabs from '$lib/components/ui/tabs';
	import * as Resizable from '$lib/components/ui/resizable';
	import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
	import * as AlertDialog from '$lib/components/ui/alert-dialog';
	import * as Sidebar from '$lib/components/ui/sidebar';
	import * as Field from '$lib/components/ui/field';
	import { Input } from '$lib/components/ui/input';
	import PanelRight from '@lucide/svelte/icons/panel-right';
	import AgentHub from '$lib/components/AgentHub.svelte';
	import UsagePanel from '$lib/components/UsagePanel.svelte';
	import StateDot from '$lib/components/StateDot.svelte';
	import Sheet from '$lib/components/Sheet.svelte';
	import Tip from '$lib/components/Tip.svelte';
	import SquarePen from '@lucide/svelte/icons/square-pen';
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
		onToast: (t) => toasts.push(t.text, t.level, t.ms),
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

	// The right pane opens and closes as the sidebar does: its width runs for 200ms while the chat
	// takes or gives the room. It is a collapsible pane of the resizable group, so the handle stays.
	const SLIDE = 200;
	let pane = $state<Pane>();
	let paneEl = $state<HTMLElement | null>(null);
	let groupEl = $state<HTMLElement | null>(null);
	/** While the pane is on its way: its content keeps `slideWidth`, so nothing in it reflows. */
	let sliding = $state(false);
	let slideWidth = $state(0);
	let slideTimer: ReturnType<typeof setTimeout> | undefined;
	// Set by the button: the next change of `ui.rightOpen` is shown moving. What the stored layout
	// or a drag of the handle does is not.
	let animate = false;
	// The group tells every pane its size once, when it has laid them out; only what happens after
	// that (a drag that folds the pane away) is taken as the reader's choice.
	let synced = false;
	function toggleRight() {
		animate = true;
		ui.setRight(!ui.rightOpen);
	}
	$effect(() => {
		const open = ui.rightOpen;
		// Until the group has a layout there is nothing to fold; this runs again when it has.
		if (!pane || typeof pane.getSize() !== 'number') return;
		const folded = pane.isCollapsed();
		untrack(() => {
			synced = true;
			if (open !== folded) return;
			const moving = animate;
			animate = false;
			if (moving) {
				if (!open) slideWidth = paneEl?.offsetWidth ?? 0;
				sliding = true;
				clearTimeout(slideTimer);
				slideTimer = setTimeout(() => (sliding = false), SLIDE + 40);
			}
			if (open) pane!.expand();
			else pane!.collapse();
			// The width the pane is on its way to: its share of the group, less the handle's hairline.
			if (moving && open)
				slideWidth = Math.round((((groupEl?.clientWidth ?? 0) - 1) * pane!.getSize()) / 100);
		});
	});
	const slide = $derived(sliding ? 'transition-[flex-grow] duration-200 ease-linear' : '');
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
</script>

<svelte:head><title>{waiting ? '(!) ' : ''}{title} · azazel</title></svelte:head>

{#snippet notice(role: 'status' | 'alert')}
	<div
		class="flex min-h-9 shrink-0 flex-wrap items-center gap-x-1.5 border-b bg-muted/50 px-4 py-1 text-xs text-muted-foreground"
		{role}
	>
		{#if session.status === 'reconnecting' || session.status === 'connecting'}
			<WifiOff class="size-3.5 shrink-0" />
			<span
				>{session.status === 'connecting' ? 'Connecting…' : 'Connection lost. Reconnecting…'}</span
			>
		{:else if session.status === 'ended'}
			<span>This session has ended.</span>
			{#if resume.available && session.state?.session_id}
				<Button
					variant="link"
					size="xs"
					class="px-0"
					onclick={() =>
						resume
							.continueWith(session.state!.session_id)
							.catch((e) => toasts.push(e instanceof Error ? e.message : String(e), 'error'))}
					>Continue</Button
				>
				<span>or</span>
			{/if}
			<Button variant="link" size="xs" class="px-0" href="/">Back to the list</Button>
		{:else if session.status === 'missing'}
			<span>This session is not running any more.</span>
			{#if resume.available}
				<Button variant="link" size="xs" class="px-0" onclick={() => resume.show()}
					>Resume a session</Button
				>
				<span>or</span>
			{/if}
			<Button variant="link" size="xs" class="px-0" href="/">Back to the list</Button>
		{:else}
			<span class="text-destructive">This device was signed out.</span>
			<Button variant="link" size="xs" class="px-0" onclick={() => app.init()}>Check again</Button>
		{/if}
	</div>
{/snippet}

{#snippet chat()}
	<div class="flex min-h-0 min-w-0 flex-1 flex-col">
		<!-- One quiet bar, as the page header of sin-api's shell: the title at the left, icons right. -->
		<header
			class="flex h-14 shrink-0 items-center gap-2 border-b px-4 pt-[env(safe-area-inset-top)]"
			data-session-header
		>
			{#if !viewport.wide}
				<Sidebar.Trigger class="-ml-1.5" aria-label="Open the sidebar" />
			{/if}
			<h1
				class="flex min-w-0 flex-1 items-center gap-2 text-sm font-medium"
				title={where
					? homePath(where) + (session.state?.branch ? ` · ${session.state.branch}` : '')
					: undefined}
			>
				<StateDot state={stateName} />
				<span class="truncate">{title}</span>
				{#if agentId}<span class="shrink-0 font-normal text-muted-foreground">/ {agentId}</span
					>{/if}
			</h1>
			<div class="-mr-1.5 flex shrink-0 items-center gap-0.5">
				{#if newChat.available && where}
					<Tip
						text="New chat in this folder"
						side="bottom"
						aria-label="New chat in this folder"
						onclick={() => newChat.show(where)}><SquarePen /></Tip
					>
				{/if}
				{#if !viewport.lg}
					<Tip
						text="Agents and usage"
						side="bottom"
						size={live ? 'sm' : 'icon-sm'}
						aria-label="Agents and usage"
						onclick={() => (panel = 'agents')}
					>
						<Users />
						{#if live}<span class="num text-xs text-success">{live}</span>{/if}
					</Tip>
				{:else}
					<Tip
						text={ui.rightOpen ? 'Hide the side panel' : 'Show the side panel'}
						side="bottom"
						size={live && !ui.rightOpen ? 'sm' : 'icon-sm'}
						aria-label={ui.rightOpen ? 'Hide the side panel' : 'Show the side panel'}
						aria-pressed={ui.rightOpen}
						onclick={toggleRight}
					>
						<PanelRight />
						{#if live && !ui.rightOpen}<span class="num text-xs text-success">{live}</span>{/if}
					</Tip>
				{/if}
				{#if app.canControl}
					<DropdownMenu.Root>
						<DropdownMenu.Trigger>
							{#snippet child({ props })}
								<Button {...props} variant="ghost" size="icon-sm" aria-label="Session actions">
									<Ellipsis />
								</Button>
							{/snippet}
						</DropdownMenu.Trigger>
						<DropdownMenu.Content align="end" class="min-w-44">
							<DropdownMenu.Item
								onSelect={() => {
									name = title;
									ask = 'rename';
								}}><Pencil /> Rename</DropdownMenu.Item
							>
							<DropdownMenu.Item
								onSelect={() =>
									run(() => session.fork(), 'Forked: this session continues in a copy.')}
								><GitFork /> Fork</DropdownMenu.Item
							>
							<DropdownMenu.Item onSelect={() => (ui.rewind = {})}
								><History /> Rewind…</DropdownMenu.Item
							>
							<DropdownMenu.Item
								disabled={session.busy}
								onSelect={() => run(() => session.compact(), 'Compacting the conversation.')}
								><Shrink /> Compact</DropdownMenu.Item
							>
							<DropdownMenu.Item
								onSelect={() =>
									run(async () => {
										const r = await session.exportTo();
										toasts.push(`Exported to ${r.path}`, 'success', 9000);
									})}><Download /> Export</DropdownMenu.Item
							>
							<DropdownMenu.Separator />
							<DropdownMenu.Item variant="destructive" onSelect={() => (ask = 'end')}
								><CircleStop /> End session</DropdownMenu.Item
							>
						</DropdownMenu.Content>
					</DropdownMenu.Root>
				{/if}
			</div>
		</header>

		{#if session.status === 'reconnecting' || session.status === 'connecting' || session.status === 'ended'}
			{@render notice('status')}
		{:else if session.status === 'missing' || session.status === 'unauthorized'}
			{@render notice('alert')}
		{/if}

		{@render children()}
	</div>
{/snippet}

{#snippet side()}
	<Tabs.Root bind:value={ui.rightTab} class="flex h-full min-h-0 flex-col gap-0">
		<div class="flex h-14 shrink-0 items-center border-b px-4">
			<!-- The line under the chosen tab is drawn in the accent. -->
			<Tabs.List variant="line">
				<Tabs.Trigger value="agents" class="after:bg-brand">Agents</Tabs.Trigger>
				<Tabs.Trigger value="usage" class="after:bg-brand">Usage</Tabs.Trigger>
			</Tabs.List>
		</div>
		<div class="scroll-thin min-h-0 flex-1 overflow-y-auto p-4">
			<Tabs.Content value="agents">
				<AgentHub {session} canControl={app.canControl} active={agentId ?? 'lead'} />
			</Tabs.Content>
			<Tabs.Content value="usage"><UsagePanel {session} /></Tabs.Content>
		</div>
	</Tabs.Root>
{/snippet}

<div class="flex min-h-0 flex-1">
	{#if viewport.lg}
		<Resizable.PaneGroup
			bind:ref={groupEl}
			direction="horizontal"
			autoSaveId="azazel-right-pane"
			class="min-h-0 flex-1"
		>
			<Resizable.Pane defaultSize={72} minSize={40} order={1} class="flex! min-w-0 {slide}">
				{@render chat()}
			</Resizable.Pane>
			<!-- Folded away, the pane is still there at no width: its handle and its content are not. -->
			<Resizable.Handle
				aria-label="Resize the side panel"
				class={ui.rightOpen ? '' : 'hidden'}
				disabled={!ui.rightOpen}
			/>
			<Resizable.Pane
				bind:this={pane}
				bind:ref={paneEl}
				defaultSize={28}
				minSize={20}
				maxSize={50}
				collapsible
				collapsedSize={0}
				onCollapse={() => synced && pane?.isCollapsed() && ui.rightOpen && ui.setRight(false)}
				onExpand={() => synced && pane?.isExpanded() && !ui.rightOpen && ui.setRight(true)}
				class="min-w-0 overflow-hidden {slide}"
				id="right"
				order={2}
				data-right-pane={ui.rightOpen ? 'open' : 'closed'}
				data-sliding={sliding || undefined}
			>
				{#if ui.rightOpen || sliding}
					<aside
						class="h-full min-h-0"
						style:width={sliding && slideWidth ? `${slideWidth}px` : undefined}
						aria-label="Agents and usage"
						inert={!ui.rightOpen}
					>
						{@render side()}
					</aside>
				{/if}
			</Resizable.Pane>
		</Resizable.PaneGroup>
	{:else}
		{@render chat()}
	{/if}
</div>

<Sheet
	open={panel !== null}
	title={panel === 'usage' ? 'Usage' : 'Agents'}
	onclose={() => (panel = null)}
>
	<Segmented
		class="mb-4"
		label="Panel"
		value={panel ?? 'agents'}
		onchange={(v) => (panel = v)}
		options={[
			{ value: 'agents', label: 'Agents' },
			{ value: 'usage', label: 'Usage' }
		]}
	/>
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
	class="sm:max-w-md"
>
	<form
		id="rename-open-session"
		onsubmit={(e) => {
			e.preventDefault();
			if (name.trim()) void run(() => session.setName(name.trim()).then(() => sessions.refresh()));
		}}
	>
		<Field.Field>
			<Field.Label for="rename-open-session-name">Name</Field.Label>
			<Input
				id="rename-open-session-name"
				bind:value={name}
				maxlength={120}
				autocomplete="off"
				autofocus
			/>
		</Field.Field>
	</form>
	{#snippet footer()}
		<Button variant="outline" onclick={() => (ask = null)}>Cancel</Button>
		<Button type="submit" form="rename-open-session" disabled={working || !name.trim()}
			>Rename</Button
		>
	{/snippet}
</Sheet>

<AlertDialog.Root open={ask === 'end'} onOpenChange={(v) => !v && (ask = null)}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>End this session?</AlertDialog.Title>
			<AlertDialog.Description>
				<span class="font-medium text-foreground">{title}</span> stops now and leaves the list. Its conversation
				stays on the computer and can be resumed from the terminal.
			</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
			<Button
				variant="destructive"
				disabled={working}
				onclick={() =>
					run(async () => {
						await session.end(true).catch(() => {});
						await sessions.refresh();
						await goto('/');
					})}>End session</Button
			>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
