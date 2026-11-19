<script lang="ts">
	// Search over the sessions, as the Command palette: type, arrows, Enter. With nothing typed it
	// also offers the two things people open it for besides a session.
	import { goto } from '$app/navigation';
	import { ui } from '$lib/ui.svelte';
	import { sessions, STATE_LABEL } from '$lib/sessions.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import { homePath, searchSessions, titleOf } from '$lib/sidebar';
	import * as Command from '$lib/components/ui/command';
	import StateDot from './StateDot.svelte';
	import SquarePen from '@lucide/svelte/icons/square-pen';
	import Settings from '@lucide/svelte/icons/settings';

	let query = $state('');
	const found = $derived(searchSessions(sessions.list, query).slice(0, 30));
	const actions = $derived(!query.trim());

	// Opening resets the query and reloads the list.
	$effect(() => {
		if (!ui.search) return;
		query = '';
		void sessions.refresh();
	});

	function run(fn: () => void) {
		ui.search = false;
		fn();
	}
</script>

<!-- The sessions are filtered here (title, folder, state), so the Command does not filter again. -->
<Command.Dialog
	bind:open={ui.search}
	shouldFilter={false}
	title="Search sessions"
	description="Find a session by its name or folder"
	class="top-[12dvh] sm:max-w-xl"
>
	<Command.Input placeholder="Search sessions…" aria-label="Search sessions" bind:value={query} />
	<Command.List class="scroll-thin max-h-[min(24rem,56dvh)]" aria-label="Results">
		{#if found.length}
			<Command.Group heading="Sessions">
				{#each found as s (s.id)}
					<Command.Item
						value="session:{s.id}"
						onSelect={() => run(() => void goto(`/s/${encodeURIComponent(s.id)}`))}
					>
						<StateDot state={s.state} />
						<span class="min-w-0 flex-1 truncate" data-row-text>{titleOf(s)}</span>
						<span class="sr-only">{STATE_LABEL[s.state] ?? s.state}</span>
						<span class="max-w-[45%] shrink-0 truncate font-mono text-xs text-muted-foreground"
							>{homePath(s.cwd)}</span
						>
					</Command.Item>
				{/each}
			</Command.Group>
		{:else if !actions}
			<!-- Not Command.Empty: the list is filtered here, so the component cannot tell. -->
			<p class="px-3 py-6 text-center text-sm text-muted-foreground" role="status">
				No session matches “{query.trim()}”.
			</p>
		{/if}
		{#if actions}
			{#if found.length}<Command.Separator />{/if}
			<Command.Group heading="Actions">
				{#if newChat.available}
					<Command.Item value="action:new" onSelect={() => run(() => newChat.show())}>
						<SquarePen /><span data-row-text>New chat</span>
					</Command.Item>
				{/if}
				<Command.Item value="action:settings" onSelect={() => run(() => ui.openSettings())}>
					<Settings /><span data-row-text>Settings</span>
				</Command.Item>
			</Command.Group>
		{/if}
	</Command.List>
</Command.Dialog>
