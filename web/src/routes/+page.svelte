<script lang="ts">
	// No session is open. Wide: a quiet start (the sessions are in the sidebar). A phone: the list is
	// the page, under a bar that opens the sidebar.
	import SessionList from '$lib/components/SessionList.svelte';
	import Brand from '$lib/components/Brand.svelte';
	import * as Sidebar from '$lib/components/ui/sidebar';
	import * as Empty from '$lib/components/ui/empty';
	import { Button } from '$lib/components/ui/button';
	import { ui } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import { aboveToasts } from '$lib/toast-area.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import SquarePen from '@lucide/svelte/icons/square-pen';
	import QrCode from '@lucide/svelte/icons/qr-code';

	const waiting = $derived(sessions.list.filter((s) => s.state === 'waiting for you').length);
	const summary = $derived(
		!sessions.loaded
			? 'Looking for sessions…'
			: sessions.list.length === 0
				? 'On this computer'
				: `${sessions.list.length} on this computer${waiting ? `, ${waiting} need${waiting === 1 ? 's' : ''} you` : ''}`
	);
</script>

{#if !viewport.wide}
	<div class="flex min-h-0 flex-1 flex-col">
		<header
			class="flex h-14 shrink-0 items-center gap-2 border-b px-4 pt-[env(safe-area-inset-top)]"
			{@attach aboveToasts}
		>
			<Sidebar.Trigger aria-label="Open the sidebar" />
			<span class="mr-auto"><Brand /></span>
			{#if newChat.available}
				<Button variant="ghost" size="icon-sm" aria-label="New chat" onclick={() => newChat.show()}
					><SquarePen /></Button
				>
			{/if}
		</header>
		<div
			class="scroll-thin min-h-0 flex-1 overflow-y-auto pb-[max(1rem,env(safe-area-inset-bottom))]"
		>
			<div class="px-4 pt-6 pb-2">
				<h1 class="text-2xl font-semibold tracking-tight">Sessions</h1>
				<p class="mt-1 text-sm text-muted-foreground">{summary}</p>
			</div>
			<div class="px-2"><SessionList /></div>
		</div>
	</div>
{:else}
	<div class="flex min-h-0 flex-1 overflow-y-auto p-8">
		<Empty.Root>
			<Empty.Header>
				<Empty.Media><Brand size={32} word={false} /></Empty.Media>
				<Empty.Title class="text-2xl font-semibold">
					{waiting
						? `${waiting} session${waiting === 1 ? ' needs' : 's need'} you`
						: sessions.list.length
							? 'Pick up where an agent is'
							: 'What should azazel do?'}
				</Empty.Title>
				<Empty.Description>
					{sessions.list.length
						? 'Choose a session in the sidebar to follow it live. Those waiting for an answer are listed first.'
						: 'Start a chat in a folder of this computer. It keeps running when you close the browser.'}
				</Empty.Description>
			</Empty.Header>
			<Empty.Content class="flex-row flex-wrap justify-center">
				{#if newChat.available}
					<Button onclick={() => newChat.show()}><SquarePen /> New chat</Button>
				{/if}
				<Button variant="outline" onclick={() => (ui.pairing = true)}
					><QrCode /> Open on your phone</Button
				>
			</Empty.Content>
		</Empty.Root>
	</div>
{/if}
