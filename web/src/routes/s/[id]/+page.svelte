<script lang="ts">
	import { getSession } from '$lib/context';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { entryFor } from '$lib/session/tree';
	import type { Item } from '$lib/session/reduce';
	import Transcript from '$lib/components/Transcript.svelte';
	import Composer from '$lib/components/Composer.svelte';
	import RewindSheet from '$lib/components/RewindSheet.svelte';
	import ApprovalSheet from '$lib/components/ApprovalSheet.svelte';
	import QuestionSheet from '$lib/components/QuestionSheet.svelte';

	const session = getSession();
	const lead = $derived(session.model.items.filter((i) => i.agent === 'lead'));
	// Requests are answered in the order they arrived; the rest wait behind the first.
	const request = $derived(session.model.requests[0]);
	const more = $derived(session.model.requests.length - 1);

	/** What a message's buttons ask for. The entry of the session file is looked up first. */
	async function act(action: 'retry' | 'fork' | 'rewind' | 'edit', item: Item) {
		try {
			if (action === 'retry') return void (await session.retry());
			const entry = entryFor(await session.tree(), lead, item);
			const text = item.kind === 'user' ? item.text : '';
			if (action === 'edit') {
				ui.edit = { text, entryId: entry };
				return;
			}
			if (!entry) {
				toasts.push('That message is not in the session file yet. Try again in a moment.', 'warn');
				return;
			}
			if (action === 'fork') {
				await session.fork(entry);
				toasts.push('Forked: this session continues in a copy, from that message.', 'success');
			} else ui.rewind = { entryId: entry, text };
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		}
	}
</script>

<Transcript
	items={lead}
	busy={session.busy}
	canLoadEarlier={session.model.history.more}
	loadingEarlier={session.loadingEarlier}
	onEarlier={() => session.loadEarlier().catch(() => {})}
	onaction={app.canControl ? act : undefined}
/>

{#if request}
	{#key request.id}
		{#if request.kind === 'approval'}
			<ApprovalSheet
				{request}
				canControl={app.canControl}
				onanswer={(d, fb) => session.approve(request.id, d, fb)}
			/>
		{:else}
			<QuestionSheet
				{request}
				canControl={app.canControl}
				onanswer={(v) => session.answer(request.id, v)}
			/>
		{/if}
	{/key}
	{#if more > 0}<p class="sr-only" role="status">
			{more} more request{more > 1 ? 's' : ''} waiting
		</p>{/if}
{/if}

<Composer {session} canControl={app.canControl} />
<RewindSheet {session} />
