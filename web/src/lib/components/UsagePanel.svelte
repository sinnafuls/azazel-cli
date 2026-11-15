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
</script>

<section aria-label="Usage" class="space-y-5" data-usage>
	{#if st}
		<div>
			<div class="mb-1.5 flex items-baseline justify-between text-xs">
				<span class="text-muted-foreground">Context</span>
				<span class="num"
					>{tokens(st.context_tokens)}
					<span class="text-muted-foreground">of {tokens(st.context_window)}</span></span
				>
			</div>
			<div
				class="bg-raised h-1.5 overflow-hidden rounded-full"
				role="meter"
				aria-valuenow={Math.round(pct)}
				aria-valuemin="0"
				aria-valuemax="100"
				aria-label="Context used"
			>
				<div
					class="h-full rounded-full {pct > 85
						? 'bg-destructive'
						: pct > 65
							? 'bg-primary'
							: 'bg-info'}"
					style="width: {Math.max(pct, 1)}%"
				></div>
			</div>
		</div>

		<dl class="grid grid-cols-3 gap-px overflow-hidden rounded-lg border text-center">
			{#each [['Cost', cost(st.usage.cost_usd)], ['In', tokens(st.usage.input ?? 0)], ['Out', tokens(st.usage.output ?? 0)]] as [k, v] (k)}
				<div class="bg-card px-2 py-2.5">
					<dt class="text-muted-foreground text-[11px]">{k}</dt>
					<dd class="num text-[15px] font-medium">{v}</dd>
				</div>
			{/each}
		</dl>
		<p class="text-muted-foreground num -mt-3 text-[11px]">
			{st.requests} request{st.requests === 1 ? '' : 's'} · {tokens(st.usage.cache_read ?? 0)} read from
			cache
		</p>
	{/if}

	{#if session.usage.length}
		<div>
			<h3 class="text-muted-foreground mb-2 text-xs">Provider allowance</h3>
			<ul class="space-y-3">
				{#each session.usage as a (a.provider + a.account)}
					<li>
						<div class="mb-1 text-[13px]">
							{a.provider} <span class="text-muted-foreground">{a.account}</span>
						</div>
						{#each a.windows as w (w.name + (w.scope ?? ''))}
							<div class="mb-1.5">
								<div class="text-muted-foreground flex justify-between text-[11px]">
									<span>{w.name}{w.scope ? ` · ${w.scope}` : ''}</span>
									<span class="num"
										>{Math.round(w.used * 100)}%{w.resets_at
											? ` · resets in ${resetsIn(w.resets_at)}`
											: ''}</span
									>
								</div>
								<div
									class="bg-raised mt-0.5 h-1 overflow-hidden rounded-full"
									role="meter"
									aria-valuenow={Math.round(w.used * 100)}
									aria-valuemin="0"
									aria-valuemax="100"
									aria-label="{w.name} used"
								>
									<div
										class="h-full rounded-full {w.status === 'rejected'
											? 'bg-destructive'
											: w.used > 0.75
												? 'bg-primary'
												: 'bg-info'}"
										style="width: {Math.max(w.used * 100, 1)}%"
									></div>
								</div>
							</div>
						{/each}
					</li>
				{/each}
			</ul>
		</div>
	{/if}

	{#if todo.length}
		<div>
			<h3 class="text-muted-foreground mb-2 flex justify-between text-xs">
				<span>Plan</span><span class="num">{done}/{todo.length}</span>
			</h3>
			<ul class="space-y-1.5">
				{#each todo as t (t.text)}
					<li class="flex gap-2.5 text-[13px] leading-snug">
						<span
							class="mt-0.5 grid size-4 shrink-0 place-items-center rounded-sm border {t.status ===
							'done'
								? 'border-success bg-success/20 text-success'
								: t.status === 'in_progress'
									? 'border-primary'
									: 'border-input'}"
							aria-hidden="true"
							>{#if t.status === 'done'}<Check
									class="size-3"
								/>{:else if t.status === 'in_progress'}<span
									class="bg-primary size-1.5 rounded-full"
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
