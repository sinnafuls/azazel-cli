<script lang="ts">
	// The lead and the agents it started, as a tree read down its left edge: every one an Item that
	// links to its transcript, with a Stop button beside it while it works.
	import type { Session } from '$lib/session/session.svelte';
	import StateDot from './StateDot.svelte';
	import Tip from './Tip.svelte';
	import * as Item from '$lib/components/ui/item';
	import * as Empty from '$lib/components/ui/empty';
	import * as AlertDialog from '$lib/components/ui/alert-dialog';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
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
		<Empty.Root class="items-start gap-1 p-0 text-left">
			<Empty.Title>Working alone</Empty.Title>
			<Empty.Description>
				When the lead hands work to sub-agents they appear here, live. Open one to read its work,
				message it or stop it.
			</Empty.Description>
		</Empty.Root>
	{:else}
		<div class="mb-2 flex h-5 items-center justify-between">
			<h3 class="text-xs font-medium text-muted-foreground">
				Agents <span class="num ml-1 font-normal">{members.length}</span>
			</h3>
			{#if live}<Badge variant="secondary" class="num">{live} active</Badge>{/if}
		</div>
		<ul class="-mx-2.5 grid gap-0.5">
			{#each members as m (m.id)}
				<li
					class="flex items-center gap-1"
					style="padding-left: {Math.min(m.depth, 3) * 14}px"
					data-agent={m.id}
				>
					<Item.Root
						size="xs"
						class="min-w-0 flex-1 flex-nowrap hover:bg-muted aria-[current=page]:bg-muted"
					>
						{#snippet child({ props })}
							<a {...props} href={href(m.id)} aria-current={active === m.id ? 'page' : undefined}>
								<Item.Media variant="icon"><StateDot state={m.state} /></Item.Media>
								<Item.Content class="min-w-0">
									<Item.Title class="w-full">
										<span class="truncate font-mono text-xs">{m.id}</span>
										<span class="shrink-0 text-xs font-normal text-muted-foreground">{m.state}</span
										>
										{#if elapsed(m) >= 1000}
											<span class="num ml-auto shrink-0 text-xs font-normal text-muted-foreground"
												>{duration(elapsed(m))}</span
											>
										{/if}
									</Item.Title>
									<Item.Description class="line-clamp-1 break-all"
										>{m.activity ||
											(m.id === 'lead'
												? 'Lead agent'
												: `Started by ${m.parent ?? 'lead'}`)}</Item.Description
									>
									{#if m.tokens > 0}
										<span class="num text-xs text-muted-foreground"
											>{tokens(m.tokens)} tok{m.cost_usd ? ` · ${cost(m.cost_usd)}` : ''}</span
										>
									{/if}
								</Item.Content>
							</a>
						{/snippet}
					</Item.Root>
					{#if canControl && m.id !== 'lead' && stoppable(m.state)}
						<Tip
							text="Stop {m.id}"
							side="left"
							class="mr-1 shrink-0 hover:text-destructive"
							aria-label="Stop {m.id}"
							onclick={() => (confirming = m.id)}><Square class="size-3 fill-current" /></Tip
						>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>

<AlertDialog.Root open={confirming !== null} onOpenChange={(v) => !v && (confirming = null)}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>Stop {confirming}?</AlertDialog.Title>
			<AlertDialog.Description>
				It stops now, and so do the agents it started. What it did so far stays in its transcript.
			</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Keep</AlertDialog.Cancel>
			<Button variant="destructive" onclick={() => confirming && stop(confirming)}>Stop</Button>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
