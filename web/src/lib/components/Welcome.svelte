<script lang="ts">
	// The first screen of a device that is not paired yet (or cannot reach the server): what the page
	// is, the one command that makes a link, and a still of a session as the page shows it.
	import Brand from './Brand.svelte';
	import RotateCw from '@lucide/svelte/icons/rotate-cw';
	import Copy from '@lucide/svelte/icons/copy';
	import Check from '@lucide/svelte/icons/check';
	import List from '@lucide/svelte/icons/list';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import ArrowUp from '@lucide/svelte/icons/arrow-up';

	let {
		mode,
		error,
		onretry
	}: { mode: 'needs-link' | 'error'; error: string | null; onretry: () => void } = $props();

	let copied = $state(false);
	async function copy() {
		try {
			await navigator.clipboard.writeText('azazel remote');
			copied = true;
			setTimeout(() => (copied = false), 1600);
		} catch {
			// The command stays selectable.
		}
	}
</script>

<div class="relative isolate min-h-dvh overflow-hidden">
	<div class="ground-lit absolute inset-0 -z-10" aria-hidden="true"></div>
	<div class="grid-marks absolute inset-0 -z-10" aria-hidden="true"></div>

	<div
		class="mx-auto grid min-h-dvh w-full max-w-6xl items-center gap-12 px-5 pt-[max(1.5rem,env(safe-area-inset-top))] pb-10 sm:px-8 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.05fr)] lg:gap-16"
	>
		<div class="enter-seq max-w-xl">
			<Brand size={28} />
			{#if mode === 'needs-link'}
				<h1
					class="mt-10 text-[2.5rem] leading-[1.05] font-semibold tracking-[-0.045em] text-balance sm:text-[3.25rem]"
				>
					{error ? 'That link did not work.' : 'Your agents, on any screen.'}
				</h1>
				<p class="text-muted-foreground mt-5 max-w-md text-[17px] leading-relaxed">
					{error ??
						'Follow a session as it works, answer what it asks and steer it, from this browser or your phone. Pair this device with a link from the computer that runs azazel.'}
				</p>
				<div class="mt-8">
					<div
						class="edge-lit bg-card/80 flex items-center gap-3 rounded-2xl py-2 pr-2 pl-4 shadow-e2 backdrop-blur"
					>
						<span class="text-accent-text font-mono text-sm select-none" aria-hidden="true">$</span>
						<code class="min-w-0 flex-1 truncate font-mono text-[15px]">azazel remote</code>
						<button
							type="button"
							class="btn btn-round btn-icon"
							aria-label="Copy the command"
							onclick={copy}
							>{#if copied}<Check class="text-success size-4" />{:else}<Copy
									class="size-4"
								/>{/if}</button
						>
					</div>
					<p class="text-muted-foreground mt-4 text-sm leading-relaxed">
						Open the link it prints, or scan its QR code. Press <kbd class="kbd mx-0.5">l</kbd> there
						for another. A link works once, for fifteen minutes.
					</p>
				</div>
			{:else}
				<h1
					class="mt-10 text-[2.5rem] leading-[1.05] font-semibold tracking-[-0.045em] text-balance sm:text-[3.25rem]"
				>
					Cannot reach azazel.
				</h1>
				<p class="text-muted-foreground mt-5 max-w-md text-[17px] leading-relaxed" role="alert">
					{error}
				</p>
				<div class="mt-8">
					<button type="button" class="btn btn-primary" onclick={onretry}
						><RotateCw class="size-4" /> Try again</button
					>
				</div>
			{/if}
		</div>

		<!-- A still of the page at work: what pairing opens. -->
		<div
			class="preview edge-lit bg-card/90 relative mx-auto w-full max-w-[34rem] overflow-hidden rounded-[28px] shadow-e3 backdrop-blur"
			role="img"
			aria-label="A session as this page shows it: a request, the steps the agent took, its reply and a question waiting for an answer"
		>
			<div class="flex items-center gap-2 border-b px-4 py-3">
				<span
					class="bg-info size-2 rounded-full shadow-[0_0_0_4px_color-mix(in_oklab,var(--info)_22%,transparent)]"
				></span>
				<span class="text-sm font-semibold tracking-[-0.02em]">api-server</span>
				<span class="text-faint hidden font-mono text-xs sm:inline">~/work/api-server · main</span>
				<span class="ml-auto hidden gap-1.5 sm:flex">
					<span class="chip-still">@lead · deepseek-chat</span>
				</span>
			</div>
			<div class="space-y-4 px-5 pt-5 pb-4 text-[15px]">
				<div class="flex justify-end">
					<span class="bg-raised rounded-[20px] px-4 py-2">Fix the failing auth tests</span>
				</div>
				<div class="text-muted-foreground flex items-center gap-3">
					<List class="size-[18px] shrink-0" />
					<span class="flex-1">Ran 3 commands, edited 2 files</span>
					<span class="meta"
						><span class="text-diff-add-fg">+24</span>
						<span class="text-diff-del-fg">−9</span></span
					>
					<ChevronRight class="text-faint size-4" />
				</div>
				<p class="leading-relaxed">
					The token check compared expiry in seconds against milliseconds. Both tests pass now; one
					more change needs your say.
				</p>
				<div class="edge-lit bg-raised/60 rounded-[20px] p-4">
					<div class="flex items-center gap-2">
						<span
							class="bg-ember size-2 rounded-full shadow-[0_0_0_4px_color-mix(in_oklab,var(--ember)_25%,transparent)]"
						></span>
						<span class="font-semibold tracking-[-0.02em]">Allow shell?</span>
					</div>
					<code class="well text-ink-2 mt-3 block px-3 py-2 text-xs">cargo test -p auth</code>
					<div class="mt-3 flex gap-2">
						<span class="btn btn-primary h-9 flex-1 text-sm">Allow once</span>
						<span class="btn btn-secondary h-9 text-sm">Deny</span>
					</div>
				</div>
			</div>
			<div class="px-4 pb-4">
				<div class="composer flex items-center gap-2 p-1.5 pl-4">
					<span class="text-faint flex-1 text-sm">Steer the agent…</span>
					<span class="btn btn-primary btn-icon h-9 w-9"><ArrowUp class="size-4" /></span>
				</div>
			</div>
		</div>
	</div>
</div>

<style>
	/* A field of dots that fades out from behind the preview. */
	.grid-marks {
		background-image: radial-gradient(circle at 1px 1px, var(--border-strong) 1px, transparent 0);
		background-size: 28px 28px;
		mask-image: radial-gradient(ellipse 70% 60% at 62% 45%, black 10%, transparent 75%);
		opacity: 0.6;
	}
	.chip-still {
		border: 1px solid var(--border-strong);
		border-radius: 999px;
		padding: 0.125rem 0.625rem;
		font-family: var(--font-mono);
		font-size: 0.75rem;
		color: var(--ink-2);
	}
	.preview {
		animation: rise 900ms 200ms var(--ease-out) both;
	}
	.preview :global(.btn) {
		pointer-events: none;
	}
	@media (prefers-reduced-motion: reduce) {
		.preview {
			animation: none;
		}
	}
</style>
