<script lang="ts">
	// The sessions of this computer: those that need you, those at work, then the rest by project
	// folder. Every row is a Sidebar.MenuButton that is a link; its ••• is a Sidebar.MenuAction that
	// opens a DropdownMenu, shown on hover, on focus and always on touch.
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import { sessions, STATE_LABEL } from '$lib/sessions.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { oneShot } from '$lib/rpc/oneshot';
	import { arrange, homePath, titleOf } from '$lib/sidebar';
	import { ago, basename } from '$lib/format';
	import type { SessionInfo } from '$lib/protocol';
	import * as Sidebar from '$lib/components/ui/sidebar';
	import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
	import * as Collapsible from '$lib/components/ui/collapsible';
	import * as AlertDialog from '$lib/components/ui/alert-dialog';
	import * as Alert from '$lib/components/ui/alert';
	import * as Empty from '$lib/components/ui/empty';
	import * as Field from '$lib/components/ui/field';
	import { Input } from '$lib/components/ui/input';
	import { Button } from '$lib/components/ui/button';
	import StateDot from './StateDot.svelte';
	import Sheet from './Sheet.svelte';
	import Tip from './Tip.svelte';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import Ellipsis from '@lucide/svelte/icons/ellipsis';
	import Pencil from '@lucide/svelte/icons/pencil';
	import GitFork from '@lucide/svelte/icons/git-fork';
	import Download from '@lucide/svelte/icons/download';
	import CircleStop from '@lucide/svelte/icons/circle-stop';

	let {
		current = null,
		onnavigate
	}: {
		current?: string | null;
		onnavigate?: () => void;
	} = $props();
	onMount(() => sessions.watch());

	const view = $derived(arrange(sessions.list));
	// Beside the row where there is a page beside the sidebar; on a narrow screen the sidebar is a
	// sheet with no room at its side, and the tip goes under the row.
	const tipSide = $derived(viewport.wide ? 'right' : 'bottom');

	// What the row menu asked for and has to be confirmed or typed first.
	let ask = $state<{ kind: 'rename' | 'end'; s: SessionInfo } | null>(null);
	let name = $state('');
	let busy = $state(false);

	const fail = (e: unknown) => toasts.push(e instanceof Error ? e.message : String(e), 'error');
	/** The session on screen answers through its own connection; any other through a short one. */
	const open = (s: SessionInfo) => (ui.session?.id === s.id ? ui.session : null);

	async function fork(s: SessionInfo) {
		try {
			const here = open(s);
			if (here) await here.fork();
			else await oneShot(s.id, { type: 'fork_session', entry_id: null });
			toasts.push('Forked: this session now continues in a copy.', 'success');
			await sessions.refresh();
			onnavigate?.();
			await goto(`/s/${encodeURIComponent(s.id)}`);
		} catch (e) {
			fail(e);
		}
	}
	async function exportIt(s: SessionInfo) {
		try {
			const r = await (open(s)?.exportTo() ??
				oneShot<{ path: string }>(s.id, { type: 'export', format: null }));
			toasts.push(`Exported to ${r.path}`, 'success', 9000);
		} catch (e) {
			fail(e);
		}
	}
	async function confirm() {
		if (!ask) return;
		const { kind, s } = ask;
		busy = true;
		try {
			if (kind === 'rename') {
				const title = name.trim();
				if (!title) return;
				const here = open(s);
				if (here) await here.setName(title);
				else await oneShot(s.id, { type: 'set_name', name: title });
			} else {
				await oneShot(s.id, { type: 'stop', now: true });
				if (current === s.id) await goto('/');
			}
			ask = null;
			await sessions.refresh();
		} catch (e) {
			fail(e);
		} finally {
			busy = false;
		}
	}
</script>

<!-- `folder`: the row names its folder on a second line, so it is the tall (lg) button. -->
{#snippet row(s: SessionInfo, folder: boolean)}
	<Sidebar.MenuItem data-session-row>
		<Tip
			text="{homePath(s.cwd)} · {STATE_LABEL[s.state] ?? s.state}{s.mode
				? ` · ${s.mode}`
				: ''} · {ago(s.started)}"
			side={tipSide}
		>
			{#snippet child({ props: tip })}
				<Sidebar.MenuButton size={folder ? 'lg' : 'default'} isActive={current === s.id} {...tip}>
					{#snippet child({ props })}
						<a
							{...props}
							href="/s/{encodeURIComponent(s.id)}"
							aria-current={current === s.id ? 'page' : undefined}
							onclick={onnavigate}
						>
							<StateDot state={s.state} />
							<span class="sr-only">{STATE_LABEL[s.state] ?? s.state}</span>
							{#if folder}
								<span class="grid min-w-0 flex-1 gap-0.5 leading-4">
									<span class="truncate" data-row-text>{titleOf(s)}</span>
									<span class="truncate text-xs font-normal text-muted-foreground"
										>{basename(s.cwd)}</span
									>
								</span>
							{:else}
								<span data-row-text>{titleOf(s)}</span>
							{/if}
						</a>
					{/snippet}
				</Sidebar.MenuButton>
			{/snippet}
		</Tip>
		{#if app.canControl}
			<DropdownMenu.Root>
				<DropdownMenu.Trigger>
					{#snippet child({ props })}
						<!-- The stock action sits at top-2.5 beside a tall button, 4px above its middle:
						     top-3.5 puts its 20px box on the 48px row's centre. -->
						<Sidebar.MenuAction
							showOnHover
							{...props}
							aria-label="Actions for {titleOf(s)}"
							class="pointer-coarse:opacity-100 peer-data-[size=lg]/menu-button:top-3.5"
						>
							<Ellipsis />
						</Sidebar.MenuAction>
					{/snippet}
				</DropdownMenu.Trigger>
				<DropdownMenu.Content side="bottom" align="start" class="min-w-44">
					<DropdownMenu.Item
						onSelect={() => {
							name = titleOf(s);
							ask = { kind: 'rename', s };
						}}><Pencil /> Rename</DropdownMenu.Item
					>
					<DropdownMenu.Item onSelect={() => void fork(s)}><GitFork /> Fork</DropdownMenu.Item>
					<DropdownMenu.Item onSelect={() => void exportIt(s)}
						><Download /> Export</DropdownMenu.Item
					>
					<DropdownMenu.Separator />
					<DropdownMenu.Item variant="destructive" onSelect={() => (ask = { kind: 'end', s })}
						><CircleStop /> End session</DropdownMenu.Item
					>
				</DropdownMenu.Content>
			</DropdownMenu.Root>
		{/if}
	</Sidebar.MenuItem>
{/snippet}

<nav aria-label="Sessions" data-session-list>
	{#if !sessions.loaded}
		<Sidebar.Group>
			<Sidebar.Menu aria-busy="true">
				{#each [0, 1, 2, 3] as i (i)}
					<Sidebar.MenuItem><Sidebar.MenuSkeleton showIcon /></Sidebar.MenuItem>
				{/each}
			</Sidebar.Menu>
		</Sidebar.Group>
	{:else if sessions.error}
		<Sidebar.Group>
			<Alert.Root variant="destructive" role="alert">
				<Alert.Description>{sessions.error}</Alert.Description>
			</Alert.Root>
		</Sidebar.Group>
	{:else if view.total === 0}
		<Sidebar.Group>
			<Empty.Root class="items-start gap-1 p-2 text-left">
				<Empty.Title>No sessions yet</Empty.Title>
				<Empty.Description class="text-xs/relaxed"
					>Start a new chat and it shows up here.</Empty.Description
				>
			</Empty.Root>
		</Sidebar.Group>
	{:else}
		{#if view.needs.length}
			<Sidebar.Group aria-labelledby="sl-needs">
				<Sidebar.GroupLabel id="sl-needs" class="text-warning">Needs you</Sidebar.GroupLabel>
				<Sidebar.Menu>
					{#each view.needs as s (s.id)}{@render row(s, true)}{/each}
				</Sidebar.Menu>
			</Sidebar.Group>
		{/if}
		{#if view.running.length}
			<Sidebar.Group aria-labelledby="sl-running">
				<Sidebar.GroupLabel id="sl-running">Running</Sidebar.GroupLabel>
				<Sidebar.Menu>
					{#each view.running as s (s.id)}{@render row(s, true)}{/each}
				</Sidebar.Menu>
			</Sidebar.Group>
		{/if}
		{#if view.recent.length}
			<Sidebar.Group aria-labelledby="sl-recent">
				<Sidebar.GroupLabel id="sl-recent">Recent</Sidebar.GroupLabel>
				{#each view.recent as g (g.cwd)}
					{@const folded = !!ui.folded[g.cwd]}
					<Collapsible.Root open={!folded} onOpenChange={() => ui.toggleFolder(g.cwd)}>
						<Sidebar.Menu>
							<Sidebar.MenuItem>
								<Tip text={homePath(g.cwd)} side={tipSide}>
									{#snippet child({ props: tip })}
										<Collapsible.Trigger {...tip}>
											{#snippet child({ props })}
												<Sidebar.MenuButton {...props} class="pr-8 text-muted-foreground">
													<ChevronRight
														class="transition-transform duration-200 {folded ? '' : 'rotate-90'}"
													/>
													<span data-row-text>{g.name}</span>
												</Sidebar.MenuButton>
											{/snippet}
										</Collapsible.Trigger>
									{/snippet}
								</Tip>
								<Sidebar.MenuBadge class="font-normal text-muted-foreground"
									>{g.sessions.length}</Sidebar.MenuBadge
								>
							</Sidebar.MenuItem>
						</Sidebar.Menu>
						<Collapsible.Content>
							<!-- The stock sub-list, with whole rows in it (a sub-button has no action). -->
							<Sidebar.MenuSub class="mr-0 gap-0 pr-0">
								{#each g.sessions as s (s.id)}{@render row(s, false)}{/each}
							</Sidebar.MenuSub>
						</Collapsible.Content>
					</Collapsible.Root>
				{/each}
			</Sidebar.Group>
		{/if}
	{/if}
</nav>

<Sheet
	open={ask?.kind === 'rename'}
	title="Rename session"
	onclose={() => (ask = null)}
	class="sm:max-w-md"
>
	<form
		id="rename-session"
		onsubmit={(e) => {
			e.preventDefault();
			void confirm();
		}}
	>
		<Field.Field>
			<Field.Label for="rename-session-name">Name</Field.Label>
			<Input
				id="rename-session-name"
				bind:value={name}
				maxlength={120}
				autocomplete="off"
				autofocus
			/>
		</Field.Field>
	</form>
	{#snippet footer()}
		<Button variant="outline" onclick={() => (ask = null)}>Cancel</Button>
		<Button type="submit" form="rename-session" disabled={busy || !name.trim()}>Rename</Button>
	{/snippet}
</Sheet>

<AlertDialog.Root open={ask?.kind === 'end'} onOpenChange={(v) => !v && (ask = null)}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>End this session?</AlertDialog.Title>
			<AlertDialog.Description>
				<span class="font-medium text-foreground">{ask ? titleOf(ask.s) : ''}</span> stops now and leaves
				the list. Its conversation stays on the computer and can be resumed from the terminal.
			</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
			<Button variant="destructive" disabled={busy} onclick={confirm}>End session</Button>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
