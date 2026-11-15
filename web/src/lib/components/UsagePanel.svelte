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
		bad ? 'bg-destructive' : p > 85 ? 'bg-destructive' : p > 65 ? 'bg-primary' : 'bg-info';
</script>

<section aria-label="Usage" class="space-y-6" data-usage>
	{#if st}
		<div>
			<div class="mb-2 flex items-baseline justify-between">
				<span class="label">Context</span>
				<span class="num font-mono text-xs"
					>{tokens(st.context_tokens)}<span class="text-faint">
						/ {tokens(st.context_window)}</span
					></span
				>
			</div>
			<div
				class="bg-inset h-1.5 overflow-hidden rounded-full border"
				role="meter"
				aria-valuenow={Math.round(pct)}
				aria-valuemin="0"
				aria-valuemax="100"
				aria-label="Context used"
			>
				<div class="h-full rounded-full {bar(pct)}" style="width: {Math.max(pct, 1.5)}%"></div>
			</div>
		</div>

		<div>
			<div class="label mb-2">This session</div>
			<dl class="surface grid grid-cols-3 divide-x overflow-hidden rounded-xl text-center">
				{#each [['Cost', cost(st.usage.cost_usd)], ['Input', tokens(st.usage.input ?? 0)], ['Output', tokens(st.usage.output ?? 0)]] as [k, v] (k)}
					<div class="px-2 py-3">
						<dd class="num font-mono text-[15px] font-medium">{v}</dd>
						<dt class="text-muted-foreground mt-0.5 text-[11px]">{k}</dt>
					</div>
				{/each}
			</dl>
			<p class="text-faint num mt-2 text-[11.5px]">
				{st.requests} request{st.requests === 1 ? '' : 's'} · {tokens(st.usage.cache_read ?? 0)} read
				from cache
			</p>
		</div>
	{/if}

	{#if session.usage.length}
		<div>
			<div class="label mb-2">Provider allowance</div>
			<ul class="space-y-4">
				{#each session.usage as a (a.provider + a.account)}
					<li>
						<div class="mb-1.5 text-[13px] font-medium">
							{a.provider} <span class="text-muted-foreground font-normal">· {a.account}</span>
						</div>
						{#each a.windows as w (w.name + (w.scope ?? ''))}
							<div class="mb-2">
								<div class="text-muted-foreground flex justify-between text-[11.5px]">
									<span class="font-mono">{w.name}{w.scope ? ` · ${w.scope}` : ''}</span>
									<span class="num"
										>{Math.round(w.used * 100)}%{w.resets_at
											? ` · resets in ${resetsIn(w.resets_at)}`
											: ''}</span
									>
								</div>
								<div
									class="bg-inset mt-1 h-1 overflow-hidden rounded-full border"
									role="meter"
									aria-valuenow={Math.round(w.used * 100)}
									aria-valuemin="0"
									aria-valuemax="100"
									aria-label="{w.name} used"
								>
									<div
										class="h-full rounded-full {bar(w.used * 100, w.status === 'rejected')}"
										style="width: {Math.max(w.used * 100, 2)}%"
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
			<div class="label mb-2 flex justify-between">
				<span>Plan</span><span class="num normal-case tracking-normal">{done}/{todo.length}</span>
			</div>
			<ul class="space-y-2">
				{#each todo as t (t.text)}
					<li class="flex gap-2.5 text-[13px] leading-snug">
						<span
							class="mt-0.5 grid size-4 shrink-0 place-items-center rounded-full border {t.status ===
							'done'
								? 'border-success/50 bg-success/15 text-success'
								: t.status === 'in_progress'
									? 'border-primary'
									: 'border-input'}"
							aria-hidden="true"
							>{#if t.status === 'done'}<Check
									class="size-2.5"
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
