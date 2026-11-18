<script lang="ts">
	// The settings window: a list of panes on the left and the open pane beside it. On a phone it
	// fills the screen and the list is its first page. A native modal <dialog>: the focus stays
	// inside, Escape closes it and gives the focus back.
	import { tick, type Component } from 'svelte';
	import { ui, PANES, type Pane } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import General from './General.svelte';
	import Models from './Models.svelte';
	import Permissions from './Permissions.svelte';
	import UsagePane from './UsagePane.svelte';
	import ContextPane from './ContextPane.svelte';
	import AllSettings from './AllSettings.svelte';
	import Devices from './Devices.svelte';
	import Shortcuts from './Shortcuts.svelte';
	import About from './About.svelte';
	import X from '@lucide/svelte/icons/x';
	import ChevronLeft from '@lucide/svelte/icons/chevron-left';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import SlidersHorizontal from '@lucide/svelte/icons/sliders-horizontal';
	import Cpu from '@lucide/svelte/icons/cpu';
	import Shield from '@lucide/svelte/icons/shield';
	import Gauge from '@lucide/svelte/icons/gauge';
	import Layers from '@lucide/svelte/icons/layers';
	import Settings2 from '@lucide/svelte/icons/settings-2';
	import Smartphone from '@lucide/svelte/icons/smartphone';
	import Keyboard from '@lucide/svelte/icons/keyboard';
	import Info from '@lucide/svelte/icons/info';

	const VIEW: Record<Pane, { icon: Component<{ class?: string }>; view: Component }> = {
		general: { icon: SlidersHorizontal, view: General },
		models: { icon: Cpu, view: Models },
		permissions: { icon: Shield, view: Permissions },
		usage: { icon: Gauge, view: UsagePane },
		context: { icon: Layers, view: ContextPane },
		all: { icon: Settings2, view: AllSettings },
		devices: { icon: Smartphone, view: Devices },
		shortcuts: { icon: Keyboard, view: Shortcuts },
		about: { icon: Info, view: About }
	};

	let el = $state<HTMLDialogElement>();
	let nav = $state<HTMLDivElement>();
	let body = $state<HTMLDivElement>();

	const open = $derived(ui.settings !== null);
	// Wide: a pane is always open. A phone: '' is the list.
	const pane = $derived<Pane | ''>(
		ui.settings === null ? '' : ui.settings === '' && viewport.wide ? 'general' : ui.settings
	);
	const title = $derived(PANES.find(([id]) => id === pane)?.[1] ?? 'Settings');
	const View = $derived(pane ? VIEW[pane].view : null);
	const showList = $derived(viewport.wide || pane === '');
	const showPane = $derived(pane !== '');

	$effect(() => {
		const d = el;
		if (!d) return;
		if (open && !d.open) d.showModal();
		else if (!open && d.open) d.close();
	});
	$effect(() => {
		void pane;
		body?.scrollTo({ top: 0 });
	});

	function navKey(e: KeyboardEvent) {
		const tabs = [...(nav?.querySelectorAll<HTMLButtonElement>('[role=tab]') ?? [])];
		const at = tabs.indexOf(document.activeElement as HTMLButtonElement);
		let to = -1;
		if (e.key === 'ArrowDown') to = (at + 1) % tabs.length;
		else if (e.key === 'ArrowUp') to = (at - 1 + tabs.length) % tabs.length;
		else if (e.key === 'Home') to = 0;
		else if (e.key === 'End') to = tabs.length - 1;
		else return;
		e.preventDefault();
		tabs[to]?.focus();
		// With the pane beside the list, moving through the list shows each pane, as tabs do.
		if (viewport.wide) ui.settings = PANES[to][0];
	}
	async function back() {
		const from = pane;
		ui.settings = '';
		await tick();
		nav?.querySelector<HTMLButtonElement>(`#set-tab-${from}`)?.focus();
	}
</script>

<dialog
	bind:this={el}
	class="settings text-foreground"
	aria-labelledby="settings-title"
	data-settings
	onclose={() => (ui.settings = null)}
	onclick={(e) => {
		if (e.target === el) ui.settings = null;
	}}
>
	{#if open}
		<div class="flex h-full min-h-0">
			{#if showList}
				<div class="list bg-rail flex min-h-0 shrink-0 flex-col md:w-56 md:border-r">
					<div class="flex h-12 shrink-0 items-center justify-between pr-2 pl-4">
						<h2 id={viewport.wide ? undefined : 'settings-title'} class="text-[15px] font-semibold">
							Settings
						</h2>
						{#if !viewport.wide}
							<button
								type="button"
								class="icon-btn"
								aria-label="Close settings"
								onclick={() => (ui.settings = null)}><X class="size-4" /></button
							>
						{/if}
					</div>
					<!-- The tabs take the focus (a roving tabindex), not the list that holds them. -->
					<!-- svelte-ignore a11y_interactive_supports_focus -->
					<div
						bind:this={nav}
						class="scroll-thin min-h-0 flex-1 overflow-y-auto px-2 pb-2"
						role="tablist"
						aria-orientation="vertical"
						aria-label="Settings"
						onkeydown={navKey}
					>
						{#each PANES as [id, label] (id)}
							{@const Icon = VIEW[id].icon}
							<button
								type="button"
								role="tab"
								id="set-tab-{id}"
								class="side-row {pane === id ? 'is-open' : ''}"
								aria-selected={pane === id}
								aria-controls={pane === id ? 'set-panel' : undefined}
								tabindex={pane === id || (pane === '' && id === 'general') ? 0 : -1}
								onclick={() => (ui.settings = id)}
							>
								<Icon class="size-4 shrink-0" />
								<span class="min-w-0 flex-1 truncate">{label}</span>
								{#if !viewport.wide}<ChevronRight class="text-faint size-4 shrink-0" />{/if}
							</button>
						{/each}
					</div>
				</div>
			{/if}
			{#if showPane && View}
				<div class="flex min-h-0 min-w-0 flex-1 flex-col">
					<div class="flex h-12 shrink-0 items-center gap-1 pr-2 pl-2 md:pl-6">
						{#if !viewport.wide}
							<button type="button" class="icon-btn" aria-label="All settings panes" onclick={back}
								><ChevronLeft class="size-[18px]" /></button
							>
						{/if}
						<h2
							id="settings-title"
							class="min-w-0 flex-1 truncate pl-1 text-[15px] font-semibold md:pl-0"
						>
							{title}
						</h2>
						<button
							type="button"
							class="icon-btn"
							aria-label="Close settings"
							onclick={() => (ui.settings = null)}><X class="size-4" /></button
						>
					</div>
					<div
						bind:this={body}
						id="set-panel"
						role="tabpanel"
						aria-labelledby={viewport.wide ? `set-tab-${pane}` : 'settings-title'}
						class="scroll-thin min-h-0 flex-1 overflow-y-auto px-4 pb-6 md:px-6"
						data-pane={pane}
					>
						{#key pane}<View />{/key}
					</div>
				</div>
			{/if}
		</div>
	{/if}
</dialog>

<style>
	.settings {
		width: 100%;
		max-width: none;
		height: 100dvh;
		max-height: none;
		margin: 0;
		padding: 0;
		border: 0;
		background: var(--background);
		overflow: hidden;
	}
	.settings .list {
		width: 100%;
	}
	.settings::backdrop {
		background: var(--scrim);
	}
	@media (min-width: 768px) {
		.settings {
			width: min(56rem, calc(100% - 3rem));
			height: min(42rem, calc(100dvh - 4rem));
			margin: auto;
			border: 1px solid var(--border-strong);
			border-radius: 20px;
			box-shadow: var(--elev-3);
		}
		.settings .list {
			width: 14rem;
		}
		.settings[open] {
			animation: pop var(--dur) var(--ease-out) both;
		}
		.settings[open]::backdrop {
			animation: fade-in var(--dur) var(--ease-out) both;
		}
	}
</style>
