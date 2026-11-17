<script lang="ts">
	import type { Session } from '$lib/session/session.svelte';
	import StateDot from './StateDot.svelte';
	import { cost, duration, tokens } from '$lib/format';
	import { toasts } from '$lib/toasts.svelte';
	import Square from '@lucide/svelte/icons/square';
	import { onMount } from 'svelte';

	let {
		session,
		canControl,
		active = null
	}: { session: Session; canControl: boolean; active?: string | null } = $props();

	const members = $derived(session.model.roster);
	const subs = $derived(members.filter((m) => m.id !== 'lead').length);
	const live = $derived(
		members.filter(
			(m) => m.state === 'running' || m.state === 'queued' || m.state === 'waiting for approval'
		).length
	);
	let confirming = $state<string | null>(null);

	// The roster reports elapsed time when it was sent. A running agent keeps counting from that
	// moment, so the number moves between frames instead of freezing.
	let now = $state(Date.now());
	onMount(() => {
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});
	const elapsed = (m: (typeof members)[number]) =>
		m.state === 'running' ? m.elapsed_ms + Math.max(0, now - session.model.rosterAt) : m.elapsed_ms;

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
	const href = (id: string) =>
		id === 'lead'
			? `/s/${encodeURIComponent(session.id)}`
			: `/s/${encodeURIComponent(session.id)}/agents/${encodeURIComponent(id)}`;
</script>

<section aria-label="Agents" data-agent-hub>
	{#if subs === 0}
		<div class="py-2">
			<p class="text-sm font-medium tracking-[-0.015em]">Working alone</p>
			<p class="text-muted-foreground mt-1 text-sm leading-relaxed">
				When the lead hands work to sub-agents they appear here, live. Open one to read its work,
				message it or stop it.
			</p>
		</div>
	{:else}
		<div class="mb-2 flex items-baseline justify-between">
			<span class="label">Agents <span class="text-faint meta ml-1">{members.length}</span></span>
			{#if live}<span class="tag tag-info num">{live} active</span>{/if}
		</div>
		<!-- One list with hairlines, not a card per agent: the tree is read down its left edge. -->
		<ul class="-mx-2 space-y-0.5">
			{#each members as m (m.id)}
				<li>
					<div class="flex items-stretch gap-1">
						<a
							class="row flex min-h-14 min-w-0 flex-1 gap-3 rounded-lg py-2 pr-2"
							style="padding-left: {8 + Math.min(m.depth, 3) * 14}px"
							href={href(m.id)}
							aria-current={active === m.id ? 'page' : undefined}
						>
							<StateDot state={m.state} class="mt-[5px]" />
							<span class="min-w-0 flex-1">
								<span class="flex items-baseline gap-2">
									<span class="truncate font-mono text-xs font-medium">{m.id}</span>
									<span class="text-muted-foreground shrink-0 text-xs">{m.state}</span>
									{#if elapsed(m) >= 1000}<span class="text-faint meta ml-auto shrink-0"
											>{duration(elapsed(m))}</span
										>{/if}
								</span>
								<span class="text-muted-foreground mt-0.5 block truncate text-xs"
									>{m.activity ||
										(m.id === 'lead' ? 'Lead agent' : `Started by ${m.parent ?? 'lead'}`)}</span
								>
								{#if m.tokens > 0}<span class="text-faint meta mt-0.5 block"
										>{tokens(m.tokens)} tok{m.cost_usd ? ` · ${cost(m.cost_usd)}` : ''}</span
									>{/if}
							</span>
						</a>
						{#if canControl && m.id !== 'lead' && stoppable(m.state)}
							<button
								type="button"
								class="btn btn-quiet btn-icon stop self-center"
								aria-label="Stop {m.id}"
								onclick={() => (confirming = m.id)}><Square class="size-3.5" /></button
							>
						{/if}
					</div>
					{#if confirming === m.id}
						<div
							class="surface mx-2 mt-1 mb-2 animate-enter rounded-lg p-3 text-sm"
							role="alertdialog"
							aria-label="Confirm stop"
						>
							<p>Stop {m.id} and the agents it started?</p>
							<div class="mt-3 flex gap-2">
								<button class="btn btn-secondary flex-1" onclick={() => (confirming = null)}
									>Keep</button
								>
								<button class="btn btn-destroy flex-1" onclick={() => stop(m.id)}>Stop</button>
							</div>
						</div>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>

<style>
	.row {
		transition: background-color var(--dur-fast) var(--ease-out);
	}
	.row[aria-current='page'] {
		background: var(--hover);
		box-shadow: inset 0 0 0 1px var(--border);
	}
	@media (hover: hover) {
		.row:hover {
			background: var(--hover);
		}
		.stop:hover {
			color: var(--destructive);
		}
	}
</style>
