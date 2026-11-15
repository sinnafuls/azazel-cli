<script lang="ts">
	import type { Session } from '$lib/session/session.svelte';
	import StateDot from './StateDot.svelte';
	import { cost, duration, tokens } from '$lib/format';
	import { toasts } from '$lib/toasts.svelte';
	import Square from '@lucide/svelte/icons/square';
	import Users from '@lucide/svelte/icons/users';
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
		<div class="flex flex-col items-center gap-3 px-2 py-10 text-center">
			<span
				class="bg-raised text-muted-foreground grid size-10 place-items-center rounded-xl border"
				><Users class="size-[18px]" /></span
			>
			<p class="text-sm font-medium">Working alone</p>
			<p class="text-muted-foreground max-w-[15rem] text-[13px] leading-relaxed">
				When the lead hands work to sub-agents they appear here, live. Open one to read its work,
				message it or stop it.
			</p>
		</div>
	{:else}
		<div class="mb-3 flex items-center justify-between">
			<span class="label">Agents · {members.length}</span>
			{#if live}<span class="text-info num text-xs">{live} active</span>{/if}
		</div>
		<ul class="space-y-1.5">
			{#each members as m (m.id)}
				<li style="margin-left: {Math.min(m.depth, 3) * 12}px">
					<div
						class="surface flex items-stretch overflow-hidden rounded-xl {active === m.id
							? 'border-border-strong bg-raised'
							: ''}"
					>
						<a
							class="hover:bg-raised/60 flex min-h-[3.75rem] min-w-0 flex-1 items-center gap-3 px-3.5 py-2.5"
							href={href(m.id)}
							aria-current={active === m.id ? 'page' : undefined}
						>
							<StateDot state={m.state} />
							<span class="min-w-0 flex-1">
								<span class="flex items-baseline gap-2">
									<span class="truncate font-mono text-[13px] font-medium">{m.id}</span>
									<span class="text-muted-foreground shrink-0 text-[11px]">{m.state}</span>
								</span>
								<span class="text-muted-foreground mt-0.5 block truncate text-xs"
									>{m.activity ||
										(m.id === 'lead' ? 'Lead agent' : `Started by ${m.parent ?? 'lead'}`)}</span
								>
							</span>
							<span class="text-faint num shrink-0 text-right font-mono text-[11px] leading-4">
								{#if elapsed(m) >= 1000}<span class="block">{duration(elapsed(m))}</span>{/if}
								{#if m.tokens > 0}<span class="block"
										>{tokens(m.tokens)} tok{m.cost_usd ? ` · ${cost(m.cost_usd)}` : ''}</span
									>{/if}
							</span>
						</a>
						{#if canControl && m.id !== 'lead' && stoppable(m.state)}
							<button
								type="button"
								class="text-muted-foreground hover:text-destructive hover:bg-destructive/10 grid w-12 shrink-0 place-items-center border-l"
								aria-label="Stop {m.id}"
								onclick={() => (confirming = m.id)}><Square class="size-3.5" /></button
							>
						{/if}
					</div>
					{#if confirming === m.id}
						<div
							class="float mt-1.5 flex animate-enter items-center gap-2 rounded-xl p-2.5 text-[13px]"
							role="alertdialog"
							aria-label="Confirm stop"
						>
							<span class="min-w-0 flex-1">Stop {m.id} and the agents it started?</span>
							<button
								class="hover:bg-raised h-9 rounded-lg border px-3"
								onclick={() => (confirming = null)}>Keep</button
							>
							<button
								class="bg-destructive text-destructive-foreground h-9 rounded-lg px-3 font-medium"
								onclick={() => stop(m.id)}>Stop</button
							>
						</div>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>
