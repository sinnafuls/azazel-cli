<script lang="ts">
	// The session's model, profile, thinking level, mode and primary agent: ghost Buttons, each
	// opening a Popover (the model: a Command list with a search) or a DropdownMenu anchored to it.
	// It sits inside the composer, so what the next message will be sent with is beside the message.
	import { Button } from '$lib/components/ui/button';
	import * as Popover from '$lib/components/ui/popover';
	import * as Command from '$lib/components/ui/command';
	import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
	import * as AlertDialog from '$lib/components/ui/alert-dialog';
	import * as Tooltip from '$lib/components/ui/tooltip';
	import { badgeVariants } from '$lib/components/ui/badge';
	import { cn } from '$lib/utils';
	import type { Session } from '$lib/session/session.svelte';
	import { MODES, THINKING_LEVELS } from '$lib/protocol';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import Brain from '@lucide/svelte/icons/brain';
	import Shield from '@lucide/svelte/icons/shield';
	import Bot from '@lucide/svelte/icons/bot';
	import Hourglass from '@lucide/svelte/icons/hourglass';
	import { cacheLeft, tokens } from '$lib/format';
	import { modelLabel, pickableRoles, shortModel, splitModel } from '$lib/models';
	import ProfileChip from './ProfileChip.svelte';

	let { session, canControl }: { session: Session; canControl: boolean } = $props();

	type Which = 'model' | 'profile' | 'thinking' | 'mode' | 'agent' | null;
	let which = $state<Which>(null);
	let query = $state('');
	let confirmBypass = $state(false);

	const st = $derived(session.state);
	// A lead that follows a role shows the role and the model it answers with.
	const label = $derived(modelLabel(st));
	const current = $derived(label.role ? `@${label.role}` : label.model);
	const model = $derived(
		(session.models ?? []).find((m) => m.model === (st?.model ?? label.model))
	);
	// Always the whole `provider/id`: that is what set_model takes. The provider is dimmed.
	const shown = $derived(splitModel(label.model));
	// The chip names the model short: its catalogue name, else the id without provider, vendor
	// prefix and version. The whole `provider/id` is in the popover, never cut off.
	const shortName = $derived(model?.name || shortModel(shown.id));
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
	const modes = $derived(MODES.filter((m) => m !== 'bypass' || app.me?.allow_bypass));

	// The prompt cache's clock: `get_state` gives the seconds left when it was made, and the
	// page counts down from when it arrived. It ticks only while there is time left.
	let cacheEnd = $state<number | null>(null);
	let now = $state(Date.now());
	$effect(() => {
		const left = st?.cache_left_secs;
		cacheEnd = left === undefined || left === null ? null : Date.now() + left * 1000;
		now = Date.now();
	});
	$effect(() => {
		if (cacheEnd === null || cacheEnd <= Date.now()) return;
		const timer = setInterval(() => {
			now = Date.now();
			if (cacheEnd !== null && now >= cacheEnd) clearInterval(timer);
		}, 1000);
		return () => clearInterval(timer);
	});
	const cacheText = $derived(cacheEnd === null ? null : cacheLeft((cacheEnd - now) / 1000));

	async function run(fn: () => Promise<void>) {
		try {
			await fn();
			which = null;
			confirmBypass = false;
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		}
	}
	// A `/model`, `/profile`, `/mode` or `/thinking` typed in the message box opens the picker here.
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
	// Binds one picker's open state to `which`; a click outside or Escape closes it.
	const isOpen = (w: Exclude<Which, null>) => which === w;
	const change = (w: Exclude<Which, null>) => (v: boolean) => {
		if (v) open(w);
		else if (which === w) which = null;
	};

	// Every chip is the same Button: ghost, small, in the quiet text colour.
	const chip = 'max-w-full min-w-0 px-2 font-normal text-muted-foreground';
</script>

<div class="flex min-w-0 flex-wrap items-center gap-0.5" data-statusbar>
	<Popover.Root bind:open={() => isOpen('model'), change('model')}>
		<Popover.Trigger>
			{#snippet child({ props })}
				<Button
					{...props}
					variant="ghost"
					size="sm"
					class={chip}
					disabled={!canControl}
					aria-label="Model: {label.role ? `@${label.role} (${label.model})` : label.model}"
				>
					<span class="truncate">{label.role ? `@${label.role}` : shortName}</span>
					{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
				</Button>
			{/snippet}
		</Popover.Trigger>
		<Popover.Content side="top" align="start" class="w-[min(24rem,calc(100vw-1.5rem))] gap-0 p-0">
			<p class="border-b px-3 py-2 text-xs break-all text-muted-foreground" data-current-model>
				<!-- The space is after the block: white space before the end of a block is dropped. -->
				{#if label.role}<span class="text-foreground">@{label.role}</span> answers with{/if}
				<span class="font-mono"
					>{shown.provider}<span class="text-foreground">{shown.id}</span></span
				>
			</p>
			<Command.Root shouldFilter={false} class="rounded-none! bg-transparent">
				<Command.Input
					placeholder="Search roles and models…"
					aria-label="Search roles and models"
					bind:value={query}
				/>
				<Command.List class="max-h-[min(22rem,50dvh)]" aria-label="Roles and models">
					{#if roles.length}
						<Command.Group heading="Roles">
							{#each roles as r (r.role)}
								<Command.Item
									value="@{r.role}"
									data-checked={`@${r.role}` === current}
									onSelect={() => run(() => session.setModel(`@${r.role}`))}
								>
									<span class="grid min-w-0 flex-1 gap-0.5">
										<span class="truncate font-mono text-xs">@{r.role}</span>
										<span class="truncate text-xs text-muted-foreground">
											{r.resolved[0]}{r.source !== 'builtin' ? ` · ${r.source}` : ''}
										</span>
									</span>
								</Command.Item>
							{/each}
						</Command.Group>
					{/if}
					{#if filtered.length}
						<Command.Group heading="Models">
							{#each filtered as m (m.model)}
								<Command.Item
									value={m.model}
									data-checked={m.model === current}
									onSelect={() => run(() => session.setModel(m.model))}
								>
									<span class="grid min-w-0 flex-1 gap-0.5">
										<span class="truncate font-mono text-xs">{m.model}</span>
										<span class="truncate text-xs text-muted-foreground">
											{tokens(m.context_window)} ctx{m.reasoning ? ' · thinks' : ''}{m.vision
												? ' · images'
												: ''}{m.cost ? ` · $${m.cost.input}/${m.cost.output} per M` : ''}
										</span>
									</span>
								</Command.Item>
							{/each}
						</Command.Group>
					{:else}
						<!-- Not Command.Empty: the list is filtered here, so the component cannot tell. -->
						<p class="px-3 py-6 text-center text-sm text-muted-foreground" role="status">
							{session.models.length
								? 'No model matches.'
								: session.modelsError
									? `Could not load models: ${session.modelsError}`
									: 'Loading models…'}
						</p>
					{/if}
				</Command.List>
			</Command.Root>
		</Popover.Content>
	</Popover.Root>

	<ProfileChip
		{session}
		{canControl}
		class={chip}
		bind:open={() => isOpen('profile'), change('profile')}
		onpick={(name) => run(() => session.setProfile(name))}
	/>

	<DropdownMenu.Root bind:open={() => isOpen('thinking'), change('thinking')}>
		<DropdownMenu.Trigger>
			{#snippet child({ props })}
				<Button
					{...props}
					variant="ghost"
					size="sm"
					class={chip}
					disabled={!canControl}
					aria-label="Thinking: {st?.thinking ?? 'off'}"
				>
					<Brain />
					<span class="truncate">{st?.thinking ?? 'off'}</span>
					{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
				</Button>
			{/snippet}
		</DropdownMenu.Trigger>
		<DropdownMenu.Content side="top" align="start" class="min-w-40">
			<DropdownMenu.Label>Thinking level</DropdownMenu.Label>
			<DropdownMenu.RadioGroup
				value={st?.thinking}
				onValueChange={(v) => run(() => session.setThinking(v))}
			>
				{#each levels as l (l)}
					<DropdownMenu.RadioItem value={l}>{l}</DropdownMenu.RadioItem>
				{/each}
			</DropdownMenu.RadioGroup>
			{#if model && levels.length === 1}
				<DropdownMenu.Separator />
				<DropdownMenu.Label class="font-normal">This model does not think.</DropdownMenu.Label>
			{/if}
		</DropdownMenu.Content>
	</DropdownMenu.Root>

	<DropdownMenu.Root bind:open={() => isOpen('mode'), change('mode')}>
		<DropdownMenu.Trigger>
			{#snippet child({ props })}
				<Button
					{...props}
					variant="ghost"
					size="sm"
					class={cn(chip, st?.mode === 'bypass' && 'text-destructive')}
					disabled={!canControl}
					aria-label="Mode: {st?.mode ?? ''}"
				>
					<Shield />
					<span class="truncate">{st?.mode ?? ''}</span>
					{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
				</Button>
			{/snippet}
		</DropdownMenu.Trigger>
		<DropdownMenu.Content side="top" align="start" class="min-w-48">
			<DropdownMenu.Label>Permission mode</DropdownMenu.Label>
			<DropdownMenu.RadioGroup
				value={st?.mode}
				onValueChange={(m) =>
					m === 'bypass' ? (confirmBypass = true) : run(() => session.setMode(m))}
			>
				{#each modes as m (m)}
					<DropdownMenu.RadioItem value={m} class={m === 'bypass' ? 'text-destructive' : ''}>
						{m}
					</DropdownMenu.RadioItem>
				{/each}
			</DropdownMenu.RadioGroup>
		</DropdownMenu.Content>
	</DropdownMenu.Root>

	{#if st?.primary_agents && st.primary_agents.length > 1}
		<DropdownMenu.Root bind:open={() => isOpen('agent'), change('agent')}>
			<DropdownMenu.Trigger>
				{#snippet child({ props })}
					<Button
						{...props}
						variant="ghost"
						size="sm"
						class={chip}
						disabled={!canControl}
						aria-label="Agent: {st.agent}"
					>
						<Bot />
						<span class="truncate">{st.agent}</span>
						{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
					</Button>
				{/snippet}
			</DropdownMenu.Trigger>
			<DropdownMenu.Content side="top" align="start" class="w-96 max-w-[calc(100vw-1.5rem)]">
				<DropdownMenu.Label>Primary agent</DropdownMenu.Label>
				<DropdownMenu.RadioGroup
					value={st.agent}
					onValueChange={(a) => run(() => session.setAgent(a))}
				>
					{#each st.primary_agents as a (a.name)}
						<DropdownMenu.RadioItem value={a.name}>
							<span class="grid min-w-0 gap-0.5">
								<span class="font-medium">{a.name}</span>
								<!-- Two lines for every agent, so the rows of the list are one height. -->
								<span class="line-clamp-2 min-h-8 text-xs text-muted-foreground"
									>{a.description}</span
								>
							</span>
						</DropdownMenu.RadioItem>
					{/each}
				</DropdownMenu.RadioGroup>
			</DropdownMenu.Content>
		</DropdownMenu.Root>
	{/if}
	{#if cacheText !== null && !session.busy}
		<Tooltip.Provider delayDuration={300}>
			<Tooltip.Root>
				<!-- The trigger is the Badge itself, as a button, so that the keyboard reaches its tooltip. -->
				<Tooltip.Trigger
					class={cn(
						badgeVariants({ variant: 'outline' }),
						'ml-1 cursor-default font-mono font-normal',
						cacheText === 'cold' ? 'border-warning/40 text-warning' : 'text-muted-foreground'
					)}
					aria-label="Prompt cache: {cacheText}"
					data-cache-clock
				>
					<Hourglass />
					{cacheText}
				</Tooltip.Trigger>
				<Tooltip.Content side="top" class="max-w-64">
					{cacheText === 'cold'
						? 'The prompt cache has run out: the next request sends the conversation anew'
						: 'Time until the prompt cache runs out unless a request reads it'}
				</Tooltip.Content>
			</Tooltip.Root>
		</Tooltip.Provider>
	{/if}
</div>

<AlertDialog.Root open={confirmBypass} onOpenChange={(v) => !v && (confirmBypass = false)}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>Use bypass?</AlertDialog.Title>
			<AlertDialog.Description>
				For the lead and every sub-agent. Bypass lifts every guard except deny rules and the push
				guard. Agents will edit files and run commands without asking.
			</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
			<Button variant="destructive" onclick={() => run(() => session.setMode('bypass'))}
				>Use bypass</Button
			>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
