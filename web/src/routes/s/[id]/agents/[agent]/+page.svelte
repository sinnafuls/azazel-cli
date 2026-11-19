<script lang="ts">
	// One sub-agent: its transcript, and a box to message it.
	import { page } from '$app/state';
	import { getSession } from '$lib/context';
	import { app } from '$lib/app.svelte';
	import { rowsToItems, type Item } from '$lib/session/reduce';
	import type { Row } from '$lib/protocol';
	import { toasts } from '$lib/toasts.svelte';
	import Transcript from '$lib/components/Transcript.svelte';
	import StateDot from '$lib/components/StateDot.svelte';
	import { Button } from '$lib/components/ui/button';
	import * as InputGroup from '$lib/components/ui/input-group';
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

<div class="flex h-10 shrink-0 items-center gap-2 border-b px-4" data-agent-bar>
	<Button variant="ghost" size="sm" class="-ml-2" href="/s/{encodeURIComponent(session.id)}"
		><ArrowLeft /> Lead</Button
	>
	<StateDot state={member?.state ?? 'idle'} />
	<span class="truncate font-mono text-xs font-medium">{agent}</span>
	<span class="shrink-0 text-xs text-muted-foreground">{member?.state ?? 'gone'}</span>
	{#if member?.activity}
		<span class="min-w-0 flex-1 truncate text-xs text-muted-foreground">· {member.activity}</span>
	{/if}
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
		class="shrink-0 pt-1 pb-[max(0.75rem,env(safe-area-inset-bottom))] md:pb-4"
		onsubmit={(e) => (e.preventDefault(), send())}
	>
		<div class="mx-auto w-full max-w-3xl px-4 md:px-6">
			<InputGroup.Root>
				<InputGroup.Input
					placeholder="Message {agent}…"
					aria-label="Message {agent}"
					bind:value={text}
				/>
				<InputGroup.Addon align="inline-end">
					<InputGroup.Button type="submit" variant="default" disabled={sending || !text.trim()}
						>Send</InputGroup.Button
					>
				</InputGroup.Addon>
			</InputGroup.Root>
		</div>
	</form>
{/if}
