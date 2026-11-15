<script lang="ts">
	import SessionList from '$lib/components/SessionList.svelte';
	import Brand from '$lib/components/Brand.svelte';
	import PairDevice from '$lib/components/PairDevice.svelte';
	import { app } from '$lib/app.svelte';
	import QrCode from '@lucide/svelte/icons/qr-code';
	import LogOut from '@lucide/svelte/icons/log-out';
	import Sun from '@lucide/svelte/icons/sun';

	let pairing = $state(false);
</script>

<!-- Phone and tablet: the list is the page. Desktop has it in the sidebar, so this is a quiet prompt. -->
<div class="flex min-h-0 flex-1 flex-col lg:hidden">
	<header
		class="flex h-12 shrink-0 items-center justify-between border-b px-4 pt-[env(safe-area-inset-top)] box-content"
	>
		<Brand />
		<div class="-mr-2 flex">
			<button
				class="text-muted-foreground hover:text-foreground grid size-11 place-items-center"
				aria-label="Open on another device"
				onclick={() => (pairing = true)}><QrCode class="size-[18px]" /></button
			>
			<button
				class="text-muted-foreground hover:text-foreground grid size-11 place-items-center"
				aria-label="Theme: {app.theme}"
				onclick={() =>
					app.setTheme(app.theme === 'dark' ? 'light' : app.theme === 'light' ? 'system' : 'dark')}
				><Sun class="size-[18px]" /></button
			>
			<button
				class="text-muted-foreground hover:text-foreground grid size-11 place-items-center"
				aria-label="Sign out"
				onclick={() => app.logout()}><LogOut class="size-[18px]" /></button
			>
		</div>
	</header>
	<div
		class="scroll-thin min-h-0 flex-1 overflow-y-auto p-2 pb-[max(0.5rem,env(safe-area-inset-bottom))]"
	>
		<h1 class="text-muted-foreground px-2 pt-2 pb-2 text-xs">Sessions on this machine</h1>
		<SessionList />
	</div>
</div>

<div
	class="hidden min-h-0 flex-1 flex-col items-center justify-center gap-3 p-8 text-center lg:flex"
>
	<p class="text-sm font-medium">Choose a session</p>
	<p class="text-muted-foreground max-w-xs text-[13px] leading-relaxed">
		Pick one on the left to follow it live. Sessions that wait for you are listed first.
	</p>
	<button
		class="hover:bg-raised mt-1 flex h-9 items-center gap-2 rounded-md border px-3 text-[13px]"
		onclick={() => (pairing = true)}><QrCode class="size-4" /> Open on your phone</button
	>
</div>

<PairDevice open={pairing} onclose={() => (pairing = false)} />
