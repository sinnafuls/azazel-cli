<script lang="ts">
	import { sessions, STATE_LABEL } from '$lib/sessions.svelte';
	import { basename, cost, ago } from '$lib/format';
	import StateDot from './StateDot.svelte';
	import { onMount } from 'svelte';

	let {
		current = null,
		card = false,
		onnavigate
	}: {
		current?: string | null;
		/** The list is the page (a phone): one raised card of divided rows instead of loose rows. */
		card?: boolean;
		onnavigate?: () => void;
	} = $props();
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
	const frame = $derived(card ? 'group-card divide-y' : 'space-y-1');
</script>

<nav aria-label="Sessions" data-session-list>
	{#if !sessions.loaded}
		<ul class={frame} aria-busy="true">
			{#each [0, 1, 2] as i (i)}
				<li class="flex gap-3 px-3 py-3.5 {card ? 'px-4' : ''}" style="opacity: {1 - i * 0.28}">
					<span class="skeleton mt-1 size-2.5 shrink-0 rounded-full"></span>
					<span class="flex-1 space-y-2.5 py-0.5">
						<span class="skeleton block h-3 w-3/5 rounded-sm"></span>
						<span class="skeleton block h-2.5 w-4/5 rounded-sm"></span>
					</span>
				</li>
			{/each}
		</ul>
	{:else if sessions.error}
		<p
			class="text-destructive rounded-lg px-3 py-3 text-sm {card ? 'group-card px-4' : ''}"
			role="alert"
		>
			{sessions.error}
		</p>
	{:else if list.length === 0}
		<div class="px-3 py-6 {card ? 'group-card px-5' : ''}">
			<p class="text-sm font-medium tracking-[-0.015em]">No sessions running</p>
			<p class="text-muted-foreground mt-1 text-sm leading-relaxed">
				Start one on this machine and it shows up here.
			</p>
			<code class="well text-ink-2 mt-4 flex gap-2 border px-3 py-2.5"
				><span class="text-faint select-none" aria-hidden="true">$</span>azazel bg "fix the build"</code
			>
		</div>
	{:else}
		<ul class={frame}>
			{#each list as s (s.id)}
				{@const needs = s.state === 'waiting for you'}
				<li>
					<a
						href="/s/{encodeURIComponent(s.id)}"
						class="row flex gap-3 {card ? 'in-card px-4 py-4' : 'rounded-[14px] px-3 py-2.5'}"
						aria-current={current === s.id ? 'page' : undefined}
						onclick={onnavigate}
					>
						<StateDot state={s.state} class="mt-[5px]" />
						<span class="min-w-0 flex-1">
							<span class="block truncate text-[15px] font-medium tracking-[-0.015em]"
								>{s.name ?? basename(s.cwd)}</span
							>
							<span class="mt-0.5 flex items-baseline gap-3">
								<span class="text-muted-foreground meta min-w-0 flex-1 truncate">{home(s.cwd)}</span
								>
								<span class="text-faint num shrink-0 text-xs">{ago(s.started)}</span>
							</span>
							<span class="num mt-1.5 flex min-w-0 items-center gap-1.5 text-xs">
								{#if needs}
									<span class="tag tag-ember">{STATE_LABEL[s.state] ?? s.state}</span>
								{:else}
									<span class="text-muted-foreground">{STATE_LABEL[s.state] ?? s.state}</span>
								{/if}
								{#if s.mode}
									<span class="text-faint" aria-hidden="true">·</span>
									<span class="text-faint">{s.mode}</span>
								{/if}
								<span class="text-faint" aria-hidden="true">·</span>
								<span class="text-faint">{cost(s.cost_usd)}</span>
							</span>
						</span>
					</a>
				</li>
			{/each}
		</ul>
	{/if}
</nav>

<style>
	.row {
		transition:
			background-color var(--dur-fast) var(--ease-out),
			box-shadow var(--dur-fast) var(--ease-out);
	}
	/* Inside the card the ring would be cut by the rounded corners: draw it inwards. */
	.row.in-card {
		outline-offset: -2px;
	}
	.row:active {
		background: var(--active);
	}
	@media (hover: hover) {
		.row:hover {
			background: var(--hover);
		}
	}
	/* The open session is lifted out of the rail. */
	.row[aria-current='page']:not(.in-card) {
		background: var(--card);
		box-shadow:
			0 0 0 1px var(--border),
			var(--elev-1),
			var(--highlight);
	}
	.row.in-card[aria-current='page'] {
		background: var(--hover);
	}
</style>
