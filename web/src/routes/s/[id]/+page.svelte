<script lang="ts">
	import { getSession } from '$lib/context';
	import { app } from '$lib/app.svelte';
	import Transcript from '$lib/components/Transcript.svelte';
	import Composer from '$lib/components/Composer.svelte';
	import ApprovalSheet from '$lib/components/ApprovalSheet.svelte';
	import QuestionSheet from '$lib/components/QuestionSheet.svelte';

	const session = getSession();
	const lead = $derived(session.model.items.filter((i) => i.agent === 'lead'));
	// Requests are answered in the order they arrived; the rest wait behind the first.
	const request = $derived(session.model.requests[0]);
	const more = $derived(session.model.requests.length - 1);
</script>

<Transcript
	items={lead}
	busy={session.busy}
	canLoadEarlier={session.model.history.more}
	loadingEarlier={session.loadingEarlier}
	onEarlier={() => session.loadEarlier().catch(() => {})}
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
