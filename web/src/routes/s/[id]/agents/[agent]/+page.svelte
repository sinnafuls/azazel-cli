<script lang="ts">
	import { page } from '$app/state';
	import { getSession } from '$lib/context';
	import { app } from '$lib/app.svelte';
	import { rowsToItems, type Item } from '$lib/session/reduce';
	import type { Row } from '$lib/protocol';
	import { toasts } from '$lib/toasts.svelte';
	import Transcript from '$lib/components/Transcript.svelte';
	import StateDot from '$lib/components/StateDot.svelte';
	import { Button } from '$lib/components/ui/button';
	import ArrowLeft from '@lucide/svelte/icons/arrow-left';

	const session = getSession();
	const agent = $derived(page.params.agent as string);
	const member = $derived(session.model.roster.find((m) => m.id === agent));
	const live = $derived(session.model.items.filter((i) => i.agent === agent));
	let history = $state.raw<Item[]>([]);
	let loaded = $state(false);
	let text = $state('');
	let sending = $state(false);

	// What happened before this page opened comes from the agent's transcript; what follows is live.
	$effect(() => {
		const a = agent;
		loaded = false;
		history = [];
		session
			.agentHistory(a)
			.then((rows: Row[]) => {
				if (a === agent) history = rowsToItems(rows, a, `ah-${a}-`);
			})
			.catch(() => {})
			.finally(() => (loaded = a === agent || loaded));
	});

	// A live run replays the same text the history already holds; show history first, then what is newer.
	const items = $derived.by(() => {
		if (!history.length) return live;
		const seen = new Set(
			history.filter((h) => h.kind === 'user').map((h) => (h as { text: string }).text)
		);
		return [...history, ...live.filter((i) => !(i.kind === 'user' && seen.has(i.text)))];
	});
	const busy = $derived(member?.state === 'running');

	async function send() {
		const msg = text.trim();
		if (!msg || sending) return;
		sending = true;
		try {
			await session.agentSend(agent, msg);
			text = '';
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		} finally {
			sending = false;
		}
	}
</script>

<div class="bg-inset flex shrink-0 items-center gap-2 border-b px-1.5 py-1 lg:px-3">
	<a href="/s/{encodeURIComponent(session.id)}" class="btn btn-quiet gap-1.5 px-2.5 text-xs"
		><ArrowLeft class="size-4" /> Lead</a
	>
	<div class="min-w-0 flex-1">
		<div class="flex items-center gap-2">
			<StateDot state={member?.state ?? 'idle'} /><span
				class="truncate font-mono text-xs font-medium">{agent}</span
			><span class="text-muted-foreground shrink-0 text-xs">{member?.state ?? 'gone'}</span>
		</div>
		{#if member?.activity}<p class="text-muted-foreground truncate pl-[18px] text-xs">
				{member.activity}
			</p>{/if}
	</div>
</div>

<Transcript
	{items}
	{busy}
	showAgent={false}
	empty={loaded ? 'This agent has not produced anything yet.' : 'Loading…'}
/>

{#if app.canControl}
	<!-- The same box as the lead's composer, on the same column. -->
	<form
		class="shrink-0 pt-1 pb-[max(0.75rem,env(safe-area-inset-bottom))] md:pb-5"
		onsubmit={(e) => (e.preventDefault(), send())}
	>
		<div class="col">
			<div class="composer flex items-center gap-1 p-1.5">
				<input
					class="placeholder:text-faint h-11 min-w-0 flex-1 bg-transparent px-2.5 text-[15px] outline-none fine:h-9"
					placeholder="Message {agent}…"
					aria-label="Message {agent}"
					bind:value={text}
				/>
				<Button type="submit" disabled={sending || !text.trim()}>Send</Button>
			</div>
		</div>
	</form>
{/if}
