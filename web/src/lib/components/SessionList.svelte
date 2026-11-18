<script lang="ts">
	// The sessions of this computer: those that need you, those at work, then the rest by project
	// folder. Each row is one link; its ••• menu shows on hover, on focus and always on touch.
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { sessions, STATE_LABEL } from '$lib/sessions.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { oneShot } from '$lib/rpc/oneshot';
	import { arrange, homePath, titleOf } from '$lib/sidebar';
	import { ago, basename } from '$lib/format';
	import type { SessionInfo } from '$lib/protocol';
	import StateDot from './StateDot.svelte';
	import Menu, { type MenuItem } from './Menu.svelte';
	import Sheet from './Sheet.svelte';
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
	const actions = (s: SessionInfo): MenuItem[] => [
		{
			label: 'Rename',
			icon: Pencil,
			onselect: () => {
				name = titleOf(s);
				ask = { kind: 'rename', s };
			}
		},
		{ label: 'Fork', icon: GitFork, onselect: () => void fork(s) },
		{ label: 'Export', icon: Download, onselect: () => void exportIt(s) },
		'sep',
		{
			label: 'End session',
			icon: CircleStop,
			danger: true,
			onselect: () => (ask = { kind: 'end', s })
		}
	];
</script>

{#snippet row(s: SessionInfo, folder: boolean)}
	<li class="row relative">
		<a
			href="/s/{encodeURIComponent(s.id)}"
			class="side-row {app.canControl ? 'pr-8' : ''}"
			aria-current={current === s.id ? 'page' : undefined}
			title="{homePath(s.cwd)} · {STATE_LABEL[s.state] ?? s.state}{s.mode ? ` · ${s.mode}` : ''}"
			onclick={onnavigate}
		>
			<StateDot state={s.state} />
			<span class="min-w-0 flex-1 truncate">{titleOf(s)}</span>
			<span class="sr-only">{STATE_LABEL[s.state] ?? s.state}</span>
			<span class="meta-right text-faint num shrink-0 text-xs"
				>{folder ? basename(s.cwd) : ago(s.started).replace(' ago', '')}</span
			>
		</a>
		{#if app.canControl}
			<span class="more">
				<Menu label="Actions for {titleOf(s)}" items={actions(s)}>
					<Ellipsis class="size-4" />
				</Menu>
			</span>
		{/if}
	</li>
{/snippet}

<nav aria-label="Sessions" data-session-list>
	{#if !sessions.loaded}
		<ul aria-busy="true">
			{#each [0, 1, 2, 3] as i (i)}
				<li class="flex h-8 items-center gap-2 px-2" style="opacity: {1 - i * 0.22}">
					<span class="skeleton size-2.5 shrink-0 rounded-full"></span>
					<span class="skeleton block h-2.5 rounded-sm" style="width: {70 - i * 12}%"></span>
				</li>
			{/each}
		</ul>
	{:else if sessions.error}
		<p class="text-destructive px-2 py-2 text-[13px] leading-snug" role="alert">{sessions.error}</p>
	{:else if view.total === 0}
		<div class="px-2 py-3">
			<p class="text-[13px] font-medium">No sessions yet</p>
			<p class="text-muted-foreground mt-1 text-[13px] leading-relaxed">
				Start a new chat and it shows up here.
			</p>
		</div>
	{:else}
		{#if view.needs.length}
			<section aria-labelledby="sl-needs" class="mb-3">
				<h3 id="sl-needs" class="pop-label text-ember-text">Needs you</h3>
				<ul>
					{#each view.needs as s (s.id)}{@render row(s, true)}{/each}
				</ul>
			</section>
		{/if}
		{#if view.running.length}
			<section aria-labelledby="sl-running" class="mb-3">
				<h3 id="sl-running" class="pop-label">Running</h3>
				<ul>
					{#each view.running as s (s.id)}{@render row(s, true)}{/each}
				</ul>
			</section>
		{/if}
		{#if view.recent.length}
			<section aria-labelledby="sl-recent">
				<h3 id="sl-recent" class="pop-label">Recent</h3>
				{#each view.recent as g (g.cwd)}
					{@const folded = !!ui.folded[g.cwd]}
					<div class="mb-1">
						<button
							type="button"
							class="side-row folder"
							aria-expanded={!folded}
							title={homePath(g.cwd)}
							onclick={() => ui.toggleFolder(g.cwd)}
						>
							<ChevronRight class="chev size-3.5 shrink-0 {folded ? '' : 'rotate-90'}" />
							<span class="min-w-0 flex-1 truncate">{g.name}</span>
							<span class="text-faint num text-xs">{g.sessions.length}</span>
						</button>
						{#if !folded}
							<ul class="pl-3">
								{#each g.sessions as s (s.id)}{@render row(s, false)}{/each}
							</ul>
						{/if}
					</div>
				{/each}
			</section>
		{/if}
	{/if}
</nav>

<Sheet
	open={ask?.kind === 'rename'}
	title="Rename session"
	onclose={() => (ask = null)}
	class="md:max-w-md"
>
	<form
		class="space-y-4"
		onsubmit={(e) => {
			e.preventDefault();
			void confirm();
		}}
	>
		<label class="block">
			<span class="label mb-1.5 block">Name</span>
			<!-- svelte-ignore a11y_autofocus -->
			<input class="field" bind:value={name} maxlength="120" autocomplete="off" autofocus />
		</label>
		<div class="flex justify-end gap-2">
			<button type="button" class="btn btn-secondary" onclick={() => (ask = null)}>Cancel</button>
			<button type="submit" class="btn btn-primary" disabled={busy || !name.trim()}>Rename</button>
		</div>
	</form>
</Sheet>

<Sheet
	open={ask?.kind === 'end'}
	title="End this session?"
	onclose={() => (ask = null)}
	class="md:max-w-md"
>
	<div class="space-y-4">
		<p class="text-muted-foreground text-sm leading-relaxed">
			<span class="text-foreground font-medium">{ask ? titleOf(ask.s) : ''}</span> stops now and leaves
			the list. Its conversation stays on the computer and can be resumed from the terminal.
		</p>
		<div class="flex justify-end gap-2">
			<button type="button" class="btn btn-secondary" onclick={() => (ask = null)}>Cancel</button>
			<button type="button" class="btn btn-destroy" disabled={busy} onclick={confirm}
				>End session</button
			>
		</div>
	</div>
</Sheet>

<style>
	.folder {
		color: var(--muted-foreground);
		font-weight: 500;
		gap: 0.375rem;
	}
	.folder :global(.chev) {
		transition: transform var(--dur) var(--ease-out);
	}
	/* The ••• takes the place of the meta on the right once the row is pointed at or focused. */
	.more {
		position: absolute;
		top: 50%;
		right: 0.25rem;
		display: grid;
		transform: translateY(-50%);
		opacity: 0;
	}
	.row:hover .more,
	.row:focus-within .more,
	.more:has(:global([aria-expanded='true'])) {
		opacity: 1;
	}
	.row:hover .meta-right,
	.row:focus-within .meta-right,
	.row:has(:global([aria-expanded='true'])) .meta-right {
		visibility: hidden;
	}
	.row:has(:global([aria-expanded='true'])) > a {
		background: var(--hover);
	}
	@media (pointer: coarse) {
		.more {
			opacity: 1;
		}
		.meta-right {
			display: none;
		}
		.row > a {
			padding-right: 2.75rem;
		}
	}
</style>
