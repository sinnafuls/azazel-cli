<script lang="ts">
	// The session's model, thinking level, mode and primary agent, each a picker in a sheet.
	import type { Session } from '$lib/session/session.svelte';
	import { MODES, THINKING_LEVELS } from '$lib/protocol';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import Sheet from './Sheet.svelte';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import Check from '@lucide/svelte/icons/check';
	import Cpu from '@lucide/svelte/icons/cpu';
	import Brain from '@lucide/svelte/icons/brain';
	import Shield from '@lucide/svelte/icons/shield';
	import Bot from '@lucide/svelte/icons/bot';
	import { tokens } from '$lib/format';
	import { modelLabel, pickableRoles, splitModel } from '$lib/models';

	let { session, canControl }: { session: Session; canControl: boolean } = $props();

	type Which = 'model' | 'thinking' | 'mode' | 'agent' | null;
	let which = $state<Which>(null);
	let query = $state('');
	let confirmBypass = $state(false);

	const st = $derived(session.state);
	// A lead that follows a role shows the role and the model it answers with.
	const label = $derived(modelLabel(st));
	const current = $derived(label.role ? `@${label.role}` : label.model);
	const model = $derived(session.models.find((m) => m.model === (st?.model ?? label.model)));
	// Always the whole `provider/id`: that is what set_model takes. The provider is dimmed.
	const shown = $derived(splitModel(label.model));
	const roles = $derived(pickableRoles(session.roles, query));
	// An unknown model (the list not read yet) offers every level rather than claiming it
	// cannot think; only a model the list says does not reason gets `off` alone.
	const levels = $derived(
		!model
			? [...THINKING_LEVELS]
			: model.reasoning
				? model.thinking_levels?.length
					? ['off', ...model.thinking_levels.filter((l) => l !== 'off')]
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
	// A `/model`, `/mode` or `/thinking` typed in the message box opens the picker here.
	$effect(() => {
		const w = ui.picker;
		if (!w) return;
		ui.picker = null;
		open(w);
	});
	function open(w: Which) {
		if (!canControl) return;
		query = '';
		which = w;
		if ((w === 'model' || w === 'thinking') && !session.models.length) {
			session.modelsError = null;
			void session.refreshModels();
		}
		if (w === 'model') void session.refreshRoles().catch(() => {});
	}
</script>

<div
	class="no-scrollbar flex items-center gap-1.5 overflow-x-auto px-3 [mask-image:linear-gradient(to_right,black_calc(100%-28px),transparent)] md:[mask-image:none] md:px-4 fine:pb-2"
	data-statusbar
>
	<button
		class="chip"
		disabled={!canControl}
		onclick={() => open('model')}
		aria-label="Model: {label.role ? `@${label.role} (${label.model})` : label.model}"
		title={label.role ? `@${label.role}: ${label.model}` : label.model}
	>
		<Cpu class="text-faint size-3.5" />
		<span class="text-foreground max-w-[15rem] truncate font-mono text-xs"
			>{#if label.role}<span class="text-accent-text">@{label.role}</span><span class="text-faint">
					·
				</span>{/if}<span class="text-faint">{shown.provider}</span>{shown.id}</span
		>
		{#if canControl}<ChevronDown class="text-faint size-3" />{/if}
	</button>
	<button
		class="chip"
		disabled={!canControl}
		onclick={() => open('thinking')}
		aria-label="Thinking: {st?.thinking ?? 'off'}"
	>
		<Brain class="text-faint size-3.5" />
		{st?.thinking ?? 'off'}
		{#if canControl}<ChevronDown class="text-faint size-3" />{/if}
	</button>
	<button
		class="chip {st?.mode === 'bypass' ? 'text-destructive font-medium' : ''}"
		disabled={!canControl}
		onclick={() => open('mode')}
		aria-label="Mode: {st?.mode ?? ''}"
	>
		<Shield class="text-faint size-3.5" />
		{st?.mode ?? ''}
		{#if canControl}<ChevronDown class="text-faint size-3" />{/if}
	</button>
	{#if st?.primary_agents && st.primary_agents.length > 1}
		<button
			class="chip"
			disabled={!canControl}
			onclick={() => open('agent')}
			aria-label="Agent: {st.agent}"
		>
			<Bot class="text-faint size-3.5" />
			{st.agent}
			{#if canControl}<ChevronDown class="text-faint size-3" />{/if}
		</button>
	{/if}
	{#if st && st.context_window}
		<span class="text-faint meta ml-auto hidden shrink-0 pl-2.5 sm:inline" title="Context in use"
			><span class="text-muted-foreground">{tokens(st.context_tokens)}</span> / {tokens(
				st.context_window
			)}</span
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
		class="field mb-2"
		placeholder="Search roles and models"
		aria-label="Search roles and models"
		bind:value={query}
	/>
	{#if roles.length}
		<p class="text-muted-foreground meta mt-1 mb-1">Roles</p>
		<ul class="-mx-2 mb-2 space-y-px" aria-label="Roles">
			{#each roles as r (r.role)}
				<li>
					<button
						class="opt"
						aria-current={`@${r.role}` === current ? 'true' : undefined}
						onclick={() => run(() => session.setModel(`@${r.role}`))}
					>
						<span class="min-w-0 flex-1 py-1">
							<span class="block truncate font-mono text-xs">@{r.role}</span>
							<span class="text-muted-foreground meta mt-0.5 block truncate">
								{r.resolved[0]}{r.source !== 'builtin' ? ` · ${r.source}` : ''}
							</span>
						</span>
						{#if `@${r.role}` === current}<Check class="text-accent-text size-4 shrink-0" />{/if}
					</button>
				</li>
			{/each}
		</ul>
		<p class="text-muted-foreground meta mb-1">Models</p>
	{/if}
	<ul class="-mx-2 space-y-px" aria-label="Models">
		{#each filtered as m (m.model)}
			<li>
				<button
					class="opt"
					aria-current={m.model === current ? 'true' : undefined}
					onclick={() => run(() => session.setModel(m.model))}
				>
					<span class="min-w-0 flex-1 py-1">
						<span class="block truncate font-mono text-xs">{m.model}</span>
						<span class="text-muted-foreground meta mt-0.5 block">
							{tokens(m.context_window)} ctx{m.reasoning ? ' · thinks' : ''}{m.vision
								? ' · images'
								: ''}{m.cost ? ` · $${m.cost.input}/${m.cost.output} per M` : ''}
						</span>
					</span>
					{#if m.model === current}<Check class="text-accent-text size-4 shrink-0" />{/if}
				</button>
			</li>
		{:else}
			<li class="text-muted-foreground px-2 py-6 text-sm">
				{session.models.length
					? 'No model matches.'
					: session.modelsError
						? `Could not load models: ${session.modelsError}`
						: 'Loading models…'}
			</li>
		{/each}
	</ul>
</Sheet>

<Sheet open={which === 'thinking'} title="Thinking level" onclose={() => (which = null)}>
	<ul class="-mx-2">
		{#each levels as l (l)}
			<li>
				<button
					class="opt justify-between"
					aria-current={l === st?.thinking ? 'true' : undefined}
					onclick={() => run(() => session.setThinking(l))}
				>
					{l}{#if l === st?.thinking}<Check class="text-accent-text size-4" />{/if}
				</button>
			</li>
		{/each}
	</ul>
	{#if model && levels.length === 1}<p class="text-muted-foreground mt-2 text-sm">
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
		<div class="space-y-4">
			<p class="text-sm leading-relaxed">
				Bypass lifts every guard except deny rules and the push guard. Agents will edit files and
				run commands without asking.
			</p>
			<div class="flex gap-2">
				<button class="btn btn-secondary flex-1" onclick={() => (confirmBypass = false)}
					>Cancel</button
				>
				<button class="btn btn-destroy flex-1" onclick={() => run(() => session.setMode('bypass'))}
					>Use bypass</button
				>
			</div>
		</div>
	{:else}
		<ul class="-mx-2">
			{#each MODES.filter((m) => m !== 'bypass' || app.me?.allow_bypass) as m (m)}
				<li>
					<button
						class="opt justify-between"
						aria-current={m === st?.mode ? 'true' : undefined}
						onclick={() =>
							m === 'bypass' ? (confirmBypass = true) : run(() => session.setMode(m))}
					>
						<span class={m === 'bypass' ? 'text-destructive' : ''}>{m}</span
						>{#if m === st?.mode}<Check class="text-accent-text size-4" />{/if}
					</button>
				</li>
			{/each}
		</ul>
	{/if}
</Sheet>

<Sheet open={which === 'agent'} title="Primary agent" onclose={() => (which = null)}>
	<ul class="-mx-2">
		{#each st?.primary_agents ?? [] as a (a.name)}
			<li>
				<button
					class="opt justify-between"
					aria-current={a.name === st?.agent ? 'true' : undefined}
					onclick={() => run(() => session.setAgent(a.name))}
				>
					<span class="min-w-0 py-1"
						><span class="block text-sm font-medium">{a.name}</span><span
							class="text-muted-foreground mt-0.5 block text-xs leading-snug">{a.description}</span
						></span
					>
					{#if a.name === st?.agent}<Check class="text-accent-text size-4 shrink-0" />{/if}
				</button>
			</li>
		{/each}
	</ul>
</Sheet>
