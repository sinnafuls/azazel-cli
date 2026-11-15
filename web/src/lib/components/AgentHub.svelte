<script lang="ts">
	import type { Session } from '$lib/session/session.svelte';
	import StateDot from './StateDot.svelte';
	import { cost, duration, tokens } from '$lib/format';
	import { toasts } from '$lib/toasts.svelte';
	import Square from '@lucide/svelte/icons/square';

	let {
		session,
		canControl,
		active = null
	}: { session: Session; canControl: boolean; active?: string | null } = $props();

	const members = $derived(session.model.roster);
	const live = $derived(
		members.filter(
			(m) => m.state === 'running' || m.state === 'queued' || m.state === 'waiting for approval'
		).length
	);
	let confirming = $state<string | null>(null);

	async function stop(id: string) {
		try {
			await session.agentStop(id);
			toasts.push(`Stopped ${id}.`, 'success');
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		} finally {
			confirming = null;
		}
	}
	const stoppable = (s: string) =>
		s === 'running' || s === 'queued' || s === 'waiting for approval';
</script>

<section aria-label="Agents" data-agent-hub>
	<div class="text-muted-foreground mb-2 flex items-baseline justify-between text-xs">
		<span
			>{members.length
				? `${members.length} agent${members.length === 1 ? '' : 's'}`
				: 'No agents yet'}</span
		>
		{#if live}<span class="num">{live} active</span>{/if}
	</div>

	{#if members.length <= 1}
		<p class="text-muted-foreground text-[13px] leading-relaxed">
			When the lead hands work to sub-agents, they show up here with what each one is doing. You can
			open one to read its work, message it or stop it.
		</p>
	{/if}

	<ul class="space-y-1">
		{#each members as m (m.id)}
			<li style="margin-left: {Math.min(m.depth, 3) * 14}px">
				<div
					class="flex items-stretch gap-1 rounded-lg border {active === m.id
						? 'border-input bg-raised'
						: ''}"
				>
					<a
						class="hover:bg-raised flex min-h-14 min-w-0 flex-1 items-center gap-3 rounded-lg px-3 py-2 md:min-h-12"
						href={m.id === 'lead'
							? `/s/${encodeURIComponent(session.id)}`
							: `/s/${encodeURIComponent(session.id)}/agents/${encodeURIComponent(m.id)}`}
						aria-current={active === m.id ? 'page' : undefined}
					>
						<StateDot state={m.state} />
						<span class="min-w-0 flex-1">
							<span class="flex items-baseline gap-2">
								<span class="truncate font-mono text-[13px] font-medium">{m.id}</span>
								<span class="text-muted-foreground shrink-0 text-[11px]">{m.state}</span>
							</span>
							<span class="text-muted-foreground block truncate text-xs"
								>{m.activity || (m.id === 'lead' ? m.def : `from ${m.parent ?? 'lead'}`)}</span
							>
						</span>
						<span
							class="text-muted-foreground num hidden shrink-0 text-right text-[11px] min-[420px]:block"
						>
							<span class="block">{duration(m.elapsed_ms)}</span>
							<span class="block">{tokens(m.tokens)} · {cost(m.cost_usd)}</span>
						</span>
					</a>
					{#if canControl && m.id !== 'lead' && stoppable(m.state)}
						<button
							type="button"
							class="text-muted-foreground hover:text-destructive hover:bg-destructive/10 grid w-11 shrink-0 place-items-center rounded-r-lg"
							aria-label="Stop {m.id}"
							onclick={() => (confirming = m.id)}><Square class="size-3.5" /></button
						>
					{/if}
				</div>
				{#if confirming === m.id}
					<div
						class="bg-card mt-1 flex items-center gap-2 rounded-lg border p-2 text-[13px]"
						role="alertdialog"
						aria-label="Confirm stop"
					>
						<span class="min-w-0 flex-1">Stop {m.id} and the agents it started?</span>
						<button
							class="hover:bg-raised h-9 rounded-md border px-3"
							onclick={() => (confirming = null)}>Keep</button
						>
						<button
							class="bg-destructive text-destructive-foreground h-9 rounded-md px-3 font-medium"
							onclick={() => stop(m.id)}>Stop</button
						>
					</div>
				{/if}
			</li>
		{/each}
	</ul>
</section>
