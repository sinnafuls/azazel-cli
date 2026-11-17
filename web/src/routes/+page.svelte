<script lang="ts">
	import SessionList from '$lib/components/SessionList.svelte';
	import Brand from '$lib/components/Brand.svelte';
	import PairDevice from '$lib/components/PairDevice.svelte';
	import ThemeToggle from '$lib/components/ThemeToggle.svelte';
	import { app } from '$lib/app.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import Plus from '@lucide/svelte/icons/plus';
	import QrCode from '@lucide/svelte/icons/qr-code';
	import LogOut from '@lucide/svelte/icons/log-out';

	let pairing = $state(false);
	const waiting = $derived(sessions.list.filter((s) => s.state === 'waiting for you').length);
	const summary = $derived(
		!sessions.loaded
			? 'Looking for sessions…'
			: sessions.list.length === 0
				? 'On this machine'
				: `${sessions.list.length} on this machine${waiting ? `, ${waiting} need${waiting === 1 ? 's' : ''} you` : ''}`
	);
</script>

<!-- Phone and tablet: the list is the page. Desktop has it in the sidebar, so this is a quiet prompt. -->
<div class="flex min-h-0 flex-1 flex-col lg:hidden">
	<header
		class="box-content flex h-14 shrink-0 items-center justify-between pt-[env(safe-area-inset-top)] pr-1.5 pl-4"
	>
		<Brand />
		<div class="flex">
			{#if newChat.available}
				<button
					type="button"
					class="btn btn-quiet btn-icon"
					aria-label="New chat"
					onclick={() => newChat.show()}><Plus class="size-[18px]" /></button
				>
			{/if}
			<button
				type="button"
				class="btn btn-quiet btn-icon"
				aria-label="Open on another device"
				onclick={() => (pairing = true)}><QrCode class="size-[18px]" /></button
			>
			<ThemeToggle />
			<button
				type="button"
				class="btn btn-quiet btn-icon"
				aria-label="Sign out"
				onclick={() => app.logout()}><LogOut class="size-[18px]" /></button
			>
		</div>
	</header>
	<div
		class="scroll-thin min-h-0 flex-1 overflow-y-auto pb-[max(1rem,env(safe-area-inset-bottom))]"
	>
		<div class="mx-auto w-full max-w-2xl px-4">
			<div class="pt-5 pb-4">
				<h1 class="text-2xl font-semibold tracking-[-0.04em]">Sessions</h1>
				<p class="text-muted-foreground mt-0.5 text-sm">{summary}</p>
			</div>
			<SessionList card />
		</div>
	</div>
</div>

<div class="relative hidden min-h-0 flex-1 place-items-center p-8 lg:grid">
	<div class="dotgrid" aria-hidden="true"></div>
	<div class="relative flex w-full max-w-sm flex-col items-center text-center">
		<Brand size={40} word={false} />
		<h1 class="mt-6 text-xl font-semibold tracking-[-0.03em]">Choose a session</h1>
		<p class="text-muted-foreground mt-2 text-sm leading-relaxed">
			Pick one on the left to follow it live. Sessions that wait for you are listed first.
		</p>
		<div class="mt-6 flex flex-wrap justify-center gap-2">
			{#if newChat.available}
				<button type="button" class="btn btn-primary" onclick={() => newChat.show()}
					><Plus class="size-4" /> New chat</button
				>
			{/if}
			<button type="button" class="btn btn-secondary" onclick={() => (pairing = true)}
				><QrCode class="size-4" /> Open on your phone</button
			>
		</div>
	</div>
</div>

<PairDevice open={pairing} onclose={() => (pairing = false)} />
