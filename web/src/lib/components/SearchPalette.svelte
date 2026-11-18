<script lang="ts">
	// Search over the sessions, as a palette: type, arrows, Enter. With nothing typed it also offers
	// the two things people open it for besides a session.
	import { tick } from 'svelte';
	import { goto } from '$app/navigation';
	import { ui } from '$lib/ui.svelte';
	import { sessions, STATE_LABEL } from '$lib/sessions.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import { homePath, searchSessions, titleOf } from '$lib/sidebar';
	import StateDot from './StateDot.svelte';
	import Search from '@lucide/svelte/icons/search';
	import SquarePen from '@lucide/svelte/icons/square-pen';
	import Settings from '@lucide/svelte/icons/settings';

	let el = $state<HTMLDialogElement>();
	let input = $state<HTMLInputElement>();
	let query = $state('');
	let at = $state(0);

	type Row =
		| { kind: 'session'; id: string; label: string; hint: string; state: string }
		| { kind: 'action'; id: string; label: string; hint: string; run: () => void };

	const rows = $derived.by((): Row[] => {
		const found: Row[] = searchSessions(sessions.list, query)
			.slice(0, 30)
			.map((s) => ({
				kind: 'session',
				id: s.id,
				label: titleOf(s),
				hint: homePath(s.cwd),
				state: s.state
			}));
		if (query.trim()) return found;
		const actions: Row[] = [];
		if (newChat.available)
			actions.push({
				kind: 'action',
				id: 'new',
				label: 'New chat',
				hint: '',
				run: () => newChat.show()
			});
		actions.push({
			kind: 'action',
			id: 'settings',
			label: 'Settings',
			hint: '',
			run: () => ui.openSettings()
		});
		return [...found, ...actions];
	});
	const firstAction = $derived(rows.findIndex((r) => r.kind === 'action'));

	$effect(() => {
		const d = el;
		if (!d) return;
		if (ui.search && !d.open) {
			query = '';
			at = 0;
			d.showModal();
			void sessions.refresh();
			void tick().then(() => input?.focus());
		} else if (!ui.search && d.open) d.close();
	});
	$effect(() => {
		void query;
		at = 0;
	});

	function pick(i: number) {
		const r = rows[i];
		if (!r) return;
		ui.search = false;
		if (r.kind === 'session') void goto(`/s/${encodeURIComponent(r.id)}`);
		else r.run();
	}
	function key(e: KeyboardEvent) {
		if (e.key === 'ArrowDown') at = rows.length ? (at + 1) % rows.length : 0;
		else if (e.key === 'ArrowUp') at = rows.length ? (at - 1 + rows.length) % rows.length : 0;
		else if (e.key === 'Enter') pick(at);
		else return;
		e.preventDefault();
		void tick().then(() =>
			el?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' })
		);
	}
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<dialog
	bind:this={el}
	class="palette pop text-foreground w-[calc(100%-1.5rem)] max-w-xl overflow-hidden p-0"
	aria-label="Search sessions"
	onclose={() => (ui.search = false)}
	onclick={(e) => {
		if (e.target === el) ui.search = false;
	}}
>
	{#if ui.search}
		<div class="flex items-center gap-2.5 border-b px-3.5">
			<Search class="text-muted-foreground size-4 shrink-0" />
			<input
				bind:this={input}
				bind:value={query}
				onkeydown={key}
				class="placeholder:text-faint h-12 min-w-0 flex-1 bg-transparent text-[15px] outline-none"
				placeholder="Search sessions"
				role="combobox"
				aria-label="Search sessions"
				aria-expanded="true"
				aria-controls="palette-list"
				aria-activedescendant={rows[at] ? `palette-${rows[at].kind}-${rows[at].id}` : undefined}
				autocomplete="off"
				spellcheck="false"
			/>
			<span class="kbd hidden fine:inline-grid" aria-hidden="true">Esc</span>
		</div>
		<ul
			id="palette-list"
			role="listbox"
			aria-label="Results"
			class="scroll-thin max-h-[min(24rem,56dvh)] overflow-y-auto p-1"
		>
			{#each rows as r, i (r.kind + r.id)}
				{#if i === firstAction && i > 0}<li class="pop-sep" role="presentation"></li>{/if}
				<!-- svelte-ignore a11y_click_events_have_key_events -->
				<li
					id="palette-{r.kind}-{r.id}"
					role="option"
					aria-selected={i === at}
					class="pop-item cursor-pointer"
					onpointermove={() => (at = i)}
					onclick={() => pick(i)}
				>
					{#if r.kind === 'session'}
						<StateDot state={r.state} />
						<span class="min-w-0 flex-1 truncate">{r.label}</span>
						<span class="sr-only">{STATE_LABEL[r.state] ?? r.state}</span>
						<span class="text-muted-foreground max-w-[45%] shrink-0 truncate font-mono text-xs"
							>{r.hint}</span
						>
					{:else}
						{#if r.id === 'new'}<SquarePen class="size-3.5 shrink-0" />{:else}<Settings
								class="size-3.5 shrink-0"
							/>{/if}
						<span class="min-w-0 flex-1 truncate">{r.label}</span>
					{/if}
				</li>
			{:else}
				<li class="text-muted-foreground px-2 py-6 text-center" role="presentation">
					No session matches “{query.trim()}”.
				</li>
			{/each}
		</ul>
	{/if}
</dialog>

<style>
	.palette {
		margin: 12dvh auto auto;
		border-radius: var(--radius-2xl);
	}
	.palette::backdrop {
		background: var(--scrim);
	}
</style>
