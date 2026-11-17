<script lang="ts">
	import type { Session } from '$lib/session/session.svelte';
	import { cost, resetsIn, tokens } from '$lib/format';
	import Check from '@lucide/svelte/icons/check';

	let { session }: { session: Session } = $props();
	const st = $derived(session.state);
	const pct = $derived(
		st && st.context_window ? Math.min(100, (st.context_tokens / st.context_window) * 100) : 0
	);
	const todo = $derived(st?.todo ?? []);
	const done = $derived(todo.filter((t) => t.status === 'done').length);
	const bar = (p: number, bad = false) =>
		bad ? 'bg-destructive' : p > 85 ? 'bg-destructive' : p > 65 ? 'bg-ember' : 'bg-info';
</script>

<section aria-label="Usage" class="space-y-6" data-usage>
	{#snippet meter(label: string, used: number, note: string, bad = false)}
		<div class="px-4 py-3.5">
			<div class="flex items-baseline justify-between gap-3">
				<span class="min-w-0 truncate text-[15px] font-medium">{label}</span>
				<span class="text-muted-foreground num shrink-0 text-sm">{Math.round(used)}% used</span>
			</div>
			<div
				class="track mt-2.5 h-[7px] overflow-hidden rounded-full"
				role="meter"
				aria-valuenow={Math.round(used)}
				aria-valuemin="0"
				aria-valuemax="100"
				aria-label="{label} used"
			>
				<div
					class="h-full rounded-full {bar(used, bad)}"
					style="width: {Math.max(used, 1.5)}%"
				></div>
			</div>
			{#if note}<p class="text-muted-foreground num mt-2 text-xs">{note}</p>{/if}
		</div>
	{/snippet}

	{#if st}
		<div>
			<h3 class="group-label">Context</h3>
			<div class="group-card">
				{@render meter(
					'Context window',
					pct,
					`${tokens(st.context_tokens)} of ${tokens(st.context_window)} tokens`
				)}
			</div>
		</div>

		<div>
			<h3 class="group-label">This session</h3>
			<!-- The numbers are what the reader came for: large, aligned on one edge, labels beneath. -->
			<dl class="group-card grid grid-cols-3 divide-x">
				{#each [['Cost', cost(st.usage.cost_usd)], ['Input', tokens(st.usage.input ?? 0)], ['Output', tokens(st.usage.output ?? 0)]] as [k, v] (k)}
					<div class="flex min-w-0 flex-col-reverse px-4 py-3">
						<dt class="text-muted-foreground mt-0.5 text-xs">{k}</dt>
						<dd class="num truncate font-mono text-lg font-medium tracking-[-0.03em]">{v}</dd>
					</div>
				{/each}
			</dl>
			<p class="text-muted-foreground num mt-2 px-1 text-xs">
				{st.requests} request{st.requests === 1 ? '' : 's'} · {tokens(st.usage.cache_read ?? 0)} read
				from cache
			</p>
		</div>
	{/if}

	{#each session.usage as a (a.provider + a.account)}
		<div>
			<h3 class="group-label">
				{a.provider} <span class="text-faint font-normal">· {a.account}</span>
			</h3>
			<div class="group-card divide-y">
				{#each a.windows as w (w.name + (w.scope ?? ''))}
					{@render meter(
						`${w.name}${w.scope ? ` · ${w.scope}` : ''}`,
						w.used * 100,
						w.resets_at ? `Resets in ${resetsIn(w.resets_at)}` : '',
						w.status === 'rejected'
					)}
				{/each}
			</div>
		</div>
	{/each}

	{#if todo.length}
		<div>
			<h3 class="group-label flex justify-between">
				<span>Plan</span><span class="meta">{done}/{todo.length}</span>
			</h3>
			<ul class="group-card divide-y">
				{#each todo as t (t.text)}
					<li class="flex gap-3 px-4 py-3 text-sm leading-snug">
						<span
							class="mt-0.5 grid size-[18px] shrink-0 place-items-center rounded-full border {t.status ===
							'done'
								? 'border-success/60 text-success'
								: t.status === 'in_progress'
									? 'border-info'
									: 'border-input'}"
							aria-hidden="true"
							>{#if t.status === 'done'}<Check
									class="size-3"
								/>{:else if t.status === 'in_progress'}<span class="bg-info size-2 rounded-full"
								></span>{/if}</span
						>
						<span class={t.status === 'done' ? 'text-muted-foreground line-through' : ''}
							>{t.text}</span
						>
						<span class="sr-only">{t.status.replace('_', ' ')}</span>
					</li>
				{/each}
			</ul>
		</div>
	{/if}
</section>

<style>
	/* A meter's groove: set into the surface, so the fill reads as raised out of it. */
	.track {
		background: var(--active);
		box-shadow: inset 0 1px 1px light-dark(oklch(0.25 0.02 260 / 6%), oklch(0 0 0 / 40%));
	}
</style>
