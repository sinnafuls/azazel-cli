<script lang="ts">
	import { sessions, STATE_LABEL } from '$lib/sessions.svelte';
	import { basename, cost, ago } from '$lib/format';
	import StateDot from './StateDot.svelte';
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
</script>

<nav aria-label="Sessions" data-session-list>
	{#if !sessions.loaded}
		<ul class="space-y-1.5" aria-busy="true">
			{#each [0, 1, 2] as i (i)}<li
					class="bg-raised/60 h-[68px] animate-pulse rounded-lg"
				></li>{/each}
		</ul>
	{:else if sessions.error}
		<p class="text-destructive p-3 text-sm" role="alert">{sessions.error}</p>
	{:else if list.length === 0}
		<div class="text-muted-foreground px-3 py-10 text-center text-sm">
			<p class="text-foreground mb-1 font-medium">No sessions running</p>
			<p>
				Start one on this machine with <code
					class="bg-raised rounded px-1.5 py-0.5 font-mono text-xs">azazel bg "…"</code
				> and it will appear here.
			</p>
		</div>
	{:else}
		<ul class="space-y-1">
			{#each list as s (s.id)}
				<li>
					<a
						href="/s/{encodeURIComponent(s.id)}"
						class="hover:bg-raised block rounded-lg border px-3 py-2.5 {current === s.id
							? 'border-input bg-raised'
							: 'border-transparent'}"
						aria-current={current === s.id ? 'page' : undefined}
						onclick={onnavigate}
					>
						<div class="flex items-center gap-2">
							<StateDot state={s.state} />
							<span class="min-w-0 flex-1 truncate text-sm font-medium"
								>{s.name ?? basename(s.cwd)}</span
							>
							<span
								class="text-muted-foreground shrink-0 text-[11px] {s.state === 'waiting for you'
									? 'text-accent-text font-medium'
									: ''}">{STATE_LABEL[s.state] ?? s.state}</span
							>
						</div>
						<div class="text-muted-foreground mt-0.5 flex items-center gap-2 pl-4 text-xs">
							<span class="min-w-0 flex-1 truncate font-mono">{s.cwd}</span>
						</div>
						<div class="text-muted-foreground num mt-0.5 flex gap-2 pl-4 text-[11px]">
							{#if s.mode}<span>{s.mode}</span><span aria-hidden="true">·</span>{/if}
							<span>{cost(s.cost_usd)}</span><span aria-hidden="true">·</span><span
								>{ago(s.started)}</span
							>
						</div>
					</a>
				</li>
			{/each}
		</ul>
	{/if}
</nav>
