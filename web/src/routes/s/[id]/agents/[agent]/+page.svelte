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

<div class="bg-card flex shrink-0 items-center gap-3 border-b px-3 py-2 md:px-6">
	<a
		href="/s/{encodeURIComponent(session.id)}"
		class="text-muted-foreground hover:text-foreground hover:bg-raised flex min-h-11 items-center gap-1.5 rounded-md pr-2 text-xs md:min-h-8"
		><ArrowLeft class="size-4" /> Lead</a
	>
	<div class="min-w-0 flex-1">
		<div class="flex items-center gap-2">
			<StateDot state={member?.state ?? 'idle'} /><span
				class="truncate font-mono text-[13px] font-medium">{agent}</span
			><span class="text-muted-foreground text-xs">{member?.state ?? 'gone'}</span>
		</div>
		{#if member?.activity}<p class="text-muted-foreground truncate text-xs">
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
	<form
		class="bg-background border-t px-3 pt-2.5 pb-[max(0.625rem,env(safe-area-inset-bottom))] md:px-6"
		onsubmit={(e) => (e.preventDefault(), send())}
	>
		<div
			class="bg-card focus-within:border-ring mx-auto flex max-w-3xl items-center gap-1 rounded-xl border p-1.5"
		>
			<input
				class="placeholder:text-muted-foreground h-11 min-w-0 flex-1 bg-transparent px-2 text-[15px] outline-none md:h-9"
				placeholder="Message {agent}…"
				aria-label="Message {agent}"
				bind:value={text}
			/>
			<Button type="submit" class="h-11 md:h-9" disabled={sending || !text.trim()}>Send</Button>
		</div>
	</form>
{/if}
