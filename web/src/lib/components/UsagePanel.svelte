<script lang="ts">
	// What the session used: the context window, cost and tokens, the provider's usage windows and
	// the agent's plan. Each group is a small Card; every meter is a Progress filled in the accent.
	import type { Session } from '$lib/session/session.svelte';
	import { cost, resetsIn, tokens } from '$lib/format';
	import * as Card from '$lib/components/ui/card';
	import { Progress } from '$lib/components/ui/progress';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import CircleDot from '@lucide/svelte/icons/circle-dot';
	import Circle from '@lucide/svelte/icons/circle';

	let { session }: { session: Session } = $props();
	const st = $derived(session.state);
	const pct = $derived(
		st && st.context_window ? Math.min(100, (st.context_tokens / st.context_window) * 100) : 0
	);
	const todo = $derived(st?.todo ?? []);
	const done = $derived(todo.filter((t) => t.status === 'done').length);
	// The fill of a meter: the accent, amber when it runs high, red when it is nearly full or refused.
	const FILL = {
		ok: '[&>[data-slot=progress-indicator]]:bg-brand',
		high: '[&>[data-slot=progress-indicator]]:bg-warning',
		full: '[&>[data-slot=progress-indicator]]:bg-destructive'
	};
	const fill = (p: number, bad = false) =>
		bad || p > 85 ? FILL.full : p > 65 ? FILL.high : FILL.ok;
	const heading = 'mb-2 flex h-5 items-center text-xs font-medium text-muted-foreground';
</script>

<section aria-label="Usage" class="grid gap-6" data-usage>
	{#snippet meter(label: string, used: number, note: string, bad = false)}
		<div class="grid gap-1.5">
			<div class="flex items-baseline justify-between gap-3">
				<span class="min-w-0 truncate text-sm font-medium">{label}</span>
				<span class="num shrink-0 text-xs text-muted-foreground">{Math.round(used)}% used</span>
			</div>
			<Progress
				value={Math.round(used)}
				max={100}
				aria-label="{label} used"
				class={fill(used, bad)}
			/>
			{#if note}<p class="num text-xs text-muted-foreground">{note}</p>{/if}
		</div>
	{/snippet}

	{#if st}
		<div>
			<h3 class={heading}>Context</h3>
			<Card.Root size="sm">
				<Card.Content>
					{@render meter(
						'Context window',
						pct,
						`${tokens(st.context_tokens)} of ${tokens(st.context_window)} tokens`
					)}
				</Card.Content>
			</Card.Root>
		</div>

		<div>
			<h3 class={heading}>This session</h3>
			<!-- The numbers are what the reader came for: larger, on one edge, their labels beneath. -->
			<Card.Root size="sm">
				<Card.Content>
					<dl class="grid grid-cols-3 gap-3">
						{#each [['Cost', cost(st.usage.cost_usd)], ['Input', tokens(st.usage.input ?? 0)], ['Output', tokens(st.usage.output ?? 0)]] as [k, v] (k)}
							<div class="flex min-w-0 flex-col-reverse">
								<dt class="text-xs text-muted-foreground">{k}</dt>
								<dd class="num truncate text-base font-medium">{v}</dd>
							</div>
						{/each}
					</dl>
				</Card.Content>
			</Card.Root>
			<p class="num mt-2 text-xs text-muted-foreground">
				{st.requests} request{st.requests === 1 ? '' : 's'} · {tokens(st.usage.cache_read ?? 0)} read
				from cache
			</p>
		</div>
	{/if}

	{#each session.usage as a (a.provider + a.account)}
		<div>
			<h3 class={heading}>
				<span class="truncate">{a.provider} <span class="font-normal">· {a.account}</span></span>
			</h3>
			<Card.Root size="sm">
				<Card.Content class="grid gap-4">
					{#each a.windows as w (w.name + (w.scope ?? ''))}
						{@render meter(
							`${w.name}${w.scope ? ` · ${w.scope}` : ''}`,
							w.used * 100,
							w.resets_at ? `Resets in ${resetsIn(w.resets_at)}` : '',
							w.status === 'rejected'
						)}
					{/each}
				</Card.Content>
			</Card.Root>
		</div>
	{/each}

	{#if todo.length}
		<div>
			<h3 class="{heading} justify-between">
				<span>Plan</span><span class="num font-normal">{done}/{todo.length}</span>
			</h3>
			<Card.Root size="sm">
				<Card.Content>
					<ul class="grid gap-2">
						{#each todo as t (t.text)}
							<li class="flex gap-2 text-sm leading-5">
								<!-- The icon's box is as tall as the first line, so the two share a centre. -->
								<span class="grid h-5 shrink-0 place-items-center" aria-hidden="true">
									{#if t.status === 'done'}
										<CircleCheck class="size-4 text-success" />
									{:else if t.status === 'in_progress'}
										<CircleDot class="size-4 text-brand" />
									{:else}
										<Circle class="size-4 text-muted-foreground" />
									{/if}
								</span>
								<span class={t.status === 'done' ? 'text-muted-foreground line-through' : ''}
									>{t.text}</span
								>
								<span class="sr-only">{t.status.replace('_', ' ')}</span>
							</li>
						{/each}
					</ul>
				</Card.Content>
			</Card.Root>
		</div>
	{/if}
</section>
