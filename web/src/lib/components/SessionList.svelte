<script lang="ts">
	import { sessions, STATE_LABEL } from '$lib/sessions.svelte';
	import { basename, cost, ago } from '$lib/format';
	import StateDot from './StateDot.svelte';
	import Terminal from '@lucide/svelte/icons/terminal';
	import { onMount } from 'svelte';

	let { current = null, onnavigate }: { current?: string | null; onnavigate?: () => void } =
		$props();
	onMount(() => sessions.watch());

	// Sessions that need you come first, then the rest by start time.
	const order = (s: string) =>
		s === 'waiting for you' ? 0 : s === 'working' ? 1 : s === 'idle' ? 2 : 3;
	const list = $derived(
		[...sessions.list].sort(
			(a, b) => order(a.state) - order(b.state) || Date.parse(b.started) - Date.parse(a.started)
		)
	);
	const home = (p: string) =>
		p.replace(/^\/home\/[^/]+/, '~').replace(/^[A-Za-z]:\\Users\\[^\\]+/, '~');
</script>

<nav aria-label="Sessions" data-session-list>
	{#if !sessions.loaded}
		<ul class="space-y-1.5" aria-busy="true">
			{#each [0, 1, 2] as i (i)}<li
					class="bg-raised/50 h-[74px] animate-pulse rounded-xl"
					style="animation-delay: {i * 120}ms"
				></li>{/each}
		</ul>
	{:else if sessions.error}
		<p class="text-destructive p-3 text-sm" role="alert">{sessions.error}</p>
	{:else if list.length === 0}
		<div class="flex flex-col items-center gap-3 px-4 py-12 text-center">
			<span
				class="bg-raised text-muted-foreground grid size-11 place-items-center rounded-xl border"
				><Terminal class="size-5" /></span
			>
			<p class="text-sm font-medium">No sessions running</p>
			<p class="text-muted-foreground max-w-[16rem] text-[13px] leading-relaxed">
				Start one on this machine and it shows up here.
			</p>
			<code class="bg-inset rounded-lg border px-3 py-1.5 text-xs">azazel bg "fix the build"</code>
		</div>
	{:else}
		<ul class="space-y-1">
			{#each list as s (s.id)}
				{@const needs = s.state === 'waiting for you'}
				<li>
					<a
						href="/s/{encodeURIComponent(s.id)}"
						class="press group relative block rounded-xl border px-3.5 py-3 transition-colors {current ===
						s.id
							? 'bg-raised border-border-strong shadow-[var(--edge)]'
							: 'hover:bg-raised/60 border-transparent'}"
						aria-current={current === s.id ? 'page' : undefined}
						onclick={onnavigate}
					>
						{#if needs}<span
								class="bg-primary absolute top-3 bottom-3 left-0 w-[3px] rounded-r-full"
							></span>{/if}
						<div class="flex items-center gap-2.5">
							<StateDot state={s.state} />
							<span class="min-w-0 flex-1 truncate text-[14px] font-medium tracking-[-0.01em]"
								>{s.name ?? basename(s.cwd)}</span
							>
							<span
								class="shrink-0 text-[11px] {needs
									? 'bg-primary/15 text-accent-text rounded-full px-2 py-0.5 font-medium'
									: 'text-muted-foreground'}">{STATE_LABEL[s.state] ?? s.state}</span
							>
						</div>
						<p class="text-muted-foreground mt-1 truncate pl-5 font-mono text-[11.5px]">
							{home(s.cwd)}
						</p>
						<p class="text-faint num mt-0.5 flex gap-1.5 pl-5 text-[11.5px]">
							{#if s.mode}<span>{s.mode}</span><span aria-hidden="true">·</span>{/if}
							<span>{cost(s.cost_usd)}</span><span aria-hidden="true">·</span><span
								>{ago(s.started)}</span
							>
						</p>
					</a>
				</li>
			{/each}
		</ul>
	{/if}
</nav>
