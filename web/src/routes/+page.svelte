<script lang="ts">
	// No session is open. Wide: a quiet start (the sessions are in the sidebar). A phone: the list is
	// the page, under a bar that opens the sidebar.
	import SessionList from '$lib/components/SessionList.svelte';
	import Brand from '$lib/components/Brand.svelte';
	import { ui } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import SquarePen from '@lucide/svelte/icons/square-pen';
	import QrCode from '@lucide/svelte/icons/qr-code';
	import PanelLeft from '@lucide/svelte/icons/panel-left';

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
		<header class="flex h-12 shrink-0 items-center gap-1 px-2 pt-[env(safe-area-inset-top)]">
			<button
				type="button"
				class="icon-btn"
				aria-label="Open the sidebar"
				onclick={() => (ui.drawer = true)}><PanelLeft class="size-[18px]" /></button
			>
			<span class="mr-auto pl-1"><Brand size={22} /></span>
			{#if newChat.available}
				<button type="button" class="icon-btn" aria-label="New chat" onclick={() => newChat.show()}
					><SquarePen class="size-[18px]" /></button
				>
			{/if}
		</header>
		<div
			class="scroll-thin min-h-0 flex-1 overflow-y-auto pb-[max(1rem,env(safe-area-inset-bottom))]"
		>
			<div class="px-4 pt-4 pb-3">
				<h1 class="text-2xl font-semibold tracking-[-0.03em]">Sessions</h1>
				<p class="text-muted-foreground mt-0.5 text-sm">{summary}</p>
			</div>
			<div class="px-2"><SessionList /></div>
		</div>
	</div>
{:else}
	<div class="grid min-h-0 flex-1 place-items-center overflow-y-auto p-8">
		<div class="enter-seq flex w-full max-w-md flex-col items-center text-center">
			<Brand size={40} word={false} />
			<h1 class="mt-6 text-[1.75rem] leading-tight font-semibold tracking-[-0.035em] text-balance">
				{waiting
					? `${waiting} session${waiting === 1 ? ' needs' : 's need'} you`
					: sessions.list.length
						? 'Pick up where an agent is'
						: 'What should azazel do?'}
			</h1>
			<p class="text-muted-foreground mt-2.5 text-[15px] leading-relaxed text-pretty">
				{sessions.list.length
					? 'Choose a session in the sidebar to follow it live. Those waiting for an answer are listed first.'
					: 'Start a chat in a folder of this computer. It keeps running when you close the browser.'}
			</p>
			<div class="mt-7 flex flex-wrap justify-center gap-2.5">
				{#if newChat.available}
					<button type="button" class="btn btn-primary" onclick={() => newChat.show()}
						><SquarePen class="size-4" /> New chat</button
					>
				{/if}
				<button type="button" class="btn btn-secondary" onclick={() => (ui.pairing = true)}
					><QrCode class="size-4" /> Open on your phone</button
				>
			</div>
		</div>
	</div>
{/if}
