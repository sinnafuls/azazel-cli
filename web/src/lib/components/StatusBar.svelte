<script lang="ts">
	// The session's model, thinking level, mode and primary agent, each a picker in a sheet.
	import type { Session } from '$lib/session/session.svelte';
	import { MODES, THINKING_LEVELS } from '$lib/protocol';
	import { app } from '$lib/app.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import Sheet from './Sheet.svelte';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import Check from '@lucide/svelte/icons/check';
	import { tokens } from '$lib/format';

	let { session, canControl }: { session: Session; canControl: boolean } = $props();

	type Which = 'model' | 'thinking' | 'mode' | 'agent' | null;
	let which = $state<Which>(null);
	let query = $state('');
	let confirmBypass = $state(false);

	const st = $derived(session.state);
	const current = $derived(st?.chain?.[0] ?? st?.model ?? '…');
	const model = $derived(session.models.find((m) => m.model === (st?.model ?? current)));
	const levels = $derived(
		model && model.reasoning
			? model.thinking_levels?.length
				? ['off', ...model.thinking_levels]
				: [...THINKING_LEVELS]
			: ['off']
	);
	const filtered = $derived(
		session.models
			.filter(
				(m) => m.usable && `${m.model} ${m.name ?? ''}`.toLowerCase().includes(query.toLowerCase())
			)
			.slice(0, 80)
	);

	async function run(fn: () => Promise<void>) {
		try {
			await fn();
			which = null;
			confirmBypass = false;
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		}
	}
	function open(w: Which) {
		if (!canControl) return;
		query = '';
		which = w;
	}

	const chip =
		'hover:bg-raised enabled:active:bg-raised flex min-h-11 shrink-0 items-center gap-1 rounded-md px-2 text-xs md:min-h-7 disabled:cursor-default';
</script>

<div class="no-scrollbar flex items-center gap-0.5 overflow-x-auto" data-statusbar>
	<button
		class="{chip} font-mono"
		disabled={!canControl}
		onclick={() => open('model')}
		aria-label="Model: {current}"
	>
		<span class="max-w-[11rem] truncate">{current.replace(/^[^/]+\//, '')}</span>
		{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
	</button>
	<button
		class={chip}
		disabled={!canControl}
		onclick={() => open('thinking')}
		aria-label="Thinking: {st?.thinking ?? 'off'}"
	>
		<span class="text-muted-foreground">think</span>
		{st?.thinking ?? 'off'}
		{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
	</button>
	<button
		class="{chip} {st?.mode === 'bypass' ? 'text-destructive' : ''}"
		disabled={!canControl}
		onclick={() => open('mode')}
		aria-label="Mode: {st?.mode ?? ''}"
	>
		{st?.mode ?? ''}
		{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
	</button>
	{#if st?.primary_agents && st.primary_agents.length > 1}
		<button
			class={chip}
			disabled={!canControl}
			onclick={() => open('agent')}
			aria-label="Agent: {st.agent}"
		>
			<span class="text-muted-foreground">agent</span>
			{st.agent}
			{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
		</button>
	{/if}
	{#if st && st.context_window}
		<span
			class="text-muted-foreground num ml-1 hidden shrink-0 px-2 text-xs sm:inline"
			title="Context in use">{tokens(st.context_tokens)} / {tokens(st.context_window)}</span
		>
	{/if}
</div>

<Sheet
	open={which === 'model'}
	title="Model"
	subtitle="Applies from the next request"
	onclose={() => (which = null)}
>
	<input
		class="bg-background focus-visible:ring-ring mb-2 h-11 w-full rounded-md border px-3 text-sm outline-none focus-visible:ring-2 md:h-9"
		placeholder="Search models"
		aria-label="Search models"
		bind:value={query}
	/>
	<ul class="-mx-1">
		{#each filtered as m (m.model)}
			<li>
				<button
					class="hover:bg-raised flex min-h-11 w-full items-center gap-2 rounded-md px-2 text-left md:min-h-9"
					onclick={() => run(() => session.setModel(m.model))}
				>
					<span class="min-w-0 flex-1">
						<span class="block truncate font-mono text-[13px]">{m.model}</span>
						<span class="text-muted-foreground num block text-[11px]">
							{tokens(m.context_window)} ctx{m.reasoning ? ' · thinks' : ''}{m.vision
								? ' · images'
								: ''}{m.cost ? ` · $${m.cost.input}/${m.cost.output} per M` : ''}
						</span>
					</span>
					{#if m.model === current}<Check class="text-primary size-4" />{/if}
				</button>
			</li>
		{:else}
			<li class="text-muted-foreground py-6 text-center text-sm">
				{session.models.length ? 'No model matches.' : 'Loading models…'}
			</li>
		{/each}
	</ul>
</Sheet>

<Sheet open={which === 'thinking'} title="Thinking level" onclose={() => (which = null)}>
	<ul class="-mx-1">
		{#each levels as l (l)}
			<li>
				<button
					class="hover:bg-raised flex min-h-11 w-full items-center justify-between rounded-md px-2 text-sm md:min-h-9"
					onclick={() => run(() => session.setThinking(l))}
				>
					{l}{#if l === st?.thinking}<Check class="text-primary size-4" />{/if}
				</button>
			</li>
		{/each}
	</ul>
	{#if levels.length === 1}<p class="text-muted-foreground mt-2 text-xs">
			This model does not think.
		</p>{/if}
</Sheet>

<Sheet
	open={which === 'mode'}
	title="Permission mode"
	subtitle="For the lead and every sub-agent"
	onclose={() => ((which = null), (confirmBypass = false))}
>
	{#if confirmBypass}
		<div class="space-y-3 py-1">
			<p class="text-sm">
				Bypass lifts every guard except deny rules and the push guard. Agents will edit files and
				run commands without asking.
			</p>
			<div class="flex gap-2">
				<button class="h-11 flex-1 rounded-lg border md:h-9" onclick={() => (confirmBypass = false)}
					>Cancel</button
				>
				<button
					class="bg-destructive text-destructive-foreground h-11 flex-1 rounded-lg font-medium md:h-9"
					onclick={() => run(() => session.setMode('bypass'))}>Use bypass</button
				>
			</div>
		</div>
	{:else}
		<ul class="-mx-1">
			{#each MODES.filter((m) => m !== 'bypass' || app.me?.allow_bypass) as m (m)}
				<li>
					<button
						class="hover:bg-raised flex min-h-11 w-full items-center justify-between rounded-md px-2 text-sm md:min-h-9"
						onclick={() =>
							m === 'bypass' ? (confirmBypass = true) : run(() => session.setMode(m))}
					>
						<span class={m === 'bypass' ? 'text-destructive' : ''}>{m}</span
						>{#if m === st?.mode}<Check class="text-primary size-4" />{/if}
					</button>
				</li>
			{/each}
		</ul>
	{/if}
</Sheet>

<Sheet open={which === 'agent'} title="Primary agent" onclose={() => (which = null)}>
	<ul class="-mx-1">
		{#each st?.primary_agents ?? [] as a (a.name)}
			<li>
				<button
					class="hover:bg-raised flex min-h-11 w-full items-center justify-between gap-3 rounded-md px-2 text-left md:min-h-9"
					onclick={() => run(() => session.setAgent(a.name))}
				>
					<span
						><span class="block text-sm">{a.name}</span><span
							class="text-muted-foreground block text-xs">{a.description}</span
						></span
					>
					{#if a.name === st?.agent}<Check class="text-primary size-4 shrink-0" />{/if}
				</button>
			</li>
		{/each}
	</ul>
</Sheet>
